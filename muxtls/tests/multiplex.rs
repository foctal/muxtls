use std::time::Duration;

use bytes::Bytes;
use muxtls::{ClientConfig, Endpoint, Limits, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        max_frame_size: 32,
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
        .write_chunk(Bytes::from(vec![0; 29]))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_write_splits_buffers_at_frame_boundaries() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let limits = Limits {
        max_frame_size: 64,
        max_open_streams: 8,
        max_inbound_connection_bytes: 16 * 1024,
        max_outbound_connection_bytes: 16 * 1024,
        max_inbound_stream_bytes: 8 * 1024,
        max_outbound_stream_bytes: 8 * 1024,
    };
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_limits(limits.clone());
    let addr = server.local_addr().expect("server address");

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        let (mut send, mut recv) = conn.accept_bi().await.expect("accept stream");
        tokio::io::copy(&mut recv, &mut send).await.expect("echo");
        send.shutdown().await.expect("shutdown echo");
    });

    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"))
            .with_limits(limits);
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("open stream");
    let payload = vec![0x5a; 4096];

    send.write_all(&payload).await.expect("write all");
    send.shutdown().await.expect("shutdown");
    send.shutdown()
        .await
        .expect("repeated shutdown is idempotent");

    let mut received = Vec::new();
    recv.read_to_end(&mut received).await.expect("read echo");
    assert_eq!(received, payload);

    conn.close("done").await.expect("close");
    timeout(Duration::from_secs(2), conn.wait_closed())
        .await
        .expect("connection close timeout");
    server_task.await.expect("server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_limits_fail_before_connecting() {
    let limits = Limits {
        max_open_streams: 0,
        ..Limits::default()
    };
    let endpoint = Endpoint::client(ClientConfig::dangerous_insecure_no_verify_for_testing())
        .with_limits(limits);
    let addr = "127.0.0.1:9".parse().expect("socket address");

    let error = match endpoint.connect(addr, "localhost") {
        Ok(_) => panic!("invalid limits must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        muxtls::Error::InvalidLimit {
            field: "max_open_streams",
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_tls_handshake_times_out() {
    let (server_cfg, _) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_handshake_timeout(Duration::from_millis(50));
    let addr = server.local_addr().expect("server address");

    let accept_task = tokio::spawn(async move { server.accept().await });
    let _tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("raw TCP connect");
    let result = timeout(Duration::from_secs(1), accept_task)
        .await
        .expect("accept did not observe handshake timeout")
        .expect("accept task");
    let error = match result {
        Ok(_) => panic!("non-TLS client must time out"),
        Err(error) => error,
    };
    assert!(matches!(error, muxtls::Error::Timeout("TLS handshake")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_before_stream_data_is_delivered() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server");
    let addr = server.local_addr().expect("server address");

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        let (_send, recv) = conn.accept_bi().await.expect("accept reset stream");
        let error = recv
            .read_chunk()
            .await
            .expect_err("reset must reach receiver");
        assert!(matches!(error, muxtls::Error::StreamReset(42)));
    });

    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"));
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect");
    let (send, _recv) = conn.open_bi().await.expect("open stream");
    send.reset(42).await.expect("reset stream");

    timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server timeout")
        .expect("server task");
    conn.close("done").await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_rejects_tls_without_muxtls_alpn() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server");
    let addr = server.local_addr().expect("server address");
    let accept_task = tokio::spawn(async move { server.accept().await });

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).expect("add test root");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("TCP connect");
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let _tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(server_name, tcp)
        .await
        .expect("base TLS handshake");

    let result = accept_task.await.expect("accept task");
    let error = match result {
        Ok(_) => panic!("missing ALPN must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, muxtls::Error::TlsHandshake(message) if message.contains("ALPN")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_last_connection_handle_closes_the_peer() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server");
    let addr = server.local_addr().expect("server address");
    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        timeout(Duration::from_secs(2), conn.wait_closed())
            .await
            .expect("peer drop did not close connection");
        assert!(conn.is_closed());
    });

    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"));
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect");
    drop(conn);

    server_task.await.expect("server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_buffer_violation_closes_instead_of_blocking_reader() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server_limits = Limits {
        max_frame_size: 64,
        max_open_streams: 8,
        max_inbound_connection_bytes: 1024,
        max_outbound_connection_bytes: 1024,
        max_inbound_stream_bytes: 32,
        max_outbound_stream_bytes: 1024,
    };
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_limits(server_limits);
    let addr = server.local_addr().expect("server address");
    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        timeout(Duration::from_secs(2), conn.wait_closed())
            .await
            .expect("inbound limit did not close connection");
    });

    let client_limits = Limits {
        max_frame_size: 64,
        max_open_streams: 8,
        max_inbound_connection_bytes: 1024,
        max_outbound_connection_bytes: 1024,
        max_inbound_stream_bytes: 1024,
        max_outbound_stream_bytes: 1024,
    };
    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"))
            .with_limits(client_limits);
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect");
    let (send, _recv) = conn.open_bi().await.expect("open stream");
    send.write_chunk(Bytes::from(vec![0; 40]))
        .await
        .expect("queue oversized-for-peer chunk");

    timeout(Duration::from_secs(2), conn.wait_closed())
        .await
        .expect("client did not observe protocol close");
    server_task.await.expect("server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keepalive_frames_prevent_idle_timeout() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let keepalive = Duration::from_millis(20);
    let idle_timeout = Duration::from_millis(100);
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_keepalive_interval(keepalive)
        .with_idle_timeout(idle_timeout);
    let addr = server.local_addr().expect("server address");
    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        tokio::time::sleep(Duration::from_millis(180)).await;
        assert!(!conn.is_closed());
        assert!(conn.stats().frames_received > 0);
        conn.wait_closed().await;
    });

    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"))
            .with_keepalive_interval(keepalive)
            .with_idle_timeout(idle_timeout);
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect");

    tokio::time::sleep(Duration::from_millis(180)).await;
    assert!(!conn.is_closed());
    let stats = conn.stats();
    assert!(stats.frames_sent > 0);
    assert!(stats.frames_received > 0);

    conn.close("done").await.expect("close");
    timeout(Duration::from_secs(2), server_task)
        .await
        .expect("server timeout")
        .expect("server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_timeout_closes_a_silent_connection() {
    let (server_cfg, cert) = ServerConfig::self_signed_for_localhost().expect("self-signed cert");
    let server = Endpoint::server("127.0.0.1:0", server_cfg)
        .await
        .expect("bind server")
        .with_idle_timeout(Duration::from_millis(50));
    let addr = server.local_addr().expect("server address");
    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        timeout(Duration::from_secs(2), conn.wait_closed())
            .await
            .expect("server idle timeout did not fire");
        assert!(conn.is_closed());
    });

    let client =
        Endpoint::client(ClientConfig::with_custom_roots(vec![cert]).expect("client config"));
    let conn = client
        .connect(addr, "localhost")
        .expect("start connect")
        .await
        .expect("connect");

    timeout(Duration::from_secs(2), conn.wait_closed())
        .await
        .expect("client did not observe idle close");
    assert!(conn.is_closed());
    server_task.await.expect("server task");
}

#[tokio::test]
async fn zero_connection_policy_durations_are_rejected() {
    let endpoint = Endpoint::client(ClientConfig::dangerous_insecure_no_verify_for_testing())
        .with_keepalive_interval(Duration::ZERO);
    let addr = "127.0.0.1:9".parse().expect("socket address");
    let error = match endpoint.connect(addr, "localhost") {
        Ok(_) => panic!("zero keepalive interval must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, muxtls::Error::Config(message) if message.contains("keepalive")));

    let endpoint = Endpoint::client(ClientConfig::dangerous_insecure_no_verify_for_testing())
        .with_idle_timeout(Duration::ZERO);
    let error = match endpoint.connect(addr, "localhost") {
        Ok(_) => panic!("zero idle timeout must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, muxtls::Error::Config(message) if message.contains("idle")));
}
