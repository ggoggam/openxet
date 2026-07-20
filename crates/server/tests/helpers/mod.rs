use std::sync::Arc;

use bytes::Bytes;
use tokio::net::TcpListener;

use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use xet_core_structures::merklehash::{MerkleHash, file_hash};
use xet_core_structures::metadata_shard::MDBShardFileHeader;
use xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks;
use xet_core_structures::metadata_shard::file_structs::{
    FileDataSequenceEntry, FileDataSequenceHeader, FileVerificationEntry, MDBFileInfo,
};
use xet_core_structures::metadata_shard::xorb_structs::{MDBXorbInfo, XorbChunkSequenceHeader};
use xet_core_structures::xorb_object::constants::{MAX_XORB_BYTES, MAX_XORB_CHUNKS};
use xet_core_structures::xorb_object::{
    Chunk, CompressionScheme, RawXorbData, SerializedXorbObject, deserialize_chunks,
};
use xet_data::deduplication::Chunker;

use openxet_server::auth::{Claims, Scope, create_token};
use openxet_server::config::AppConfig;
use openxet_server::routes::build_router;
use openxet_server::state::AppState;
use openxet_server::storage::{build_index, build_storage};

const TEST_SECRET: &str = "test-secret";

/// A running test server instance with a temporary data directory.
pub struct TestServer {
    pub base_url: String,
    pub client: reqwest::Client,
    /// The server's S3 gateway index, so tests can seed credentials (there is
    /// no HTTP endpoint for that in Phase 1).
    pub s3_index: Arc<openxet_server::storage::S3IndexBackend>,
    _temp_dir: tempfile::TempDir,
}

impl TestServer {
    #[allow(dead_code)]
    pub async fn start() -> Self {
        Self::start_inner(true).await
    }

    #[allow(dead_code)]
    pub async fn start_with_auth_disabled() -> Self {
        Self::start_inner(false).await
    }

    /// Start a server whose indexes are backed by the Postgres instance at
    /// `postgres_url`. Used by the Postgres-gated integration test.
    #[allow(dead_code)]
    pub async fn start_with_postgres(postgres_url: &str) -> Self {
        Self::start_configured(true, "sqlite", Some(postgres_url.to_string())).await
    }

    /// Postgres-backed, auth disabled — for exercising the S3 read gateway
    /// (which bypasses SigV4 when auth is off) against the Postgres index.
    #[allow(dead_code)]
    pub async fn start_with_postgres_no_auth(postgres_url: &str) -> Self {
        Self::start_configured(false, "sqlite", Some(postgres_url.to_string())).await
    }

    async fn start_inner(auth_enabled: bool) -> Self {
        Self::start_configured(auth_enabled, "sqlite", None).await
    }

    async fn start_configured(
        auth_enabled: bool,
        local_index_backend: &str,
        postgres_url: Option<String>,
    ) -> Self {
        let index_backend = if postgres_url.is_some() {
            "postgres".to_string()
        } else {
            local_index_backend.to_string()
        };
        let temp_dir = tempfile::tempdir().unwrap();
        let data_dir = temp_dir.path().join("data");

        // Create stub frontend directory (required by ServeDir)
        let frontend_dir = temp_dir.path().join("web").join("dist");
        tokio::fs::create_dir_all(&frontend_dir).await.unwrap();
        tokio::fs::write(frontend_dir.join("index.html"), b"<html></html>")
            .await
            .unwrap();

        let config = AppConfig {
            server: openxet_server::config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0, // OS-assigned
                frontend_dir,
                public_url: None,
                // Exercise the S3 gateway in integration tests.
                s3_gateway_enabled: true,
                s3_gateway_prefix: "/s3".to_string(),
            },
            storage: openxet_server::config::StorageConfig {
                backend: "filesystem".to_string(),
                data_dir: data_dir.clone(),
                index_backend,
                postgres_url,
                ..Default::default()
            },
            auth: openxet_server::config::AuthConfig {
                enabled: auth_enabled,
                shard_key_ttl_seconds: 3600,
                ..Default::default()
            },
            gc: Default::default(),
        };

        let storage = Arc::new(build_storage(&config.storage).await.unwrap());
        let (file_index, chunk_index, s3_index) = build_index(&config.storage).await.unwrap();
        let file_index = Arc::new(file_index);
        let chunk_index = Arc::new(chunk_index);
        let s3_index = Arc::new(s3_index);
        let s3_index_handle = s3_index.clone();

