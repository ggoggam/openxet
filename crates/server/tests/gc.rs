//! Lifecycle tests: ownership accounting, file deletion, and GC.

mod helpers;

use helpers::{
    TestServer, build_upload_artifacts, download_via_protocol, generate_test_data,
    upload_artifacts, upload_artifacts_with_token,
};

#[derive(serde::Deserialize)]
struct AccountingResponse {
    owners: Vec<OwnerUsage>,
    files: u64,
    claimed_files: u64,
    unique_file_bytes: u64,
    xorb_count: u64,
    physical_xorb_bytes: u64,
    shard_count: u64,
    dedup_ratio: f64,
}

#[derive(serde::Deserialize)]
struct OwnerUsage {
    owner: String,
    file_count: u64,
    logical_bytes: u64,
}

#[derive(serde::Deserialize)]
struct GcReport {
    live_files: u64,
    live_xorbs: u64,
    deleted_xorbs: u64,
    freed_xorb_bytes: u64,
    deleted_shards: u64,
    skipped_in_grace: u64,
}

#[derive(serde::Deserialize)]
struct DeleteFileResponse {
    deleted: bool,
    remaining_owners: u64,
}

async fn get_accounting(server: &TestServer) -> AccountingResponse {
    let resp = server
        .client
        .get(format!("{}/v1/accounting", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "accounting query failed");
    resp.json().await.unwrap()
}

async fn run_gc(server: &TestServer, grace_seconds: u64) -> GcReport {
    let resp = server
        .client
        .post(format!(
            "{}/v1/gc?grace_seconds={grace_seconds}",
            server.base_url
        ))
        .bearer_auth(server.write_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "gc failed");
    resp.json().await.unwrap()
}

async fn delete_file(server: &TestServer, token: &str, file_hash: &str) -> DeleteFileResponse {
    let resp = server
        .client
        .delete(format!("{}/v1/files/{file_hash}", server.base_url))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "delete failed");
    resp.json().await.unwrap()
}

#[tokio::test]
async fn accounting_reports_owner_usage_and_physical_stats() {
    let server = TestServer::start().await;
    let data = generate_test_data(512 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let acc = get_accounting(&server).await;

    assert_eq!(acc.files, 1);
    assert_eq!(acc.claimed_files, 1);
    assert_eq!(acc.unique_file_bytes, data.len() as u64);
    assert_eq!(acc.owners.len(), 1);
    assert_eq!(acc.owners[0].owner, "test-user");
    assert_eq!(acc.owners[0].file_count, 1);
    assert_eq!(acc.owners[0].logical_bytes, data.len() as u64);
    assert!(acc.xorb_count >= 1);
    assert!(acc.physical_xorb_bytes > 0);
    assert_eq!(acc.shard_count, 1);
    assert!(acc.dedup_ratio > 0.0);
}

#[tokio::test]
async fn gc_with_zero_grace_keeps_live_data() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let report = run_gc(&server, 0).await;
    assert_eq!(report.live_files, 1);
    assert!(report.live_xorbs >= 1);
    assert_eq!(report.deleted_xorbs, 0);
    assert_eq!(report.deleted_shards, 0);

    // The file must still reconstruct byte-for-byte after the sweep.
    let downloaded = download_via_protocol(&server, &artifacts.file_hash).await;
    assert_eq!(downloaded, data);
}

#[tokio::test]
async fn delete_then_gc_reclaims_all_storage() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let del = delete_file(&server, &server.write_token(), &artifacts.file_hash).await;
    assert!(del.deleted);
    assert_eq!(del.remaining_owners, 0);

    // Reconstruction must 404 immediately after deletion.
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let report = run_gc(&server, 0).await;
    assert_eq!(report.live_files, 0);
    assert_eq!(report.deleted_xorbs, artifacts.xorb_entries.len() as u64);
    assert!(report.freed_xorb_bytes > 0);
    assert_eq!(report.deleted_shards, 1);

    let acc = get_accounting(&server).await;
    assert_eq!(acc.files, 0);
    assert_eq!(acc.xorb_count, 0);
    assert_eq!(acc.shard_count, 0);
    assert!(acc.owners.is_empty());

    // Dedup queries must no longer advertise the collected xorb's chunks.
    let chunk = artifacts.chunk_hashes[0].hex();
    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{chunk}",
            server.base_url
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn grace_period_protects_unreferenced_uploads() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);

    // Upload only the xorbs — an upload in flight (no shard registered yet).
    let token = server.write_token();
    for (hash, bytes) in &artifacts.xorb_entries {
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // A pass with the default-scale grace must not touch them.
    let report = run_gc(&server, 24 * 60 * 60).await;
    assert_eq!(report.deleted_xorbs, 0);
    assert_eq!(report.skipped_in_grace, artifacts.xorb_entries.len() as u64);

    // Once past the grace period (simulated with 0), orphans are collected.
    let report = run_gc(&server, 0).await;
    assert_eq!(report.deleted_xorbs, artifacts.xorb_entries.len() as u64);
}

#[tokio::test]
async fn shared_file_survives_until_last_owner_releases() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);

    let alice = server.write_token_for("alice");
    let bob = server.write_token_for("bob");
    upload_artifacts_with_token(&server, &artifacts, &alice).await;
    upload_artifacts_with_token(&server, &artifacts, &bob).await;

    let acc = get_accounting(&server).await;
    assert_eq!(acc.claimed_files, 1);
    assert_eq!(acc.owners.len(), 2);
    // Both claimants are charged the full logical size…
    assert!(acc.owners.iter().all(|o| o.logical_bytes == data.len() as u64));
    // …but the dedup-aware total counts the file once.
    assert_eq!(acc.unique_file_bytes, data.len() as u64);

    // A stranger holds no claim and cannot release one.
    let resp = server
        .client
        .delete(format!(
            "{}/v1/files/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.write_token_for("mallory"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Alice releases: file stays live for Bob and survives GC.
    let del = delete_file(&server, &alice, &artifacts.file_hash).await;
    assert!(!del.deleted);
    assert_eq!(del.remaining_owners, 1);

    let report = run_gc(&server, 0).await;
    assert_eq!(report.deleted_xorbs, 0);
    let downloaded = download_via_protocol(&server, &artifacts.file_hash).await;
    assert_eq!(downloaded, data);

    // Bob releases: the file is gone and its storage is collectable.
    let del = delete_file(&server, &bob, &artifacts.file_hash).await;
    assert!(del.deleted);
    assert_eq!(del.remaining_owners, 0);

    let report = run_gc(&server, 0).await;
    assert_eq!(report.deleted_xorbs, artifacts.xorb_entries.len() as u64);
    assert_eq!(report.deleted_shards, 1);
}
