//! Management API tests: cursor-paginated file/xorb listing and file detail.

mod helpers;

use std::collections::BTreeSet;

use helpers::{
    TestServer, UploadArtifacts, build_upload_artifacts, generate_test_data, upload_artifacts,
    upload_artifacts_with_token,
};

#[derive(serde::Deserialize)]
struct Page<T> {
    items: Vec<T>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(serde::Deserialize)]
struct FileListEntry {
    file_hash: String,
    shard_hash: String,
    logical_bytes: u64,
}

#[derive(serde::Deserialize)]
struct XorbSummary {
    xorb_hash: String,
    num_bytes_on_disk: u64,
    chunk_count: u64,
}

#[derive(serde::Deserialize)]
struct FileDetail {
    file_hash: String,
    shard_hash: String,
    logical_bytes: u64,
    owners: Vec<OwnerClaim>,
    xorbs: Vec<String>,
}

#[derive(serde::Deserialize)]
struct OwnerClaim {
    owner: String,
    logical_bytes: u64,
    created_at_unix: i64,
}

/// Upload `n` distinct files (distinct sizes → distinct content/hashes) and
/// return their artifacts.
async fn upload_n(server: &TestServer, n: usize) -> Vec<UploadArtifacts> {
    let mut out = Vec::new();
    for i in 0..n {
        let data = generate_test_data((i + 1) * 64 * 1024);
        let artifacts = build_upload_artifacts(&data);
        upload_artifacts(server, &artifacts).await;
        out.push(artifacts);
    }
    out
}

