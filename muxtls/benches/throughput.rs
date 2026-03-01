use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use muxtls::{ClientConfig, Connection, Endpoint, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;

async fn setup_connection() -> Connection {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server");
    let addr = server.local_addr().expect("server local address");

    tokio::spawn(async move {
        let conn = server.accept().await.expect("accept connection");
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut recv, &mut send).await;
                let _ = send.shutdown().await;
            });
        }
    });

    let client_cfg = ClientConfig::with_custom_roots(vec![cert]).expect("client roots");
    let client = Endpoint::client(client_cfg);
    client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect")
}

fn bench_stream_roundtrip(c: &mut Criterion) {
    let runtime = Arc::new(Runtime::new().expect("tokio runtime"));
    let connection = runtime.block_on(setup_connection());
    let payload = vec![42u8; 16 * 1024];

    c.bench_function("stream_roundtrip_16k", |b| {
        let runtime = runtime.clone();
        let conn = connection.clone();
        b.to_async(runtime.as_ref()).iter(|| async {
            let (mut send, mut recv) = conn.open_bi().await.expect("open stream");

            send.write_all(&payload).await.expect("write payload");
            send.shutdown().await.expect("finish send");

            let mut received = Vec::with_capacity(payload.len());
            recv.read_to_end(&mut received).await.expect("read echo");
            assert_eq!(received.len(), payload.len(), "echo size mismatch");
        });
    });
}

criterion_group!(benches, bench_stream_roundtrip);
criterion_main!(benches);
