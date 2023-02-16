// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_indexer_grpc_utils::{
    cache_operator::{CacheBatchGetStatus, CacheOperator},
    config::IndexerGrpcConfig,
    constants::BLOB_STORAGE_SIZE,
    file_store_operator::FileStoreOperator,
};
use aptos_moving_average::MovingAverage;
use std::{thread::sleep, time::Duration};

pub struct Processor {
    cache_operator: Option<CacheOperator<redis::aio::Connection>>,
    file_store_processor: Option<FileStoreOperator>,
    config: IndexerGrpcConfig,
}

impl Processor {
    pub fn new(config: IndexerGrpcConfig) -> Self {
        Self {
            cache_operator: None,
            file_store_processor: None,
            config,
        }
    }

    async fn bootstrap(&mut self) {
        let conn = redis::Client::open(format!("redis://{}", self.config.redis_address))
            .expect("Create redis client failed.")
            .get_async_connection()
            .await
            .expect("Create redis connection failed.");

        let mut cache_operator = CacheOperator::new(conn);
        let chain_id = cache_operator
            .get_chain_id()
            .await
            .expect("Get chain id failed.");
        let file_store_operator =
            FileStoreOperator::new(chain_id, self.config.file_store_bucket_name.clone(), false);
        self.cache_operator = Some(cache_operator);
        self.file_store_processor = Some(file_store_operator);
    }

    // Starts the processing.
    pub async fn run(&mut self) {
        self.bootstrap().await;
        let metadata = self
            .file_store_processor
            .as_mut()
            .unwrap()
            .get_file_store_metadata()
            .await
            .unwrap();
        let mut fetch_version = metadata.version;
        let mut push_version = fetch_version;
        let mut transactions: Vec<String> = vec![];
        let mut ma = MovingAverage::new(10_000);
        let mut hit_head = false;
        loop {
            let batch_get_result = self
                .cache_operator
                .as_mut()
                .unwrap()
                .batch_get_encoded_proto_data(fetch_version)
                .await;
            match batch_get_result {
                Ok(CacheBatchGetStatus::Ok(t)) => {
                    fetch_version += t.len() as u64;
                    transactions.extend(t);
                },
                Ok(CacheBatchGetStatus::NotReady) => {
                    sleep(Duration::from_secs(1));
                    aptos_logger::info!(
                        push_version = push_version,
                        fetch_version = fetch_version,
                        "Cache is not ready. Sleep for 1 second."
                    );
                    continue;
                },
                Ok(CacheBatchGetStatus::HitTheHead(t)) => {
                    fetch_version += t.len() as u64;
                    transactions.extend(t);
                    hit_head = true;
                    aptos_logger::info!(
                        push_version = push_version,
                        fetch_version = fetch_version,
                        "Follow the head."
                    );
                },
                Ok(CacheBatchGetStatus::EvictedFromCache) => {
                    panic!(
                        "Cache evicted from cache. For file store worker, this is not expected."
                    );
                },
                Err(err) => {
                    panic!("Batch get encoded proto data failed: {}", err);
                },
            }
            if !hit_head && transactions.len() < 10 * BLOB_STORAGE_SIZE {
                // If we haven't hit the head, we want to collect more transactions.
                continue;
            }
            if hit_head && transactions.len() < BLOB_STORAGE_SIZE {
                // If we have hit the head, we want to collect at least one batch of transactions.
                continue;
            }
            let mut batch_size = BLOB_STORAGE_SIZE;
            if !hit_head && transactions.len() >= 10 * BLOB_STORAGE_SIZE {
                batch_size = 10 * BLOB_STORAGE_SIZE;
            }
            let current_batch: Vec<String> = transactions.drain(..batch_size).collect();
            self.file_store_processor
                .as_mut()
                .unwrap()
                .upload_transactions(current_batch)
                .await
                .unwrap();
            ma.tick_now(batch_size as u64);
            aptos_logger::info!(
                tps = (ma.avg() * 1000.0) as u64,
                version = push_version,
                "Upload transactions to file store."
            );
            push_version += batch_size as u64;
        }
    }
}
