# Indexer GRPC cache worker

Cache worker fetches data from fullnode GRPC and push data to Cache. 

## How to run it.

* A yaml file for the worker.

```yaml
fullnode_grpc_address: 127.0.0.1:50051
redis_address: larry.macos.network:6379
file_store_bucket_name: indexer-grpc-file-store-testnet 
health_check_port: 8081
```


* Set the `WORKER_CONFIG_PATH` ENV varaible to your yaml fille, and run your cache worker at current folder,
    `cargo run --release -- --config-path=worker.yaml`