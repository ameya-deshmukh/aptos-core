# Indexer GRPC data service

Indexer GRPC data service fetches data from both cache and file store.

## How to run it.

* service account json with write access to bucket `{FILE_STORE_BUCKET_NAME}-{CHAIN_NAME}`, e.g., with name `xxx.json`.
  
* Command-line to run:

```bash
data_service_grpc_listen_address: 0.0.0.0:50052
redis_address: 127.0.0.1:6379
file_store_bucket_name: indexer-grpc-file-store-testnet 
health_check_port: 8081
```