        let jwks = Arc::new(openxet_server::auth::JwksCache::new(
            config.auth.oidc_issuers.clone(),
            std::time::Duration::from_secs(config.auth.jwks_ttl_seconds),
        ));

        let state = AppState {
            storage,
            file_index,
            chunk_index,
            s3_index,
            config: Arc::new(config),
            jwks,
            // Tests mint HS256 tokens against this known secret, exercising
            // the same verify path the server's self-minted fetch tokens use.
            fetch_token_secret: TEST_SECRET.into(),
        };

        let app = build_router(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        TestServer {
            base_url,
            client,
            s3_index: s3_index_handle,
            _temp_dir: temp_dir,
        }
    }

    /// Seed a SigV4 credential directly into the gateway index (no HTTP endpoint
    /// exists for this in Phase 1).
    #[allow(dead_code)]
    pub async fn seed_s3_credential(&self, access_key_id: &str, secret_key: &str, owner: &str) {
        self.s3_index
            .put_credential(
                &openxet_server::storage::S3Credential {
                    access_key_id: access_key_id.to_string(),
                    secret_key: secret_key.to_string(),
                    owner_id: owner.to_string(),
                },
                0,
            )
            .await
            .unwrap();
    }

    pub fn read_token(&self) -> String {
        let claims = Claims {
            scope: Scope::Read,
            repo: "test".to_string(),
            exp: (chrono_exp()),
            sub: "test-user".to_string(),
        };
        create_token(TEST_SECRET, &claims).unwrap()
    }

    pub fn write_token(&self) -> String {
        self.write_token_for("test-user")
    }

    /// Mint a write token for a specific subject, for exercising per-owner
    /// accounting and deletion.
    #[allow(dead_code)]
    pub fn write_token_for(&self, sub: &str) -> String {
        let claims = Claims {
            scope: Scope::Write,
            repo: "test".to_string(),
            exp: (chrono_exp()),
            sub: sub.to_string(),
        };
        create_token(TEST_SECRET, &claims).unwrap()
    }
}

/// Returns an expiry timestamp 1 hour in the future.
fn chrono_exp() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize;
    now + 3600
}

/// Artifacts produced by building xorbs and shard from raw file data.
#[allow(dead_code)]
pub struct UploadArtifacts {
    pub file_hash: String,
    pub xorb_entries: Vec<(String, Bytes)>,
    pub shard_bytes: Bytes,
    pub chunk_hashes: Vec<MerkleHash>,
}

/// Replicate the client-side upload pipeline (chunk → pack xorbs → build
/// shard) with the same xet-core-structures APIs real clients use, producing
/// CAS artifacts that can be uploaded via the protocol endpoints.
pub fn build_upload_artifacts(file_data: &[u8]) -> UploadArtifacts {
    let chunks: Vec<Chunk> = Chunker::default().next_block(file_data, true);
    let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|c| c.hash).collect();

    let chunk_hashes_and_sizes: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|c| (c.hash, c.data.len() as u64))
        .collect();

    let file_hash_hex = file_hash(&chunk_hashes_and_sizes).hex();

    // Split chunks into xorb groups at xet-core's raw-byte / chunk-count cuts
    let mut groups: Vec<(usize, usize)> = Vec::new(); // [start, end) chunk indices
    let mut group_start = 0usize;
    let mut group_bytes = 0usize;
    for (i, chunk) in chunks.iter().enumerate() {
        let len = chunk.data.len();
        if i > group_start
            && (group_bytes + len > *MAX_XORB_BYTES || i - group_start >= *MAX_XORB_CHUNKS)
        {
            groups.push((group_start, i));
            group_start = i;
            group_bytes = 0;
        }
        group_bytes += len;
    }
    if group_start < chunks.len() {
        groups.push((group_start, chunks.len()));
    }

    // Build xorbs + shard metadata
    let mut file_data_entries = Vec::new();
    let mut verification_entries = Vec::new();
    let mut xorb_infos: Vec<MDBXorbInfo> = Vec::new();
    let mut xorb_entries = Vec::new();

    for &(start, end) in &groups {
        let group_chunks = &chunks[start..end];
        let raw = RawXorbData::from_chunks(group_chunks, vec![0]);
        let xorb_hash = raw.hash();
        let mut xorb_info = raw.xorb_info.clone();

        let serialized =
            SerializedXorbObject::from_xorb_with_compression(raw, CompressionScheme::LZ4, true)
                .unwrap();
        xorb_info.metadata.num_bytes_on_disk = serialized.serialized_data.len() as u32;

        xorb_entries.push((xorb_hash.hex(), Bytes::from(serialized.serialized_data)));

        let group_unpacked: u32 = group_chunks.iter().map(|c| c.data.len() as u32).sum();

        file_data_entries.push(FileDataSequenceEntry::new(
            xorb_hash,
            group_unpacked,
            0u32,
            (end - start) as u32,
        ));
        verification_entries.push(FileVerificationEntry::new(range_hash_from_chunks(
            &chunk_hashes[start..end],
        )));
        xorb_infos.push(xorb_info);
    }

    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(
            MerkleHash::from_hex(&file_hash_hex).unwrap(),
            file_data_entries.len(),
            true,
            false,
        ),
        segments: file_data_entries,
        verification: verification_entries,
        metadata_ext: None,
    };

    // Upload shard format: header with footer_size=0, file info section,
    // bookend, xorb info section, bookend.
    let mut shard_bytes = Vec::new();
    let header = MDBShardFileHeader {
        footer_size: 0,
        ..Default::default()
    };
    header.serialize(&mut shard_bytes).unwrap();
    file_info.serialize(&mut shard_bytes).unwrap();
    FileDataSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .unwrap();
    for xorb_info in &xorb_infos {
        xorb_info.serialize(&mut shard_bytes).unwrap();
    }
    XorbChunkSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .unwrap();

    UploadArtifacts {
        file_hash: file_hash_hex,
        xorb_entries,
        shard_bytes: Bytes::from(shard_bytes),
        chunk_hashes,
    }
}

