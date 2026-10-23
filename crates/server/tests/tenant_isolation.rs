//! A caller only reads files it holds an ownership claim on; admins read
//! everything. Uploading a file (or identical content) records the claim.

#[allow(dead_code)]
mod helpers;

use helpers::*;

async fn status(server: &TestServer, path: &str, token: &str) -> reqwest::StatusCode {
    server
        .client
        .get(format!("{}{path}", server.base_url))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn other_users_cannot_read_a_file() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let alice = server.write_token_for("alice");
    upload_artifacts_with_token(&server, &artifacts, &alice).await;
    let file = &artifacts.file_hash;

    for token in [server.write_token_for("bob"), server.read_token_for("bob")] {
        for path in [
            format!("/v1/reconstructions/{file}"),
            format!("/v2/reconstructions/{file}"),
            format!("/v1/files/{file}"),
        ] {
            // Indistinguishable from a file that does not exist.
            assert_eq!(status(&server, &path, &token).await, 404, "{path}");
        }
    }

    // The owner (with any scope) and an admin can.
    for token in [
        alice.clone(),
        server.read_token_for("alice"),
        server.admin_token(),
    ] {
        assert_eq!(
            status(&server, &format!("/v2/reconstructions/{file}"), &token).await,
            200
        );
        assert_eq!(
            status(&server, &format!("/v1/files/{file}"), &token).await,
            200
        );
    }
    assert_eq!(
        download_via_protocol_with_token(&server, file, &server.read_token_for("alice")).await,
        data
    );
}

#[tokio::test]
async fn uploading_identical_content_grants_a_claim() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts_with_token(&server, &artifacts, &server.write_token_for("alice")).await;

    let bob = server.write_token_for("bob");
    let path = format!("/v2/reconstructions/{}", artifacts.file_hash);
    assert_eq!(status(&server, &path, &bob).await, 404);
    upload_artifacts_with_token(&server, &artifacts, &bob).await;
    assert_eq!(status(&server, &path, &bob).await, 200);

    // Bob's view of the file detail lists only his own claim.
    let detail: serde_json::Value = server
        .client
        .get(format!(
            "{}/v1/files/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owners: Vec<&str> = detail["owners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["owner"].as_str().unwrap())
        .collect();
    assert_eq!(owners, ["bob"]);
}

#[tokio::test]
async fn file_listing_is_scoped_to_the_caller() {
    let server = TestServer::start().await;
    let a = build_upload_artifacts(&generate_test_data(64 * 1024));
    let b = build_upload_artifacts(&generate_test_data(96 * 1024));
    upload_artifacts_with_token(&server, &a, &server.write_token_for("alice")).await;
    upload_artifacts_with_token(&server, &b, &server.write_token_for("bob")).await;

    let list = |token: String, query: &'static str| {
        let server = &server;
        async move {
            let resp = server
                .client
                .get(format!("{}/v1/files{query}", server.base_url))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let hashes: Vec<String> = if status.is_success() {
                let page: serde_json::Value = resp.json().await.unwrap();
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["file_hash"].as_str().unwrap().to_string())
                    .collect()
            } else {
                Vec::new()
            };
            (status, hashes)
        }
    };

    let (s, files) = list(server.read_token_for("bob"), "").await;
    assert_eq!(s, 200);
    assert_eq!(files, std::slice::from_ref(&b.file_hash));

    let (s, files) = list(server.read_token_for("bob"), "?owner=bob").await;
    assert_eq!(s, 200);
    assert_eq!(files, std::slice::from_ref(&b.file_hash));

    let (s, _) = list(server.read_token_for("bob"), "?owner=alice").await;
    assert_eq!(s, 403);

    let (s, files) = list(server.admin_token(), "").await;
    assert_eq!(s, 200);
    assert_eq!(files.len(), 2);
}

#[tokio::test]
async fn cross_tenant_management_endpoints_are_admin_only() {
    let server = TestServer::start().await;
    for path in ["/v1/accounting", "/v1/xorbs"] {
        assert_eq!(
            status(&server, path, &server.write_token()).await,
            403,
            "{path}"
        );
        assert_eq!(
            status(&server, path, &server.admin_token()).await,
            200,
            "{path}"
        );
    }
}

