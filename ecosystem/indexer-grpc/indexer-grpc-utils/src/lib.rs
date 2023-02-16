// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

pub mod cache_operator;
pub mod config;
pub mod constants;
pub mod file_store_operator;

use aptos_protos::datastream::v1::indexer_stream_client::IndexerStreamClient;

pub type GrpcClientType = IndexerStreamClient<tonic::transport::Channel>;

/// Create a gRPC client with exponential backoff.
pub async fn create_grpc_client(address: String) -> GrpcClientType {
    backoff::future::retry(backoff::ExponentialBackoff::default(), || async {
        Ok(IndexerStreamClient::connect(address.clone()).await?)
    })
    .await
    .unwrap()
}
