mod helpers;

use openxet_cas_types::reconstruction::QueryReconstructionResponse;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

/// Helper to query reconstruction with an optional Range header.
async fn get_reconstruction(
    server: &TestServer,
    file_hash: &str,
    range: Option<&str>,
) -> reqwest::Response {
    let token = server.read_token();
    let mut req = server.client.get(format!(
        "{}/v1/reconstructions/{file_hash}",
        server.base_url
    ));
    req = req.bearer_auth(&token);
    if let Some(range) = range {
        req = req.header("range", range);
    }
    req.send().await.unwrap()
}

/// Range request returns only terms overlapping the requested byte range.
#[tokio::test]
async fn test_range_partial() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    // Get full reconstruction first for comparison
    let resp = get_reconstruction(&server, &artifacts.file_hash, None).await;
    assert_eq!(resp.status(), 200);
    let full_recon: QueryReconstructionResponse = resp.json().await.unwrap();
    let full_total: u64 = full_recon.terms.iter().map(|t| t.unpacked_length).sum();
    assert_eq!(full_total, data.len() as u64);

    // Request a sub-range
    let range_start = 1000u64;
    let range_end = 2000u64;
    let resp = get_reconstruction(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes={range_start}-{range_end}")),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let range_recon: QueryReconstructionResponse = resp.json().await.unwrap();

    // The range terms should cover at least the requested bytes
    let range_total: u64 = range_recon.terms.iter().map(|t| t.unpacked_length).sum();
    // After accounting for offset_into_first_range, the effective data should cover the range
    let effective_end = range_total - range_recon.offset_into_first_range;
    assert!(effective_end > range_end - range_start);

    // terms should be a subset (or equal) of full terms
    assert!(range_recon.terms.len() <= full_recon.terms.len());
}

/// Range request past end of file returns 416.
#[tokio::test]
async fn test_range_past_end_416() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let past_end = data.len() as u64;
    let resp = get_reconstruction(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes={past_end}-{}", past_end + 100)),
    )
    .await;
    assert_eq!(resp.status(), 416);
}

/// Range with end beyond file size is clamped.
#[tokio::test]
async fn test_range_clamped_end() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let resp = get_reconstruction(&server, &artifacts.file_hash, Some("bytes=100-999999999")).await;
    assert_eq!(resp.status(), 200);

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();
    // Should return terms covering bytes 100 to EOF
    assert!(!recon.terms.is_empty());
}

/// Range covering the full file is equivalent to no Range header.
#[tokio::test]
async fn test_range_full_file() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let last_byte = data.len() as u64 - 1;

    // Full range
    let resp = get_reconstruction(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes=0-{last_byte}")),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let range_recon: QueryReconstructionResponse = resp.json().await.unwrap();

    // No range
    let resp = get_reconstruction(&server, &artifacts.file_hash, None).await;
    assert_eq!(resp.status(), 200);
    let full_recon: QueryReconstructionResponse = resp.json().await.unwrap();

    assert_eq!(range_recon.offset_into_first_range, 0);
    assert_eq!(range_recon.terms.len(), full_recon.terms.len());
    let range_total: u64 = range_recon.terms.iter().map(|t| t.unpacked_length).sum();
    let full_total: u64 = full_recon.terms.iter().map(|t| t.unpacked_length).sum();
    assert_eq!(range_total, full_total);
}