/// Generate pseudo-random deterministic data of the given size.
pub fn generate_test_data(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    // Simple LCG for deterministic pseudo-random data
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for chunk in data.chunks_mut(8) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bytes = state.to_le_bytes();
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = bytes[i % 8];
        }
    }
    data
}

/// Upload all xorbs and the shard via the CAS protocol endpoints.
pub async fn upload_artifacts(server: &TestServer, artifacts: &UploadArtifacts) {
    let token = server.write_token();
    upload_artifacts_with_token(server, artifacts, &token).await;
}

/// Like [`upload_artifacts`] but with a caller-supplied bearer token, so tests
/// can upload as different owners.
#[allow(dead_code)]
pub async fn upload_artifacts_with_token(
    server: &TestServer,
    artifacts: &UploadArtifacts,
    token: &str,
) {
    // Upload xorbs
    for (hash, data) in &artifacts.xorb_entries {
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(token)
            .body(data.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "xorb upload failed for {hash}");
    }

    // Upload shard
    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "shard upload failed");
}

/// Download a file via the CAS protocol: query the reconstruction, fetch each
/// term's byte range from its fetch_info URL, decompress, and concatenate.
#[allow(dead_code)]
pub async fn download_via_protocol(server: &TestServer, file_hash: &str) -> Vec<u8> {
    let token = server.read_token();

    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{file_hash}",
            server.base_url
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "reconstruction query failed");
    let recon: QueryReconstructionResponse = resp.json().await.unwrap();

    let mut file_bytes = Vec::new();
    for term in &recon.terms {
        let fetch = recon
            .fetch_info
            .get(&term.hash)
            .and_then(|v| v.iter().find(|f| f.range.contains_range(&term.range)))
            .unwrap_or_else(|| panic!("no fetch_info for xorb {}", term.hash));

        // xet-core fetches fetch_info URLs without any Authorization header
        // (they are presigned / self-authenticating), so we deliberately don't
        // attach one here.
        let resp = server
            .client
            .get(&fetch.url)
            .header(
                "range",
                format!("bytes={}-{}", fetch.url_range.start, fetch.url_range.end),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "xorb range fetch failed");
        let part = resp.bytes().await.unwrap();

        // The ranged fetch returns raw chunk frames (no footer), exactly what
        // xet-core feeds deserialize_chunks.
        let (bytes, _boundaries) =
            deserialize_chunks(&mut std::io::Cursor::new(&part[..])).unwrap();
        file_bytes.extend_from_slice(&bytes);
    }

    file_bytes[recon.offset_into_first_range as usize..].to_vec()
}
