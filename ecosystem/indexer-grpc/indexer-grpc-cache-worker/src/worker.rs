// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_indexer_grpc_utils::{
    cache_operator::CacheOperator,
    config::IndexerGrpcConfig,
    constants::BLOB_STORAGE_SIZE,
    create_grpc_client,
    file_store_operator::{FileStoreMetadata, FileStoreOperator},
};
use aptos_logger::{error, info};
use aptos_moving_average::MovingAverage;
use aptos_protos::datastream::v1::{
    self as datastream, OnChainDataSummaryRequest, RawDatastreamRequest, RawDatastreamResponse,
};
use futures::{self, StreamExt};
pub struct Worker {
    redis_client: redis::Client,
    grpc_address: String,
    pub chain_id: Option<u32>,
    pub file_store_bucket_name: String,
}

pub(crate) enum GrpcDataStatus {
    // Ok status with processed count.
    DataOk(u64),
    // Init signal with current version.
    Init(u64),
    // End signal with batch end version(inclusive).
    End(u64),
}

impl Worker {
    pub async fn new(config: IndexerGrpcConfig) -> Self {
        let redis_client = redis::Client::open(format!("redis://{}", config.redis_address))
            .expect("Create redis client failed.");
        Self {
            redis_client,
            grpc_address: format!("http://{}", config.fullnode_grpc_address.unwrap()),
            chain_id: None,
            file_store_bucket_name: config.file_store_bucket_name,
        }
    }

    pub async fn run(&mut self) {
        // Re-connect if lost.
        loop {
            let conn = self
                .redis_client
                .get_tokio_connection()
                .await
                .expect("Get redis connection failed.");
            let mut rpc_client = create_grpc_client(self.grpc_address.clone()).await;
            let chain_id = rpc_client
                .on_chain_data_summary(OnChainDataSummaryRequest::default())
                .await
                .unwrap()
                .into_inner()
                .chain_id;

            let mut cache_operator = CacheOperator::new(conn);
            cache_operator.cache_setup_if_needed().await;

            // Crash the worker if cache chain id doesn't match.
            cache_operator
                .update_or_verify_chain_id(chain_id as u64)
                .await
                .expect("[Cache Worker] Chain id doesn't match.");
            self.chain_id = Some(chain_id);

            let mut file_store_operator =
                FileStoreOperator::new(chain_id as u64, self.file_store_bucket_name.clone(), true);
            let file_store_metadata = match file_store_operator.get_file_store_metadata().await {
                Ok(metadata) => metadata,
                Err(e) => {
                    error!("[Indexer Cache] Get file store metadata failed: {}", e);
                    tokio::spawn(async move {
                        // Wait for 1 minute and then exit.
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        std::process::exit(60);
                    });
                    FileStoreMetadata {
                        version: 0,
                        chain_id: chain_id as u64,
                        file_folder_size: BLOB_STORAGE_SIZE,
                    }
                },
            };
            // Always start from file store version.
            let current_version = file_store_metadata.version;

            let request = tonic::Request::new(RawDatastreamRequest {
                starting_version: current_version,
                // Worker fetch all transactions without end.
                ..Default::default()
            });
            let response = rpc_client.raw_datastream(request).await.unwrap();

            // Infinite streaming until error happens. Either function ends or worker crashes.
            process_streaming_response(
                chain_id,
                current_version,
                response.into_inner(),
                &mut cache_operator,
            )
            .await;
        }
    }
}

// Infinite streaming processing. Retry if error happens; crash if fatal.
async fn process_streaming_response(
    chain_id: u32,
    starting_version: u64,
    mut resp_stream: impl futures_core::Stream<Item = Result<RawDatastreamResponse, tonic::Status>>
        + std::marker::Unpin,
    cache_operator: &mut CacheOperator<redis::aio::Connection>,
) {
    let mut current_version = starting_version;
    let mut init_signal_received = false;
    let mut ma = MovingAverage::new(10_000);
    while let Some(received) = resp_stream.next().await {
        let received: RawDatastreamResponse = match received {
            Ok(r) => r,
            Err(err) => {
                error!("[Indexer Cache] Streaming error: {}", err);
                break;
            },
        };
        if chain_id != received.chain_id {
            panic!("Worker is running with wrong chain id.");
        }

        match process_raw_datastream_response(received, cache_operator).await {
            Ok(status) => match status {
                GrpcDataStatus::DataOk(processed_count) => {
                    current_version += processed_count;
                    ma.tick_now(processed_count);
                },
                GrpcDataStatus::Init(new_version) => {
                    if init_signal_received || new_version != current_version {
                        error!("[Indexer Cache] Init signal received twice.");
                        break;
                    }
                    init_signal_received = true;
                    current_version = new_version;
                },
                GrpcDataStatus::End(end_version) => {
                    if current_version != end_version + 1 {
                        error!("[Indexer Cache] End signal received with wrong version.");
                        break;
                    }
                    cache_operator
                        .update_cache_latest_version(current_version)
                        .await
                        .unwrap();
                    info!(
                        current_version = current_version,
                        chain_id = chain_id,
                        tps = (ma.avg() * 1000.0) as u64,
                        "[Indexer Cache] Successfully process current batch."
                    );
                },
            },
            Err(e) => {
                error!(
                    "[Indexer Cache] Process raw datastream response failed: {}",
                    e
                );
                break;
            },
        }
    }
}

async fn process_raw_datastream_response(
    response: RawDatastreamResponse,
    cache_operator: &mut CacheOperator<redis::aio::Connection>,
) -> anyhow::Result<GrpcDataStatus> {
    match response.response.unwrap() {
        datastream::raw_datastream_response::Response::Status(status) => match status.r#type {
            0 => Ok(GrpcDataStatus::Init(status.start_version)),
            1 => Ok(GrpcDataStatus::End(
                status.end_version.expect("Start version exists."),
            )),
            _ => {
                anyhow::bail!("Unknown status type: {}", status.r#type);
            },
        },
        datastream::raw_datastream_response::Response::Data(data) => {
            let transaction_len = data.transactions.len();
            for e in data.transactions {
                let version = e.version;
                let timestamp_in_seconds = match e.timestamp {
                    Some(t) => t.seconds,
                    // For Genesis block, there is no timestamp.
                    None => 0,
                };
                // Push to cache.
                match cache_operator
                    .update_cache_transaction(
                        version,
                        e.encoded_proto_data,
                        timestamp_in_seconds as u64,
                    )
                    .await
                {
                    Ok(_) => {},
                    Err(e) => {
                        anyhow::bail!("Update cache with version failed: {}", e);
                    },
                }
            }
            Ok(GrpcDataStatus::DataOk(transaction_len as u64))
        },
    }
}
