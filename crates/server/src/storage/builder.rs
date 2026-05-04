use std::sync::Arc;

use anyhow::{Context, bail};
use object_store::aws::AmazonS3Builder;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;

use crate::config::StorageConfig;

use super::dispatch::StorageDispatch;
use super::filesystem::FilesystemBackend;
use super::object_store_backend::ObjectStoreBackend;

/// Build a [`StorageDispatch`] from the given configuration.
pub async fn build_storage(config: &StorageConfig) -> anyhow::Result<StorageDispatch> {
    match config.backend.as_str() {
        "filesystem" => {
            let backend = FilesystemBackend::new(&config.data_dir)
                .await
                .context("failed to initialize filesystem storage")?;
            Ok(StorageDispatch::Filesystem(backend))
        }
        "s3" => {
            let bucket = config
                .s3_bucket
                .as_deref()
                .context("s3_bucket is required for S3 backend")?;

            let mut builder = AmazonS3Builder::new().with_bucket_name(bucket);

            if let Some(region) = &config.s3_region {
                builder = builder.with_region(region);
            }
            if let Some(endpoint) = &config.s3_endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            if let Some(key_id) = &config.s3_access_key_id {
                builder = builder.with_access_key_id(key_id);
            }
            if let Some(secret) = &config.s3_secret_access_key {
                builder = builder.with_secret_access_key(secret);
            }
            if config.s3_allow_http == Some(true) {
                builder = builder.with_allow_http(true);
            }

            let store = builder.build().context("failed to build S3 client")?;
            Ok(StorageDispatch::ObjectStore(ObjectStoreBackend::new(
                Arc::new(store),
            )))
        }
        "gcs" => {
            let bucket = config
                .gcs_bucket
                .as_deref()
                .context("gcs_bucket is required for GCS backend")?;

            let mut builder = GoogleCloudStorageBuilder::new().with_bucket_name(bucket);

            if let Some(path) = &config.gcs_service_account_path {
                builder = builder.with_service_account_path(path);
            }

            let store = builder.build().context("failed to build GCS client")?;
            Ok(StorageDispatch::ObjectStore(ObjectStoreBackend::new(
                Arc::new(store),
            )))
        }
        "azure" => {
            let container = config
                .azure_container
                .as_deref()
                .context("azure_container is required for Azure backend")?;

            let mut builder = MicrosoftAzureBuilder::new().with_container_name(container);

            if let Some(account) = &config.azure_account {
                builder = builder.with_account(account);
            }
            if let Some(key) = &config.azure_access_key {
                builder = builder.with_access_key(key);
            }

            let store = builder.build().context("failed to build Azure client")?;
            Ok(StorageDispatch::ObjectStore(ObjectStoreBackend::new(
                Arc::new(store),
            )))
        }
        other => bail!("unknown storage backend: {other}"),
    }
}
