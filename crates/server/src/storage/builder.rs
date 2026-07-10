use std::sync::Arc;

use anyhow::{Context, bail};
use object_store::ObjectStore;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::signer::Signer;

use crate::config::StorageConfig;

use super::object_store_backend::ObjectStoreBackend;

/// Build an [`ObjectStoreBackend`] from the given configuration.
///
/// Every backend — local disk, S3 (and S3-compatible stores such as RustFS),
/// GCS, and Azure Blob — is routed through the `object_store` crate so the
/// server talks to a single unified [`object_store::ObjectStore`] interface.
pub async fn build_storage(config: &StorageConfig) -> anyhow::Result<ObjectStoreBackend> {
    // Cloud backends can mint presigned URLs (the `Signer` handle); the local
    // filesystem cannot, so its signer is `None` and downloads fall back to the
    // server's own xorb route.
    let (store, signer): (Arc<dyn ObjectStore>, Option<Arc<dyn Signer>>) =
        match config.backend.as_str() {
            "filesystem" => {
                // `LocalFileSystem` requires its root prefix to exist; also create
                // the xorb/shard subtrees so listing an empty store never errors.
                let data_dir = &config.data_dir;
                tokio::fs::create_dir_all(data_dir.join("xorbs").join("default"))
                    .await
                    .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;
                tokio::fs::create_dir_all(data_dir.join("shards"))
                    .await
                    .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;

                let store = LocalFileSystem::new_with_prefix(data_dir)
                    .context("failed to initialize filesystem storage")?;
                (Arc::new(store) as Arc<dyn ObjectStore>, None)
            }
            "s3" => {
                let bucket = config
                    .s3_bucket
                    .as_deref()
                    .context("s3_bucket is required for S3 backend")?;

                // Build an S3 client for a given endpoint. All settings are
                // shared except the endpoint, so the same closure builds both
                // the data client (internal endpoint) and, when configured, a
                // separate signing client (public endpoint).
                let make_client = |endpoint: Option<&str>| -> anyhow::Result<AmazonS3> {
                    let mut builder = AmazonS3Builder::new().with_bucket_name(bucket);
                    if let Some(region) = &config.s3_region {
                        builder = builder.with_region(region);
                    }
                    if let Some(endpoint) = endpoint {
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
                    builder.build().context("failed to build S3 client")
                };

                let s3 = Arc::new(make_client(config.s3_endpoint.as_deref())?);

                // Presigned URLs are signed against the client's endpoint host.
                // When the server reaches storage over an internal address that
                // external clients can't resolve, sign with the public endpoint
                // instead so handed-out URLs point at a reachable host. Presigning
                // does no network I/O, so this client is only ever used to sign.
                let signer: Arc<dyn Signer> = match &config.s3_public_endpoint {
                    Some(public) => Arc::new(make_client(Some(public))?),
                    None => s3.clone(),
                };

                (s3 as Arc<dyn ObjectStore>, Some(signer))
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

                let gcs = Arc::new(builder.build().context("failed to build GCS client")?);
                (
                    gcs.clone() as Arc<dyn ObjectStore>,
                    Some(gcs as Arc<dyn Signer>),
                )
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

                let azure = Arc::new(builder.build().context("failed to build Azure client")?);
                (
                    azure.clone() as Arc<dyn ObjectStore>,
                    Some(azure as Arc<dyn Signer>),
                )
            }
            other => bail!("unknown storage backend: {other}"),
        };

    Ok(ObjectStoreBackend::new(store, signer))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::StorageConfig;
    use crate::storage::backend::StorageBackend;

    const TEST_HASH: &str = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";

    fn s3_config() -> StorageConfig {
        StorageConfig {
            backend: "s3".to_string(),
            s3_bucket: Some("openxet".to_string()),
            s3_region: Some("us-east-1".to_string()),
            s3_endpoint: Some("http://rustfs:9000".to_string()),
            s3_access_key_id: Some("rustfsadmin".to_string()),
            s3_secret_access_key: Some("rustfsadmin".to_string()),
            s3_allow_http: Some(true),
            ..StorageConfig::default()
        }
    }

    // Presigning does no network I/O, so these build a real S3 backend and
    // inspect the signed URL host without ever touching a live store.

    #[tokio::test]
    async fn presigned_url_uses_internal_endpoint_by_default() {
        let backend = build_storage(&s3_config()).await.unwrap();
        let url = backend
            .presigned_xorb_url(TEST_HASH, Duration::from_secs(60))
            .await
            .unwrap()
            .expect("s3 backend signs urls");
        assert!(url.contains("rustfs:9000"), "unexpected url: {url}");
    }

    #[tokio::test]
    async fn presigned_url_uses_public_endpoint_when_set() {
        let mut config = s3_config();
        config.s3_public_endpoint = Some("http://localhost:9000".to_string());

        let backend = build_storage(&config).await.unwrap();
        let url = backend
            .presigned_xorb_url(TEST_HASH, Duration::from_secs(60))
            .await
            .unwrap()
            .expect("s3 backend signs urls");
        assert!(url.contains("localhost:9000"), "unexpected url: {url}");
        assert!(!url.contains("rustfs:9000"), "leaked internal host: {url}");
    }
}
