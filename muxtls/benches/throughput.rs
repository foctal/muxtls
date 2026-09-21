use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use muxtls::{ClientConfig, Connection, Endpoint, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn setup_connection() -> Connection {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().unwrap();
    let server = Endpoint::server("127.0.0.1:0", server_cfg).await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let conn = server.accept().await.unwrap();
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            tokio::spawn(async move {
                tokio::io::copy(&mut recv, &mut send).await.unwrap();
                send.shutdown().await.unwrap();
            });
        }
    });
    Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).unwrap())
        .connect(addr, "localhost")
        .unwrap()
        .await
        .unwrap()
}

fn bench_stream_roundtrip(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("tls_stream_roundtrip");
    group.sample_size(10);
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(2));
    for (streams, size) in [(1, 64), (1, 16 * 1024), (1, 256 * 1024), (8, 16 * 1024)] {
        let connection = runtime.block_on(setup_connection());
        let payload = vec![42u8; size];
        group.throughput(Throughput::Bytes((streams * size) as u64));
        group.bench_with_input(
            BenchmarkId::new(format!("{streams}_streams"), size),
            &size,
            |b, _| {
                b.to_async(&runtime).iter(|| async {
                    let operations = (0..streams).map(|_| async {
                        let (mut send, mut recv) = connection.open_bi().await.unwrap();
                        send.write_all(&payload).await.unwrap();
                        send.shutdown().await.unwrap();
                        let mut received = Vec::with_capacity(size);
                        recv.read_to_end(&mut received).await.unwrap();
                        assert_eq!(received, payload);
                    });
                    futures_util::future::join_all(operations).await;
                });
            },
        );
        runtime.block_on(async {
            connection.close("benchmark done").await.unwrap();
            connection.wait_closed().await;
        });
    }
    group.finish();
}
criterion_group!(benches, bench_stream_roundtrip);
criterion_main!(benches);
