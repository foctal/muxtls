#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use muxtls::fuzz_connection_state;
use muxtls_proto::{ErrorCode, Frame, VarInt};

#[derive(Arbitrary, Debug)]
struct Scenario {
    is_client: bool,
    actions: Vec<Action>,
}

#[derive(Arbitrary, Debug)]
enum Action {
    Settings {
        data: u64,
        stream: u64,
        frame: u64,
    },
    MaxData {
        maximum: u64,
    },
    MaxStreamData {
        index: u8,
        maximum: u64,
    },
    StopSending {
        index: u8,
    },
    OpenNext,
    Open {
        stream_id: u64,
    },
    StreamKnown {
        index: u8,
        fin: bool,
        payload: Vec<u8>,
    },
    Stream {
        stream_id: u64,
        fin: bool,
        payload: Vec<u8>,
    },
    ResetKnown {
        index: u8,
        error_code: u64,
    },
    Reset {
        stream_id: u64,
        error_code: u64,
    },
    Ping,
    Close {
        error_code: u64,
        reason: Vec<u8>,
    },
    Raw {
        bytes: Vec<u8>,
    },
}

fuzz_target!(|scenario: Scenario| {
    let mut next_remote_id = if scenario.is_client { 1 } else { 0 };
    let mut opened = Vec::new();
    let mut encoded_frames = vec![encode(Frame::Settings {
        max_data: varint(4096),
        max_stream_data: varint(512),
        max_frame_size: varint(1024),
    })];

    for action in scenario.actions.into_iter().take(64) {
        let encoded = match action {
            Action::Settings {
                data,
                stream,
                frame,
            } => encode(Frame::Settings {
                max_data: varint(bounded(data)),
                max_stream_data: varint(bounded(stream)),
                max_frame_size: varint(bounded(frame)),
            }),
            Action::MaxData { maximum } => encode(Frame::MaxData {
                maximum: varint(bounded(maximum)),
            }),
            Action::MaxStreamData { index, maximum } => encode(Frame::MaxStreamData {
                stream_id: varint(select(&opened, index).copied().unwrap_or(VarInt::MAX)),
                maximum: varint(bounded(maximum)),
            }),
            Action::StopSending { index } => encode(Frame::StopSending {
                stream_id: varint(select(&opened, index).copied().unwrap_or(VarInt::MAX)),
            }),
            Action::OpenNext => {
                if next_remote_id > VarInt::MAX {
                    continue;
                }
                let stream_id = next_remote_id;
                next_remote_id += 2;
                opened.push(stream_id);
                encode(Frame::OpenStream {
                    stream_id: varint(stream_id),
                })
            }
            Action::Open { stream_id } => {
                let stream_id = bounded(stream_id);
                opened.push(stream_id);
                encode(Frame::OpenStream {
                    stream_id: varint(stream_id),
                })
            }
            Action::StreamKnown {
                index,
                fin,
                payload,
            } => {
                let Some(&stream_id) = select(&opened, index) else {
                    continue;
                };
                encode(Frame::Stream {
                    stream_id: varint(stream_id),
                    fin,
                    payload: payload.into_iter().take(256).collect::<Vec<_>>().into(),
                })
            }
            Action::Stream {
                stream_id,
                fin,
                payload,
            } => encode(Frame::Stream {
                stream_id: varint(bounded(stream_id)),
                fin,
                payload: payload.into_iter().take(256).collect::<Vec<_>>().into(),
            }),
            Action::ResetKnown { index, error_code } => {
                let Some(&stream_id) = select(&opened, index) else {
                    continue;
                };
                encode(Frame::ResetStream {
                    stream_id: varint(stream_id),
                    error_code: proto_error_code(error_code),
                })
            }
            Action::Reset {
                stream_id,
                error_code: code,
            } => encode(Frame::ResetStream {
                stream_id: varint(bounded(stream_id)),
                error_code: proto_error_code(code),
            }),
            Action::Ping => encode(Frame::Ping),
            Action::Close { error_code, reason } => encode(Frame::ConnectionClose {
                error_code: proto_error_code(error_code),
                reason: String::from_utf8_lossy(&reason.into_iter().take(128).collect::<Vec<_>>())
                    .into_owned(),
            }),
            Action::Raw { bytes } => bytes.into_iter().take(512).collect(),
        };
        encoded_frames.push(encoded);
    }

    fuzz_connection_state(scenario.is_client, &encoded_frames);
});

fn bounded(value: u64) -> u64 {
    value % (VarInt::MAX + 1)
}

fn varint(value: u64) -> VarInt {
    VarInt::from_u64(value).expect("bounded value")
}

fn proto_error_code(value: u64) -> ErrorCode {
    ErrorCode::from_u64(bounded(value)).expect("bounded error code")
}

fn select(streams: &[u64], index: u8) -> Option<&u64> {
    if streams.is_empty() {
        None
    } else {
        streams.get(index as usize % streams.len())
    }
}

fn encode(frame: Frame) -> Vec<u8> {
    let mut encoded = BytesMut::new();
    frame.encode(&mut encoded).expect("generated frame");
    encoded.to_vec()
}