async fn list_files_page(
    server: &TestServer,
    query: &str,
) -> (reqwest::StatusCode, Option<Page<FileListEntry>>) {
    let resp = server
        .client
        .get(format!("{}/v1/files?{query}", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = if status.is_success() {
        Some(resp.json().await.unwrap())
    } else {
        None
    };
    (status, body)
}

/// Walk every page of `/v1/files` with the given base query and `limit`,
/// returning the concatenated file hashes in the order seen.
async fn paginate_all_files(server: &TestServer, base_query: &str, limit: usize) -> Vec<String> {
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut query = format!("{base_query}&limit={limit}");
        if let Some(c) = &cursor {
            query.push_str(&format!("&cursor={c}"));
        }
        let (status, page) = list_files_page(server, &query).await;
        assert_eq!(status, 200);
        let page = page.unwrap();
        assert!(page.items.len() <= limit, "page exceeded requested limit");
        seen.extend(page.items.into_iter().map(|e| e.file_hash));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    seen
}

#[tokio::test]
async fn file_listing_paginates_in_hash_order_without_gaps_or_dups() {
    let server = TestServer::start().await;
    let artifacts = upload_n(&server, 5).await;
    let expected: BTreeSet<String> = artifacts.iter().map(|a| a.file_hash.clone()).collect();

    // Page size 2 over 5 files → 3 pages (2, 2, 1).
    let seen = paginate_all_files(&server, "", 2).await;

    // Every file exactly once…
    assert_eq!(seen.len(), expected.len(), "wrong number of files listed");
    assert_eq!(
        seen.iter().cloned().collect::<BTreeSet<_>>(),
        expected,
        "listed files differ from uploaded set"
    );
    // …and strictly ascending by hash across page boundaries.
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "results not globally ordered by hash");
}

#[tokio::test]
async fn file_listing_reports_logical_size() {
    let server = TestServer::start().await;
    let data = generate_test_data(200 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let (status, page) = list_files_page(&server, "limit=10").await;
    assert_eq!(status, 200);
    let page = page.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].file_hash, artifacts.file_hash);
    assert_eq!(page.items[0].logical_bytes, data.len() as u64);
    assert!(!page.items[0].shard_hash.is_empty());
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn file_listing_filters_by_owner() {
    let server = TestServer::start().await;
    let alice = server.write_token_for("alice");
    let bob = server.write_token_for("bob");

    let a1 = build_upload_artifacts(&generate_test_data(64 * 1024));
    let a2 = build_upload_artifacts(&generate_test_data(128 * 1024));
    let b1 = build_upload_artifacts(&generate_test_data(192 * 1024));
    upload_artifacts_with_token(&server, &a1, &alice).await;
    upload_artifacts_with_token(&server, &a2, &alice).await;
    upload_artifacts_with_token(&server, &b1, &bob).await;

    let alice_files = paginate_all_files(&server, "owner=alice", 1)
        .await
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        alice_files,
        BTreeSet::from([a1.file_hash.clone(), a2.file_hash.clone()])
    );

    let bob_files = paginate_all_files(&server, "owner=bob", 10)
        .await
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(bob_files, BTreeSet::from([b1.file_hash.clone()]));

    // Unfiltered lists all three.
    let all = paginate_all_files(&server, "owner=", 10).await;
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn file_detail_returns_owners_and_referenced_xorbs() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);

    let alice = server.write_token_for("alice");
    let bob = server.write_token_for("bob");
    upload_artifacts_with_token(&server, &artifacts, &alice).await;
    upload_artifacts_with_token(&server, &artifacts, &bob).await;

    let resp = server
        .client
        .get(format!(
            "{}/v1/files/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let detail: FileDetail = resp.json().await.unwrap();

    assert_eq!(detail.file_hash, artifacts.file_hash);
    assert!(!detail.shard_hash.is_empty());
    assert_eq!(detail.logical_bytes, data.len() as u64);

    let owner_names: BTreeSet<String> = detail.owners.iter().map(|o| o.owner.clone()).collect();
    assert_eq!(
        owner_names,
        BTreeSet::from(["alice".to_string(), "bob".to_string()])
    );
    assert!(
        detail
            .owners
            .iter()
            .all(|o| o.logical_bytes == data.len() as u64)
    );
    assert!(detail.owners.iter().all(|o| o.created_at_unix > 0));

    // The referenced xorbs must be exactly the ones the artifacts carry.
    let expected_xorbs: BTreeSet<String> = artifacts
        .xorb_entries
        .iter()
        .map(|(h, _)| h.clone())
        .collect();
    assert_eq!(
        detail.xorbs.into_iter().collect::<BTreeSet<_>>(),
        expected_xorbs
    );
}

#[tokio::test]
async fn file_detail_404_for_unknown_file() {
    let server = TestServer::start().await;
    let missing = "a".repeat(64);
    let resp = server
        .client
        .get(format!("{}/v1/files/{missing}", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn xorb_listing_paginates() {
    let server = TestServer::start().await;
    let artifacts = upload_n(&server, 3).await;
    let expected_xorbs: BTreeSet<String> = artifacts
        .iter()
        .flat_map(|a| a.xorb_entries.iter().map(|(h, _)| h.clone()))
        .collect();

    // Page size 1 to force multiple pages.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut query = "limit=1".to_string();
        if let Some(c) = &cursor {
            query.push_str(&format!("&cursor={c}"));
        }
        let resp = server
            .client
            .get(format!("{}/v1/xorbs?{query}", server.base_url))
            .bearer_auth(server.read_token())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let page: Page<XorbSummary> = resp.json().await.unwrap();
        assert!(page.items.len() <= 1);
        for x in &page.items {
            assert!(x.num_bytes_on_disk > 0, "xorb should report stored size");
            assert!(x.chunk_count > 0, "xorb should report chunk count");
        }
        seen.extend(page.items.into_iter().map(|x| x.xorb_hash));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    assert_eq!(
        seen.iter().cloned().collect::<BTreeSet<_>>(),
        expected_xorbs,
        "listed xorbs differ from uploaded set"
    );
    // Ordered ascending, no duplicates.
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen, sorted, "xorbs not ordered / contained duplicates");
}

#[tokio::test]
async fn invalid_cursor_is_rejected() {
    let server = TestServer::start().await;
    let resp = server
        .client
        .get(format!(
            "{}/v1/files?cursor=not_base64_!!!",
            server.base_url
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "a mangled cursor should be a 400");
}

#[tokio::test]
async fn limit_is_capped() {
    let server = TestServer::start().await;
    upload_n(&server, 2).await;

    // An absurd limit must not error; it is clamped server-side.
    let (status, page) = list_files_page(&server, "limit=100000000").await;
    assert_eq!(status, 200);
    assert_eq!(page.unwrap().items.len(), 2);
}
