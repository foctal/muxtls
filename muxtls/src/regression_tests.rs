use super::*;
use std::future::Future;
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn limits() -> Limits {
    Limits {
        max_frame_size: 64,
        max_open_streams: 8,
        max_inbound_connection_bytes: 96,
        max_inbound_stream_bytes: 32,
        max_outbound_connection_bytes: 256,
        max_outbound_stream_bytes: 64,
        drain_timeout: Duration::from_millis(25),
        ..Limits::default()
    }
}

fn pair() -> (Connection, Connection) {
    let (a, b) = tokio::io::duplex(128);
    (
        Connection::new(a, limits(), true, None, None, None).unwrap(),
        Connection::new(b, limits(), false, None, None, None).unwrap(),
    )
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("state transition");
}

#[tokio::test]
async fn stalled_stream_does_not_starve_two_consumers() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (a, b) = pair();
        let (a_send, _a_recv) = a.open_bi().await.unwrap();
        let (_b_send, b_stalled) = b.accept_bi().await.unwrap();
        a_send.write_chunk(Bytes::from(vec![7; 48])).await.unwrap();
        until(|| {
            b_stalled
                .state
                .receive_window
                .received
                .load(Ordering::Acquire)
                == 32
        })
        .await;
        assert_eq!(b_stalled.state.inbound_stream_bytes.available_permits(), 0);
        let (s1, _r1) = a.open_bi().await.unwrap();
        let (_s1, r1) = b.accept_bi().await.unwrap();
        let (s2, _r2) = a.open_bi().await.unwrap();
        let (_s2, r2) = b.accept_bi().await.unwrap();
        for index in 0..64u8 {
            s1.write_chunk(Bytes::from(vec![index; 32])).await.unwrap();
            s2.write_chunk(Bytes::from(vec![index; 32])).await.unwrap();
            assert_eq!(
                r1.read_chunk().await.unwrap().unwrap().as_ref(),
                &[index; 32]
            );
            assert_eq!(
                r2.read_chunk().await.unwrap().unwrap().as_ref(),
                &[index; 32]
            );
        }
        assert!(!a.is_closed() && !b.is_closed());
        assert_eq!(
            b_stalled
                .state
                .receive_window
                .received
                .load(Ordering::Acquire),
            32
        );
        let first = b_stalled.read_chunk().await.unwrap().unwrap();
        let second = b_stalled.read_chunk().await.unwrap().unwrap();
        assert_eq!(first.len() + second.len(), 48);
        a.abort();
        b.abort();
        tokio::join!(a.wait_closed(), b.wait_closed());
    })
    .await
    .expect("fairness and resumed flow");
}

#[tokio::test]
async fn partial_reads_retain_budget_and_mixed_apis_are_rejected() {
    let (a, b) = pair();
    let (mut send, _recv) = a.open_bi().await.unwrap();
    let (_send, mut recv) = b.accept_bi().await.unwrap();
    send.write_all(&[9; 32]).await.unwrap();
    send.flush().await.unwrap();
    let mut byte = [0];
    recv.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, [9]);
    assert_eq!(recv.state.inbound_stream_bytes.available_permits(), 1);
    assert!(recv.read_chunk().await.is_err());
    assert!(
        send.write_chunk(Bytes::from_static(b"must not overtake"))
            .await
            .is_err()
    );
    assert!(send.finish().await.is_err());
    let mut rest = [0; 31];
    recv.read_exact(&mut rest).await.unwrap();
    assert_eq!(rest, [9; 31]);
    assert_eq!(recv.state.inbound_stream_bytes.available_permits(), 32);
    send.shutdown().await.unwrap();
    assert_eq!(recv.read(&mut byte).await.unwrap(), 0);
    a.abort();
    b.abort();
    tokio::join!(a.wait_closed(), b.wait_closed());
}

#[tokio::test]
async fn reset_drops_unscheduled_data_without_reordering_or_credit_leak() {
    let (a, b) = pair();
    let (send, _recv) = a.open_bi().await.unwrap();
    let (_send, recv) = b.accept_bi().await.unwrap();
    send.write_chunk(Bytes::from(vec![8; 48])).await.unwrap();
    until(|| recv.state.receive_window.received.load(Ordering::Acquire) == 32).await;
    send.reset(42).await.unwrap();
    until(|| recv.state.recv_terminal.load(Ordering::Acquire)).await;
    assert!(matches!(
        recv.read_chunk().await,
        Err(Error::StreamReset(42))
    ));
    assert_eq!(b.shared.inbound_conn_bytes.available_permits(), 96);
    assert!(!a.is_closed() && !b.is_closed());
    a.abort();
    b.abort();
    tokio::join!(a.wait_closed(), b.wait_closed());
}

