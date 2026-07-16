mod support;

use std::hint::black_box;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use futures_util::StreamExt;
use futures_util::future::join_all;
use tokio::runtime::{Builder, Runtime};
use tower::ServiceExt;
use vault_server::auth::header_identity;
use vault_server::reconciliation::storage_reconciliation_report;
use vault_server::state_events::state_events_after;
use vault_server::views::{build_contents_payload, build_sidebar_payload};

use support::{
    CONCURRENT_TRANSFER_BYTES, CONCURRENT_USERS, DOWNLOAD_BYTES, EXPORT_BYTES, ExportScenario,
    LARGE_DOWNLOAD_BYTES, LARGE_UPLOAD_BYTES, LargeDownloadFixture, PerformanceFixture,
    UploadScenario,
};

fn benchmark_config() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1_500))
        .noise_threshold(0.05)
}

fn runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("performance benchmark runtime")
}

fn vault_performance(criterion: &mut Criterion) {
    let runtime = runtime();
    let fixture = runtime.block_on(PerformanceFixture::build());
    let large_download = runtime.block_on(LargeDownloadFixture::build());

    benchmark_views(criterion, &runtime, &fixture);
    benchmark_authentication(criterion, &runtime, &fixture);
    benchmark_exports(criterion, &runtime);
    benchmark_transfers(criterion, &runtime, &fixture, &large_download);
    benchmark_maintenance(criterion, &runtime, &fixture);
}

fn benchmark_views(criterion: &mut Criterion, runtime: &Runtime, fixture: &PerformanceFixture) {
    let mut group = criterion.benchmark_group("views");
    group.bench_function("contents_wide_non_admin", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let payload = build_contents_payload(
                &fixture.state.db,
                &fixture.target_folder,
                &fixture.user,
                "",
                false,
            )
            .await
            .expect("contents benchmark");
            black_box((payload.folders.len(), payload.documents.len()));
        });
    });
    group.bench_function("sidebar_deep_non_admin", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let payload = build_sidebar_payload(&fixture.state.db, &fixture.user)
                .await
                .expect("sidebar benchmark");
            black_box((payload.folder_children.len(), payload.folder_metadata.len()));
        });
    });
    group.finish();
}

fn benchmark_authentication(
    criterion: &mut Criterion,
    runtime: &Runtime,
    fixture: &PerformanceFixture,
) {
    let mut group = criterion.benchmark_group("authentication");
    group.bench_function("warm_header_identity", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let user = header_identity(&fixture.auth, &fixture.state.db, &fixture.auth_headers)
                .await
                .expect("warm header identity benchmark");
            black_box(user);
        });
    });
    group.bench_function("warm_header_identity_concurrent_8", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let requests = (0..8)
                .map(|_| header_identity(&fixture.auth, &fixture.state.db, &fixture.auth_headers));
            let users = join_all(requests).await;
            for user in &users {
                assert!(user.is_ok(), "concurrent header identity benchmark");
            }
            black_box(users);
        });
    });
    group.finish();
}

fn benchmark_exports(criterion: &mut Criterion, runtime: &Runtime) {
    let mut group = criterion.benchmark_group("exports");
    group
        .sample_size(10)
        .sampling_mode(SamplingMode::Flat)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1))
        .throughput(criterion::Throughput::Bytes(EXPORT_BYTES as u64));
    group.bench_function("forced_deflate_32_mib", |bencher| {
        bencher
            .to_async(runtime)
            .iter_custom(|iterations| async move {
                let mut measured = Duration::ZERO;
                for _ in 0..iterations {
                    let scenario = ExportScenario::build().await;
                    let start = Instant::now();
                    scenario.export_and_wait().await;
                    measured += start.elapsed();
                    drop(scenario);
                }
                measured
            });
    });
    group.finish();
}