/// Build an upload shard registering one file made of chunks `[0, num_chunks)`
/// of `xorb_hash`, the way someone who only knows the xorb hash would.
fn forged_shard(
    xorb_hash: &str,
    num_chunks: u32,
    unpacked_bytes: u32,
    verification: Option<xet_core_structures::merklehash::MerkleHash>,
) -> (String, bytes::Bytes) {
    use xet_core_structures::merklehash::MerkleHash;
    use xet_core_structures::metadata_shard::MDBShardFileHeader;
    use xet_core_structures::metadata_shard::file_structs::{
        FileDataSequenceEntry, FileDataSequenceHeader, FileVerificationEntry, MDBFileInfo,
    };
    use xet_core_structures::metadata_shard::xorb_structs::XorbChunkSequenceHeader;

    let xorb = MerkleHash::from_hex(xorb_hash).unwrap();
    // A file that is exactly one whole xorb hashes to the xorb hash keyed with
    // the zero salt, so it is computable from the xorb hash alone.
    let file_hash = xorb.hmac((&[0u8; 32]).into());
    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash, 1, verification.is_some(), false),
        segments: vec![FileDataSequenceEntry::new(
            xorb,
            unpacked_bytes,
            0u32,
            num_chunks,
        )],
        verification: verification
            .map(|v| vec![FileVerificationEntry::new(v)])
            .unwrap_or_default(),
        metadata_ext: None,
    };
    let mut shard = Vec::new();
    MDBShardFileHeader {
        footer_size: 0,
        ..Default::default()
    }
    .serialize(&mut shard)
    .unwrap();
    file_info.serialize(&mut shard).unwrap();
    FileDataSequenceHeader::bookend()
        .serialize(&mut shard)
        .unwrap();
    XorbChunkSequenceHeader::bookend()
        .serialize(&mut shard)
        .unwrap();
    (file_hash.hex(), bytes::Bytes::from(shard))
}

/// Knowing a xorb hash must not let a writer register a file over that xorb
/// and so gain a claim that reads another tenant's bytes.
#[tokio::test]
async fn xorb_hash_alone_cannot_mint_a_claim() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts_with_token(&server, &artifacts, &server.write_token_for("alice")).await;
    assert_eq!(artifacts.xorb_entries.len(), 1);
    let xorb_hash = &artifacts.xorb_entries[0].0;
    let num_chunks = artifacts.chunk_hashes.len() as u32;
    let bob = server.write_token_for("bob");

    let post = |shard: bytes::Bytes| {
        server
            .client
            .post(format!("{}/v1/shards", server.base_url))
            .bearer_auth(&bob)
            .body(shard)
            .send()
    };

    // Without verification entries.
    let (forged_file, shard) = forged_shard(xorb_hash, num_chunks, data.len() as u32, None);
    let resp = post(shard).await.unwrap();
    assert_eq!(resp.status(), 400);
    let path = format!("/v2/reconstructions/{forged_file}");
    assert_eq!(status(&server, &path, &bob).await, 404);

    // With a wrong verification hash, the error must not hand back the right one.
    let wrong = xet_core_structures::merklehash::MerkleHash::default();
    let (_, shard) = forged_shard(xorb_hash, num_chunks, data.len() as u32, Some(wrong));
    let resp = post(shard).await.unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    let correct = xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks(
        &artifacts.chunk_hashes,
    );
    assert!(
        !body.contains(&correct.hex()),
        "error leaked the verification hash: {body}"
    );
}

/// A shard must not be able to misstate a term's unpacked length: the file
/// index points at the most recent shard for a file, so a lie there would
/// corrupt range reads for every owner of that file.
#[tokio::test]
async fn shard_with_wrong_unpacked_length_is_rejected() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts_with_token(&server, &artifacts, &server.write_token_for("alice")).await;
    let xorb_hash = &artifacts.xorb_entries[0].0;
    let verification =
        xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks(
            &artifacts.chunk_hashes,
        );
    let num_chunks = artifacts.chunk_hashes.len() as u32;

    let (_, shard) = forged_shard(
        xorb_hash,
        num_chunks,
        data.len() as u32 + 1,
        Some(verification),
    );
    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(server.write_token_for("bob"))
        .body(shard)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // The honest version of the same shard is accepted.
    let (_, shard) = forged_shard(xorb_hash, num_chunks, data.len() as u32, Some(verification));
    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(server.write_token_for("bob"))
        .body(shard)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
