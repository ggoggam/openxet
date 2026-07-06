mod helpers;

use std::time::Instant;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

/// Time a CAS protocol upload for 1 MiB of data.
#[tokio::test]
#[ignore]
async fn test_perf_upload_1mb() {
    let server = TestServer::start().await;
    let data = generate_test_data(1024 * 1024);
    let artifacts = build_upload_artifacts(&data);

    let start = Instant::now();
    upload_artifacts(&server, &artifacts).await;
    let elapsed = start.elapsed();

    let throughput_mbps = (data.len() as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();
    println!(
        "[perf] 1 MiB upload: {:.2}ms ({:.1} MB/s)",
        elapsed.as_millis(),
        throughput_mbps
    );
}

/// Time a CAS protocol upload for 10 MiB of data.
#[tokio::test]
#[ignore]
async fn test_perf_upload_10mb() {
    let server = TestServer::start().await;
    let data = generate_test_data(10 * 1024 * 1024);
    let artifacts = build_upload_artifacts(&data);

    let start = Instant::now();
    upload_artifacts(&server, &artifacts).await;
    let elapsed = start.elapsed();

    let throughput_mbps = (data.len() as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();
    println!(
        "[perf] 10 MiB upload: {:.2}ms ({:.1} MB/s)",
        elapsed.as_millis(),
        throughput_mbps
    );
}

/// Measure reconstruction query latency over 100 iterations.
#[tokio::test]
#[ignore]
async fn test_perf_reconstruction_latency() {
    let server = TestServer::start().await;
    let data = generate_test_data(1024 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let iterations = 100;
    let mut durations = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let resp = server
            .client
            .get(format!(
                "{}/v1/reconstructions/{}",
                server.base_url, artifacts.file_hash
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.bytes().await.unwrap();
        durations.push(start.elapsed());
    }

    durations.sort();
    let avg_us = durations.iter().map(|d| d.as_micros()).sum::<u128>() / iterations as u128;
    let p99_us = durations[iterations * 99 / 100].as_micros();

    println!("[perf] reconstruction latency ({iterations} iters): avg={avg_us}us, p99={p99_us}us");
}

/// Concurrent uploads: 10 parallel 1 MiB files via the CAS protocol.
#[tokio::test]
#[ignore]
async fn test_perf_concurrent_uploads() {
    let server = TestServer::start().await;
    let token = server.write_token();

    let start = Instant::now();
    let mut handles = Vec::new();

    for i in 0..10u16 {
        let base_url = server.base_url.clone();
        let client = server.client.clone();
        let token = token.clone();
        handles.push(tokio::spawn(async move {
            // Each task gets slightly different data by using a different seed
            let mut data = generate_test_data(1024 * 1024);
            // Make each file unique by setting the first few bytes
            data[0] = i as u8;
            data[1] = (i >> 8) as u8;
            let artifacts = build_upload_artifacts(&data);

            for (hash, xorb_data) in &artifacts.xorb_entries {
                let resp = client
                    .post(format!("{base_url}/v1/xorbs/default/{hash}"))
                    .bearer_auth(&token)
                    .body(xorb_data.clone())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200);
            }
            let resp = client
                .post(format!("{base_url}/v1/shards"))
                .bearer_auth(&token)
                .body(artifacts.shard_bytes.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            artifacts.file_hash
        }));
    }

    let mut file_hashes = Vec::new();
    for handle in handles {
        file_hashes.push(handle.await.unwrap());
    }

    let elapsed = start.elapsed();
    println!(
        "[perf] 10 concurrent 1 MiB uploads: {:.2}ms total",
        elapsed.as_millis()
    );

    // Verify every file is reconstructible
    let read_token = server.read_token();
    for file_hash in file_hashes {
        let resp = server
            .client
            .get(format!(
                "{}/v1/reconstructions/{file_hash}",
                server.base_url
            ))
            .bearer_auth(&read_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}
