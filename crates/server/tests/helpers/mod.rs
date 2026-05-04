use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use openxet_cas_types::chunk::CompressionType;
use openxet_cas_types::shard::{
    CASChunkSequenceEntry, CASChunkSequenceHeader, CASInfoBlock, FileDataSequenceEntry,
    FileDataSequenceHeader, FileInfoBlock, FileVerificationEntry, MDB_FILE_FLAG_WITH_VERIFICATION,
    Shard, ShardHeader,
};
use openxet_cas_types::xorb::{XORB_SOFT_LIMIT, compute_xorb_hash, serialize_single_chunk};
use openxet_chunking::chunk_data;
use openxet_hashing::{
    MerkleHash, compute_chunk_hash, compute_file_hash, compute_verification_hash,
};

use openxet_server::auth::{Claims, Scope, create_token};
use openxet_server::config::AppConfig;
use openxet_server::routes::build_router;
use openxet_server::state::AppState;
use openxet_server::storage::{
    FilesystemBackend, FilesystemChunkIndex, FilesystemFileIndex, StorageDispatch,
};

const TEST_SECRET: &str = "test-secret";

/// A running test server instance with a temporary data directory.
pub struct TestServer {
    pub base_url: String,
    pub client: reqwest::Client,
    _temp_dir: tempfile::TempDir,
}

impl TestServer {
    pub async fn start() -> Self {
        let temp_dir = tempfile::tempdir().unwrap();
        let data_dir = temp_dir.path().join("data");

        // Create stub frontend directory (required by ServeDir)
        let frontend_dir = temp_dir.path().join("web").join("dist");
        tokio::fs::create_dir_all(&frontend_dir).await.unwrap();
        tokio::fs::write(frontend_dir.join("index.html"), b"<html></html>")
            .await
            .unwrap();

        // Create uploads temp directory
        let uploads_dir = data_dir.join("uploads").join("tmp");
        tokio::fs::create_dir_all(&uploads_dir).await.unwrap();

        let config = AppConfig {
            server: openxet_server::config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0, // OS-assigned
                frontend_dir,
            },
            storage: openxet_server::config::StorageConfig {
                backend: "filesystem".to_string(),
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            auth: openxet_server::config::AuthConfig {
                secret: TEST_SECRET.to_string(),
                shard_key_ttl_seconds: 3600,
            },
        };

        let storage = Arc::new(StorageDispatch::Filesystem(
            FilesystemBackend::new(&data_dir).await.unwrap(),
        ));
        let file_index = Arc::new(FilesystemFileIndex::new(&data_dir).await.unwrap());
        let chunk_index = Arc::new(FilesystemChunkIndex::new(&data_dir).await.unwrap());
        let upload_sessions = Arc::new(Mutex::new(HashMap::new()));

        let state = AppState {
            storage,
            file_index,
            chunk_index,
            config: Arc::new(config),
            upload_sessions,
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
            _temp_dir: temp_dir,
        }
    }

    pub fn read_token(&self) -> String {
        let claims = Claims {
            scope: Scope::Read,
            repo: "test".to_string(),
            exp: (chrono_exp()),
        };
        create_token(TEST_SECRET, &claims).unwrap()
    }