#[allow(clippy::too_many_lines)]
fn benchmark_transfers(
    criterion: &mut Criterion,
    runtime: &Runtime,
    fixture: &PerformanceFixture,
    large_download: &LargeDownloadFixture,
) {
    let mut group = criterion.benchmark_group("transfers");
    group
        .sample_size(10)
        .sampling_mode(SamplingMode::Flat)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1));
    group.throughput(criterion::Throughput::Bytes(LARGE_DOWNLOAD_BYTES as u64));
    group.bench_function("router_full_download_256_mib", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let mut request = Request::builder()
                .uri(format!(
                    "/documents/{}/download",
                    large_download.document_id
                ))
                .body(Body::empty())
                .expect("download benchmark request");
            *request.headers_mut() = large_download.auth_headers[0].clone();
            let response = large_download
                .app
                .clone()
                .oneshot(request)
                .await
                .expect("download benchmark response");
            assert_eq!(response.status(), StatusCode::OK);
            black_box(drain_body(response, LARGE_DOWNLOAD_BYTES).await);
        });
    });
    group.throughput(criterion::Throughput::Bytes(DOWNLOAD_BYTES as u64));
    group.bench_function("local_read_8_mib", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let data = fixture
                .local_storage
                .read_bytes(&fixture.direct_object_key)
                .await
                .expect("local read benchmark");
            assert_eq!(data.len(), DOWNLOAD_BYTES);
            black_box(data);
        });
    });
    group.throughput(criterion::Throughput::Bytes(LARGE_UPLOAD_BYTES as u64));
    group.bench_function("router_upload_complete_256_mib", |bencher| {
        bencher
            .to_async(runtime)
            .iter_custom(|iterations| async move {
                let mut measured = Duration::ZERO;
                for _ in 0..iterations {
                    let scenario = UploadScenario::build(1, LARGE_UPLOAD_BYTES).await;
                    let start = Instant::now();
                    scenario.upload_and_complete().await;
                    measured += start.elapsed();
                    drop(scenario);
                }
                measured
            });
    });
    group.throughput(criterion::Throughput::Bytes(
        (CONCURRENT_USERS * CONCURRENT_TRANSFER_BYTES) as u64,
    ));
    group.bench_function("router_upload_complete_12_users", |bencher| {
        bencher
            .to_async(runtime)
            .iter_custom(|iterations| async move {
                let mut measured = Duration::ZERO;
                for _ in 0..iterations {
                    let scenario = UploadScenario::build(
                        CONCURRENT_USERS,
                        i64::try_from(CONCURRENT_TRANSFER_BYTES).expect("concurrent transfer size"),
                    )
                    .await;
                    let start = Instant::now();
                    scenario.upload_and_complete().await;
                    measured += start.elapsed();
                    drop(scenario);
                }
                measured
            });
    });
    group.bench_function("concurrent_range_download_12_users", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let downloads = (0..CONCURRENT_USERS).map(|index| {
                let app = large_download.app.clone();
                let request = range_download_request(large_download, index);
                async move {
                    let response = app
                        .oneshot(request)
                        .await
                        .expect("concurrent download response");
                    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
                    drain_body(response, CONCURRENT_TRANSFER_BYTES).await
                }
            });
            let lengths = join_all(downloads).await;
            black_box(lengths);
        });
    });
    group.throughput(criterion::Throughput::Bytes(
        (2 * CONCURRENT_USERS * CONCURRENT_TRANSFER_BYTES) as u64,
    ));
    group.bench_function("mixed_upload_download_12_users", |bencher| {
        bencher
            .to_async(runtime)
            .iter_custom(|iterations| async move {
                let mut measured = Duration::ZERO;
                for _ in 0..iterations {
                    let scenario = UploadScenario::build(
                        CONCURRENT_USERS,
                        i64::try_from(CONCURRENT_TRANSFER_BYTES).expect("mixed transfer size"),
                    )
                    .await;
                    let download_batch = async {
                        let downloads = (0..CONCURRENT_USERS).map(|index| {
                            let app = large_download.app.clone();
                            let request = range_download_request(large_download, index);
                            async move {
                                let response =
                                    app.oneshot(request).await.expect("mixed download response");
                                assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
                                drain_body(response, CONCURRENT_TRANSFER_BYTES).await
                            }
                        });
                        join_all(downloads).await
                    };
                    let start = Instant::now();
                    let ((), downloads) =
                        tokio::join!(scenario.upload_and_complete(), download_batch);
                    measured += start.elapsed();
                    black_box(downloads);
                    drop(scenario);
                }
                measured
            });
    });
    group.finish();
}

fn range_download_request(fixture: &LargeDownloadFixture, user_index: usize) -> Request<Body> {
    let start = user_index * CONCURRENT_TRANSFER_BYTES;
    let end = start + CONCURRENT_TRANSFER_BYTES - 1;
    let mut request = Request::builder()
        .uri(format!("/documents/{}/download", fixture.document_id))
        .header("range", format!("bytes={start}-{end}"))
        .body(Body::empty())
        .expect("range download benchmark request");
    for (name, value) in &fixture.auth_headers[user_index] {
        request.headers_mut().insert(name, value.clone());
    }
    request
}

async fn drain_body(response: axum::response::Response, expected_bytes: usize) -> usize {
    let mut body = response.into_body().into_data_stream();
    let mut total = 0_usize;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.expect("transfer benchmark response body");
        total = total
            .checked_add(chunk.len())
            .expect("transfer benchmark body length");
        black_box(chunk.len());
    }
    assert_eq!(total, expected_bytes);
    total
}

fn benchmark_maintenance(
    criterion: &mut Criterion,
    runtime: &Runtime,
    fixture: &PerformanceFixture,
) {
    let mut group = criterion.benchmark_group("maintenance");
    group.bench_function("state_events_tail_after_10k", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let events = state_events_after(&fixture.state.db, fixture.state_event_cursor)
                .await
                .expect("state event tail benchmark");
            assert_eq!(events.len(), 100);
            black_box(events);
        });
    });
    group.bench_function("storage_reconciliation_dry_run", |bencher| {
        bencher.to_async(runtime).iter(|| async {
            let report =
                storage_reconciliation_report(&fixture.state.db, &fixture.local_storage, false)
                    .await
                    .expect("reconciliation benchmark");
            black_box(report);
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = benchmark_config();
    targets = vault_performance
}
criterion_main!(benches);
