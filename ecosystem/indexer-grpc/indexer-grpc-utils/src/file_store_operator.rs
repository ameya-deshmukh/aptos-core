// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::constants::BLOB_STORAGE_SIZE;
use cloud_storage::{Bucket, Object};
use itertools::{any, Itertools};
use serde::{Deserialize, Serialize};

const FILE_FOLDER_NAME: &str = "files";
const METADATA_FILE_NAME: &str = "metadata.json";
const JSON_FILE_TYPE: &str = "application/json";

#[inline]
pub fn generate_blob_name(starting_version: u64) -> String {
    format!("{}/{}.json", FILE_FOLDER_NAME, starting_version)
}

#[derive(Serialize, Deserialize)]
pub struct TransactionsFile {
    pub starting_version: u64,
    /// The transactions in the blob.
    pub transactions: Vec<String>,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub struct FileStoreMetadata {
    pub chain_id: u64,
    pub file_folder_size: usize,
    pub version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    // Read-only mode; stateless.
    Read,
    // Write mode; stateful. Requires bootstrap from latest version from metadata and write to file store without gap.
    Write(u64),
}

pub struct FileStoreOperator {
    chain_id: u64,
    bucket_name: String,
    access_mode: AccessMode,
    current_version: u64,
    latest_metadata_update_timestamp: std::time::Instant,
}

impl FileStoreOperator {
    pub fn new(cache_chain_id: u64, bucket_name: String, is_reading_only: bool) -> Self {
        let access_mode = if is_reading_only {
            AccessMode::Read
        } else {
            AccessMode::Write(0)
        };
        let current_version = 0;
        Self {
            chain_id: cache_chain_id,
            bucket_name,
            access_mode,
            current_version,
            latest_metadata_update_timestamp: std::time::Instant::now(),
        }
    }

    pub async fn bootstrap(&mut self) {
        // Verifies the bucket exists.
        Bucket::read(&self.bucket_name)
            .await
            .expect("Failed to read bucket.");

        match self.get_file_store_metadata().await {
            Ok(metadata) => {
                if metadata.chain_id != self.chain_id {
                    panic!(
                        "Chain ID mismatch. Cache: {}, File Store: {}",
                        self.chain_id, metadata.chain_id
                    );
                }
                self.current_version = metadata.version;
            },
            Err(err) => {
                aptos_logger::error!("Failed to get file store metadata: {:?}", err);
                // Fix this with metadata creation.
                panic!("Failed to get file store metadata: {:?}", err);
            },
        }
    }

    pub async fn get_transactions_file(
        &mut self,
        version: u64,
    ) -> anyhow::Result<TransactionsFile> {
        let current_file_name = generate_blob_name(version);
        match Object::download(&self.bucket_name, current_file_name.as_str()).await {
            Ok(file) => {
                let file: TransactionsFile =
                    serde_json::from_slice(&file).expect("Expected file to be valid JSON.");
                Ok(file)
            },
            Err(err) => Err(anyhow::Error::from(err)),
        }
    }

    // If the file store is empty, the metadata will be created in write mode.
    pub async fn get_file_store_metadata(&mut self) -> anyhow::Result<FileStoreMetadata> {
        match Object::download(&self.bucket_name, METADATA_FILE_NAME).await {
            Ok(metadata) => {
                let metadata: FileStoreMetadata =
                    serde_json::from_slice(&metadata).expect("Expected metadata to be valid JSON.");
                match metadata.chain_id == self.chain_id {
                    true => Ok(metadata),
                    false => Err(anyhow::Error::msg(format!(
                        "Chain ID mismatch. Cache: {}, File Store: {}",
                        self.chain_id, metadata.chain_id
                    ))),
                }
            },
            Err(cloud_storage::Error::Other(err)) => {
                let is_file_missing = err.contains("No such object: ");
                if self.access_mode != AccessMode::Read && is_file_missing {
                    // If the metadata is not found, it means the file store is empty.
                    self.update_file_store_metadata(0).await?;
                    Ok(FileStoreMetadata {
                        chain_id: self.chain_id,
                        file_folder_size: BLOB_STORAGE_SIZE,
                        version: 0,
                    })
                } else {
                    // If not in write mode, the metadata must exist.
                    Err(anyhow::Error::msg(
                        "Metadata not found and file store operator is not in write mode.",
                    ))
                }
            },
            Err(err) => Err(anyhow::Error::from(err)),
        }
    }

    async fn update_file_store_metadata(&mut self, version: u64) -> anyhow::Result<()> {
        if let AccessMode::Read = self.access_mode {
            anyhow::bail!("File store is in read-only mode.");
        }

        if (std::time::Instant::now() - self.latest_metadata_update_timestamp).as_secs() < 5 {
            return Ok(());
        }

        let metadata = FileStoreMetadata {
            chain_id: self.chain_id,
            file_folder_size: BLOB_STORAGE_SIZE,
            version,
        };
        // If the metadata is not updated, the indexer will be restarted.
        match Object::create(
            self.bucket_name.as_str(),
            serde_json::to_vec(&metadata).unwrap(),
            METADATA_FILE_NAME,
            JSON_FILE_TYPE,
        )
        .await
        {
            Ok(_) => {
                self.latest_metadata_update_timestamp = std::time::Instant::now();
                Ok(())
            },
            Err(err) => Err(anyhow::Error::from(err)),
        }
    }

    pub async fn upload_transactions(&mut self, transactions: Vec<String>) -> anyhow::Result<()> {
        if transactions.len() % BLOB_STORAGE_SIZE != 0 {
            // It's fatal if the transactions length is not a multiple of BLOB_STORAGE_SIZE; corrupted data will be uploaded.
            panic!("Transactions length is not a multiple of BLOB_STORAGE_SIZE.");
        }
        let mut tasks = vec![];
        let mut current_version = self.current_version;

        for i in transactions.chunks(BLOB_STORAGE_SIZE) {
            let bucket_name = self.bucket_name.clone();
            let current_batch = i.iter().cloned().collect_vec();
            let task = tokio::spawn(async move {
                let batch_version = current_version;
                match Object::create(
                    bucket_name.clone().as_str(),
                    serde_json::to_vec(&TransactionsFile {
                        starting_version: batch_version,
                        transactions: current_batch,
                    })
                    .unwrap(),
                    generate_blob_name(batch_version).as_str(),
                    JSON_FILE_TYPE,
                )
                .await
                {
                    Ok(_) => Ok(()),
                    Err(err) => Err(anyhow::Error::from(err)),
                }
            });
            current_version += BLOB_STORAGE_SIZE as u64;
            tasks.push(task);
        }
        let results = match futures::future::try_join_all(tasks).await {
            Ok(res) => res,
            Err(err) => panic!("Error processing transaction batches: {:?}", err),
        };
        // If any uploading fails, retry.
        if any(results, |x| x.is_err()) {
            anyhow::bail!("Uploading transactions failed.");
        }

        match self.update_file_store_metadata(current_version).await {
            Ok(_) => {
                self.current_version = current_version;
                Ok(())
            },
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn verify_blob_naming() {
        assert_eq!(super::generate_blob_name(0), "files/0.json");
        assert_eq!(
            super::generate_blob_name(100_000_000),
            "files/100000000.json"
        );
        assert_eq!(
            super::generate_blob_name(1_000_000_000),
            "files/1000000000.json"
        );
        assert_eq!(
            super::generate_blob_name(10_000_000_000),
            "files/10000000000.json"
        );
        assert_eq!(
            super::generate_blob_name(u64::MAX),
            "files/18446744073709551615.json"
        );
    }
}
