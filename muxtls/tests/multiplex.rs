use std::time::Duration;

use bytes::Bytes;
use muxtls::{ClientConfig, Endpoint, Limits, ServerConfig};
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("muxtls=debug".parse().expect("valid filter")),
        )
        .with_test_writer()
        .try_init();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiplexed_streams_are_isolated_and_ordered() {
    init_tracing();
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");

    let limits = Limits {
        max_frame_size: 16 * 1024,
        max_open_streams: 64,
        max_inbound_connection_bytes: 1024 * 1024,
        max_outbound_connection_bytes: 1024 * 1024,
        max_inbound_stream_bytes: 128 * 1024,
        max_outbound_stream_bytes: 128 * 1024,
    };

    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_limits(limits.clone());
    let addr = server.local_addr().expect("server local addr");

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept connection");
        loop {
            let (send, recv) = match conn.accept_bi().await {
                Ok(streams) => streams,
                Err(_) => break,
            };
            tokio::spawn(async move {
                while let Some(chunk) = recv.read_chunk().await.expect("recv chunk") {
                    send.write_chunk(chunk).await.expect("echo write");
                }
                send.finish().await.expect("send finish");
            });
        }
    });

    let client_cfg = ClientConfig::with_custom_roots(vec![cert]).expect("client roots");
    let client = Endpoint::client(client_cfg).with_limits(limits);
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect ok");

    let mut tasks = Vec::new();
    for stream_idx in 0..8u8 {
        let (send, recv) = conn.open_bi().await.expect("open stream");
        tasks.push(tokio::spawn(async move {
            let sent = vec![
                Bytes::from(vec![stream_idx; 5]),
                Bytes::from(vec![stream_idx; 7]),
                Bytes::from(vec![stream_idx; 3]),
            ];

            for chunk in &sent {
                send.write_chunk(chunk.clone()).await.expect("write chunk");
            }
            send.finish().await.expect("finish stream");

            let mut received = Vec::new();
            while let Some(chunk) = recv.read_chunk().await.expect("read chunk") {
                received.push(chunk);
            }

            assert_eq!(sent, received, "stream payload ordering mismatch");
        }));
    }

    for task in tasks {
        timeout(Duration::from_secs(5), task)
            .await
            .expect("stream task timeout")
            .expect("stream task join");
    }

    conn.close("done").await.expect("close connection");
    let _ = timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_frame_is_rejected() {
    init_tracing();
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");

    let limits = Limits {
        max_frame_size: 8,
        max_open_streams: 8,
        max_inbound_connection_bytes: 1024,
        max_outbound_connection_bytes: 1024,
        max_inbound_stream_bytes: 256,
        max_outbound_stream_bytes: 256,
    };

    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_limits(limits.clone());
    let addr = server.local_addr().expect("server local addr");

    let server_task = tokio::spawn(async move {
        let _ = server.accept().await;
    });

    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"))
            .with_limits(limits);
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect ok");

    let (send, _recv) = conn.open_bi().await.expect("open stream");
    let err = send
        .write_chunk(Bytes::from_static(b"012345678"))
        .await
        .expect_err("oversized chunk must fail");

    let message = err.to_string();
    assert!(
        message.contains("payload size") || message.contains("limit"),
        "unexpected error message: {message}"
    );

    let _ = conn.close("done").await;
    let _ = timeout(Duration::from_secs(2), server_task).await;
}