#[tokio::test]
async fn cancelled_close_and_reset_do_not_publish_partial_transitions() {
    let (io, _peer) = tokio::io::duplex(1);
    let conn = Connection::new(io, limits(), true, None, None, None).unwrap();
    let (send, _recv) = conn.open_bi().await.unwrap();
    let guard = conn.shared.writer.queues.lock().await;
    let mut cx = Context::from_waker(Waker::noop());
    {
        let mut close = Box::pin(conn.close("cancelled"));
        assert!(close.as_mut().poll(&mut cx).is_pending());
        let mut reset = Box::pin(send.reset(42));
        assert!(reset.as_mut().poll(&mut cx).is_pending());
    }
    assert!(!conn.is_closed());
    assert!(!send.state.send_terminal.load(Ordering::Acquire));
    drop(guard);
    send.reset(43).await.unwrap();
    conn.close("retry").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), conn.wait_closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn finite_close_joins_io_and_wakes_every_waiter() {
    let (io, mut peer) = tokio::io::duplex(1);
    let mut config = limits();
    config.max_open_streams = 1;
    let conn = Connection::new(io, config, true, None, None, None).unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    send.write_chunk(Bytes::from(vec![1; 48])).await.unwrap();
    let mut write = Box::pin(send.write_chunk(Bytes::from(vec![2; 48])));
    let mut read = Box::pin(recv.read_chunk());
    let mut open = Box::pin(conn.open_bi());
    let mut accept = Box::pin(conn.accept_bi());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(write.as_mut().poll(&mut cx).is_pending());
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(open.as_mut().poll(&mut cx).is_pending());
    assert!(accept.as_mut().poll(&mut cx).is_pending());
    conn.close("bounded").await.unwrap();
    conn.close("repeated").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), conn.wait_closed())
        .await
        .unwrap();
    assert!(matches!(write.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    assert!(matches!(read.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    assert!(matches!(open.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    assert!(matches!(accept.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    // EOF proves both halves of the owned transport have been dropped.
    let mut remaining = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut remaining))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn last_connection_drop_uses_the_same_finite_drain_policy() {
    let (io, mut peer) = tokio::io::duplex(1);
    let conn = Connection::new(io, limits(), true, None, None, None).unwrap();
    let weak = Arc::downgrade(&conn.shared);
    drop(conn);
    until(|| weak.upgrade().is_none()).await;
    let mut remaining = Vec::new();
    peer.read_to_end(&mut remaining).await.unwrap();
}

#[tokio::test]
async fn duplicate_stale_and_hostile_credit_updates_are_bounded() {
    let (io, _peer) = tokio::io::duplex(1);
    let conn = Connection::new(io, limits(), true, None, None, None).unwrap();
    let (send, _recv) = conn.open_bi().await.unwrap();
    async fn apply(conn: &Connection, frame: Frame) -> Result<bool> {
        let mut data = BytesMut::new();
        frame.encode(&mut data).unwrap();
        handle_incoming_frame(conn.shared.clone(), data).await
    }
    let number = |n| VarInt::from_u64(n).unwrap();
    let settings = Frame::Settings {
        max_data: number(0),
        max_stream_data: number(0),
        max_frame_size: number(64),
    };
    apply(&conn, settings.clone()).await.unwrap();
    assert!(apply(&conn, settings).await.is_err());
    for n in [VarInt::MAX, VarInt::MAX, 0, 1] {
        apply(&conn, Frame::MaxData { maximum: number(n) })
            .await
            .unwrap();
        apply(
            &conn,
            Frame::MaxStreamData {
                stream_id: number(send.id()),
                maximum: number(n),
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        conn.shared.writer.queues.lock().await.peer_limit,
        VarInt::MAX
    );
    assert_eq!(send.state.peer_limit.load(Ordering::Acquire), VarInt::MAX);
    assert!(
        apply(
            &conn,
            Frame::MaxStreamData {
                stream_id: number(2),
                maximum: number(1)
            }
        )
        .await
        .is_err()
    );
    assert!(conn.shared.writer.queues.lock().await.by_stream.is_empty());
    conn.abort();
    conn.wait_closed().await;
}

#[tokio::test]
async fn dropping_receiver_stops_a_credit_blocked_sender() {
    let (a, b) = pair();
    let (send, _recv) = a.open_bi().await.unwrap();
    let (_send, recv) = b.accept_bi().await.unwrap();
    send.write_chunk(Bytes::from(vec![5; 48])).await.unwrap();
    until(|| recv.state.receive_window.received.load(Ordering::Acquire) == 32).await;
    let mut blocked = Box::pin(send.write_chunk(Bytes::from(vec![6; 48])));
    assert!(
        blocked
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(recv);
    tokio::time::timeout(Duration::from_secs(2), blocked)
        .await
        .unwrap()
        .expect_err("STOP_SENDING must wake blocked writes");
    assert!(!a.is_closed() && !b.is_closed());
    a.abort();
    b.abort();
    tokio::join!(a.wait_closed(), b.wait_closed());
}

#[tokio::test]
async fn simultaneous_close_and_abort_release_transport_resources() {
    let (a, b) = pair();
    let (x, y) = tokio::join!(a.close("a"), b.close("b"));
    x.unwrap();
    y.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(a.wait_closed(), b.wait_closed());
    })
    .await
    .unwrap();
    a.abort();
    b.abort();
    tokio::join!(a.wait_closed(), b.wait_closed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit lifecycle stress campaign"]
async fn repeated_flow_control_cancellation_and_shutdown_stress() {
    let start = std::time::Instant::now();
    let mut frames = 0;
    for cycle in 0..20_000 {
        let (a, b) = pair();
        let weak_a = Arc::downgrade(&a.shared);
        let weak_b = Arc::downgrade(&b.shared);
        let (mut send, _recv) = a.open_bi().await.unwrap();
        let (_send, mut recv) = b.accept_bi().await.unwrap();
        let payload = vec![(cycle % 251) as u8; 4096];
        tokio::time::timeout(Duration::from_secs(2), async {
            let ((), received) = tokio::join!(
                async {
                    send.write_all(&payload).await.unwrap();
                    send.shutdown().await.unwrap();
                },
                async {
                    let mut data = Vec::new();
                    recv.read_to_end(&mut data).await.unwrap();
                    data
                }
            );
            assert_eq!(received, payload);
        })
        .await
        .unwrap();
        frames += a.stats().frames_sent + b.stats().frames_sent;
        a.abort();
        b.abort();
        tokio::join!(a.wait_closed(), b.wait_closed());
        assert!(a.shared.streams.lock().await.is_empty());
        assert!(b.shared.streams.lock().await.is_empty());
        assert_eq!(
            a.shared.outbound_frames.available_permits(),
            a.shared.limits.max_queued_outbound_frames
        );
        drop((send, recv, _send, _recv, a, b));
        until(|| weak_a.upgrade().is_none() && weak_b.upgrade().is_none()).await;
    }
    eprintln!(
        "STRESS cycles=20000 payload_bytes={} frames={frames} elapsed={:?}",
        20_000 * 4096,
        start.elapsed()
    );
}

#[tokio::test]
async fn one_stream_cannot_occupy_all_outbound_frame_slots() {
    let (io, _peer) = tokio::io::duplex(1);
    let config = Limits {
        max_open_streams: 4,
        max_queued_outbound_frames: 8,
        ..limits()
    };
    let conn = Connection::new(io, config, true, None, None, None).unwrap();
    let (a, _a_recv) = conn.open_bi().await.unwrap();
    let (b, _b_recv) = conn.open_bi().await.unwrap();
    a.write_chunk(Bytes::from_static(b"a")).await.unwrap();
    a.write_chunk(Bytes::from_static(b"a")).await.unwrap();
    let mut blocked = Box::pin(a.write_chunk(Bytes::from_static(b"a")));
    assert!(
        blocked
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    b.write_chunk(Bytes::from_static(b"b")).await.unwrap();
    assert_eq!(conn.shared.outbound_frames.available_permits(), 5);
    conn.abort();
    conn.wait_closed().await;
}

#[tokio::test]
async fn exhausted_connection_credit_resumes_below_batch_threshold() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (a, b) = pair();
        let mut streams = Vec::new();
        for _ in 0..3 {
            let (send, local_recv) = a.open_bi().await.unwrap();
            let (remote_send, recv) = b.accept_bi().await.unwrap();
            send.write_chunk(Bytes::from(vec![1; 32])).await.unwrap();
            streams.push((send, local_recv, remote_send, recv));
        }
        until(|| b.shared.receive_window.received.load(Ordering::Acquire) == 96).await;
        let (send, _, _, recv) = &streams[0];
        assert_eq!(recv.read_chunk().await.unwrap().unwrap().len(), 32);
        // Only one of three streams consumes: 32 bytes is below the 48-byte
        // connection batching threshold. Exhaustion must still return credit.
        send.write_chunk(Bytes::from_static(b"progress"))
            .await
            .unwrap();
        assert_eq!(
            recv.read_chunk().await.unwrap().unwrap().as_ref(),
            b"progress"
        );
        a.abort();
        b.abort();
        tokio::join!(a.wait_closed(), b.wait_closed());
    })
    .await
    .expect("small active stream must not deadlock behind stalled streams");
}

#[tokio::test]
async fn tiny_peer_frames_coalesce_without_unbounded_queue_metadata() {
    let (io, _peer) = tokio::io::duplex(1);
    let config = Limits {
        max_inbound_stream_bytes: 4096,
        max_inbound_connection_bytes: 8192,
        ..limits()
    };
    let conn = Connection::new(io, config, true, None, None, None).unwrap();
    conn.shared.on_remote_open(1).await.unwrap();
    let (_send, recv) = conn.accept_bi().await.unwrap();
    for _ in 0..1000 {
        conn.shared
            .on_remote_stream_frame(1, Bytes::from_static(b"x"), false)
            .await
            .unwrap();
    }
    assert_eq!(recv.state.inbound.lock().await.chunks.len(), 1);
    assert_eq!(
        recv.read_chunk().await.unwrap().unwrap(),
        Bytes::from(vec![b'x'; 1000])
    );
    assert_eq!(conn.shared.inbound_conn_bytes.available_permits(), 8192);
    assert_eq!(recv.state.inbound_stream_bytes.available_permits(), 4096);
    conn.abort();
    conn.wait_closed().await;
}

#[tokio::test]
async fn connection_and_stream_receive_limits_are_independently_enforced() {
    let (io, _peer) = tokio::io::duplex(1);
    let conn = Connection::new(io, limits(), true, None, None, None).unwrap();
    for id in [1, 3, 5, 7] {
        conn.shared.on_remote_open(id).await.unwrap();
    }
    for id in [1, 3, 5] {
        conn.shared
            .on_remote_stream_frame(id, Bytes::from(vec![1; 32]), false)
            .await
            .unwrap();
    }
    assert!(matches!(
        conn.shared
            .on_remote_stream_frame(7, Bytes::from_static(b"x"), false)
            .await,
        Err(Error::Protocol(_))
    ));
    conn.abort();
    conn.wait_closed().await;
    let (io, _peer) = tokio::io::duplex(1);
    let conn = Connection::new(io, limits(), true, None, None, None).unwrap();
    conn.shared.on_remote_open(1).await.unwrap();
    assert!(matches!(
        conn.shared
            .on_remote_stream_frame(1, Bytes::from(vec![1; 33]), false)
            .await,
        Err(Error::Protocol(_))
    ));
    conn.abort();
    conn.wait_closed().await;
}

#[tokio::test]
async fn graceful_peer_close_preserves_unread_data_and_fin() {
    let (a, b) = pair();
    let (send, _recv) = a.open_bi().await.unwrap();
    let (_send, recv) = b.accept_bi().await.unwrap();
    send.write_chunk(Bytes::from_static(b"accepted before close"))
        .await
        .unwrap();
    send.finish().await.unwrap();
    a.close("done").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(a.wait_closed(), b.wait_closed());
    })
    .await
    .unwrap();
    assert_eq!(
        recv.read_chunk().await.unwrap().unwrap().as_ref(),
        b"accepted before close"
    );
    assert!(recv.read_chunk().await.unwrap().is_none());
}