    pub fn write_token(&self) -> String {
        let claims = Claims {
            scope: Scope::Write,
            repo: "test".to_string(),
            exp: (chrono_exp()),
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

/// Replicate the server's `process_file_upload` logic to build CAS artifacts
/// that can be uploaded via the protocol endpoints.
pub fn build_upload_artifacts(file_data: &[u8]) -> UploadArtifacts {
    let chunk_infos = chunk_data(file_data);

    let chunk_slices: Vec<&[u8]> = chunk_infos
        .iter()
        .map(|ci| &file_data[ci.offset..ci.offset + ci.length])
        .collect();

    let chunk_hashes: Vec<MerkleHash> = chunk_slices
        .iter()
        .map(|data| compute_chunk_hash(data))
        .collect();

    let chunk_hashes_and_sizes: Vec<(MerkleHash, usize)> = chunk_hashes
        .iter()
        .zip(chunk_slices.iter())
        .map(|(h, d)| (*h, d.len()))
        .collect();

    let file_hash = compute_file_hash(&chunk_hashes_and_sizes);
    let file_hash_hex = file_hash.to_hex();

    // Split chunks into xorb groups
    struct XorbGroup {
        global_start: usize,
        global_end: usize,
        xorb_bytes: Vec<u8>,
    }

    let mut xorb_groups: Vec<XorbGroup> = Vec::new();
    let mut current_buffer: Vec<u8> = Vec::new();
    let mut group_start: usize = 0;

    for (i, chunk_slice) in chunk_slices.iter().enumerate() {
        let serialized = serialize_single_chunk(chunk_slice, CompressionType::Lz4).unwrap();

        if !current_buffer.is_empty() && current_buffer.len() + serialized.len() > XORB_SOFT_LIMIT {
            xorb_groups.push(XorbGroup {
                global_start: group_start,
                global_end: i,
                xorb_bytes: std::mem::take(&mut current_buffer),
            });
            group_start = i;
        }

        current_buffer.extend_from_slice(&serialized);
    }

    if !current_buffer.is_empty() {
        xorb_groups.push(XorbGroup {
            global_start: group_start,
            global_end: chunk_slices.len(),
            xorb_bytes: std::mem::take(&mut current_buffer),
        });
    }

    // Build shard metadata
    let mut file_data_entries = Vec::new();
    let mut cas_info_blocks = Vec::new();
    let mut verification_entries = Vec::new();
    let mut xorb_entries = Vec::new();

    for group in &xorb_groups {
        let local_count = group.global_end - group.global_start;
        let group_hashes_and_sizes = &chunk_hashes_and_sizes[group.global_start..group.global_end];

        let xorb_hash = compute_xorb_hash(group_hashes_and_sizes);
        let xorb_hash_hex = xorb_hash.to_hex();

        xorb_entries.push((xorb_hash_hex.clone(), Bytes::from(group.xorb_bytes.clone())));

        let group_unpacked: u32 = (group.global_start..group.global_end)
            .map(|i| chunk_slices[i].len() as u32)
            .sum();

        file_data_entries.push(FileDataSequenceEntry {
            cas_hash: xorb_hash,
            cas_flags: 0,
            unpacked_segment_bytes: group_unpacked,
            chunk_index_start: 0,
            chunk_index_end: local_count as u32,
        });

        let group_chunk_hashes: Vec<MerkleHash> = (group.global_start..group.global_end)
            .map(|i| chunk_hashes[i])
            .collect();
        verification_entries.push(FileVerificationEntry {
            range_hash: compute_verification_hash(&group_chunk_hashes),
        });

        let mut byte_offset = 0u32;
        let cas_entries: Vec<CASChunkSequenceEntry> = (group.global_start..group.global_end)
            .map(|i| {
                let size = chunk_slices[i].len() as u32;
                let entry = CASChunkSequenceEntry {
                    chunk_hash: chunk_hashes[i],
                    chunk_byte_range_start: byte_offset,
                    unpacked_segment_bytes: size,
                };
                byte_offset += size;
                entry
            })
            .collect();

        cas_info_blocks.push(CASInfoBlock {
            header: CASChunkSequenceHeader {
                cas_hash: xorb_hash,
                cas_flags: 0,
                num_entries: local_count as u32,
                num_bytes_in_cas: group_unpacked,
                num_bytes_on_disk: group.xorb_bytes.len() as u32,
            },
            entries: cas_entries,
        });
    }

    let file_info_block = FileInfoBlock {
        header: FileDataSequenceHeader {
            file_hash,
            file_flags: MDB_FILE_FLAG_WITH_VERIFICATION,
            num_entries: file_data_entries.len() as u32,
        },
        entries: file_data_entries,
        verification_entries,
        metadata_ext: None,
    };

    let shard = Shard {
        header: ShardHeader::new(0), // upload shards: footer_size = 0
        file_info_blocks: vec![file_info_block],
        cas_info_blocks,
        footer: None,
    };

    let shard_bytes = shard.to_bytes().unwrap();

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

    // Upload xorbs
    for (hash, data) in &artifacts.xorb_entries {
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
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
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "shard upload failed");
}
