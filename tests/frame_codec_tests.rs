//! Public-API tests for the frame codec: wire fixtures and every error path.

use ao_network::{
    AppHello, Channel, FRAME_VERSION, Frame, FrameError, Hello, LocalTag, MAX_APP_HELLO, NetError,
};

const MAX: usize = 16 * 1024;

fn hello() -> Hello {
    Hello {
        proto_min: 1,
        proto_max: 2,
        caps: 0x0102_0304,
        max_frame: 0x0000_4000,
        tag: LocalTag::new([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ]),
        app_hello: AppHello::new(vec![0xDE, 0xAD]).expect("within bound"),
    }
}

// Independent fixture: version, type, proto 1..2, caps and max_frame
// big-endian, the 16-byte tag, then a 2-byte app_hello.
const HELLO_WIRE: [u8; 29] = [
    1, 0x01, 1, 2, 0x01, 0x02, 0x03, 0x04, 0x00, 0x00, 0x40, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44,
    0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x02,
];

fn hello_wire() -> Vec<u8> {
    let mut wire = HELLO_WIRE.to_vec();
    wire.extend_from_slice(&[0xDE, 0xAD]);
    wire
}

#[test]
fn hello_matches_the_wire_fixture() {
    let frame = Frame::Hello(hello());
    assert_eq!(frame.encode(), hello_wire());
    assert_eq!(Frame::decode(&hello_wire(), MAX), Ok(frame));
}

#[test]
fn every_channel_has_its_wire_byte() {
    for (channel, byte) in [
        (Channel::Control, 0),
        (Channel::Message, 1),
        (Channel::Telemetry, 2),
        (Channel::Bulk, 3),
    ] {
        let frame = Frame::Data {
            channel,
            payload: vec![0x5A],
        };
        let wire = [FRAME_VERSION, 0x02, byte, 0x01, 0x5A];
        assert_eq!(frame.encode(), wire, "{channel:?}");
        assert_eq!(Frame::decode(&wire, MAX), Ok(frame), "{channel:?}");
    }
}

#[test]
fn data_length_uses_multi_byte_leb128() {
    let frame = Frame::Data {
        channel: Channel::Bulk,
        payload: vec![0xEE; 300],
    };
    let wire = frame.encode();
    // 300 = 0xAC 0x02 in LEB128.
    assert_eq!(&wire[..5], &[FRAME_VERSION, 0x02, 3, 0xAC, 0x02]);
    assert_eq!(wire.len(), 5 + 300);
    assert_eq!(Frame::decode(&wire, MAX), Ok(frame));
}

#[test]
fn empty_bodies_and_credit_match_their_fixtures() {
    let cases: [(Frame, &[u8]); 4] = [
        (Frame::Ping, &[1, 0x03]),
        (Frame::Pong, &[1, 0x04]),
        (Frame::Bye, &[1, 0x05]),
        (
            Frame::Credit {
                credits: 0x0A0B_0C0D,
            },
            &[1, 0x06, 0x0A, 0x0B, 0x0C, 0x0D],
        ),
    ];
    for (frame, wire) in cases {
        assert_eq!(frame.encode(), wire, "{frame:?}");
        assert_eq!(Frame::decode(wire, MAX), Ok(frame));
    }
}

#[test]
fn empty_payload_and_app_hello_are_valid() {
    let data = Frame::Data {
        channel: Channel::Control,
        payload: Vec::new(),
    };
    assert_eq!(data.encode(), [1, 0x02, 0, 0]);
    assert_eq!(Frame::decode(&data.encode(), MAX), Ok(data));

    let mut empty = hello();
    empty.app_hello = AppHello::default();
    let frame = Frame::Hello(empty);
    assert_eq!(Frame::decode(&frame.encode(), MAX), Ok(frame));
}

#[test]
fn app_hello_bound_is_inclusive() {
    let full = AppHello::new(vec![7; MAX_APP_HELLO]).expect("exactly at bound");
    assert_eq!(full.as_bytes().len(), MAX_APP_HELLO);
    assert_eq!(
        AppHello::new(vec![7; MAX_APP_HELLO + 1]),
        Err(FrameError::AppHelloTooLong {
            len: MAX_APP_HELLO + 1,
            max: MAX_APP_HELLO,
        })
    );

    let mut at_bound = hello();
    at_bound.app_hello = full;
    let frame = Frame::Hello(at_bound);
    assert_eq!(Frame::decode(&frame.encode(), MAX), Ok(frame));
}

#[test]
fn oversize_input_is_rejected_before_parsing() {
    let wire = hello_wire();
    assert_eq!(
        Frame::decode(&wire, wire.len() - 1),
        Err(FrameError::Oversize {
            len: wire.len(),
            max: wire.len() - 1,
        })
    );
    assert!(Frame::decode(&wire, wire.len()).is_ok());
    // Size is checked first, even when the bytes are garbage.
    assert_eq!(
        Frame::decode(&[0xFF; 4], 3),
        Err(FrameError::Oversize { len: 4, max: 3 })
    );
}

#[test]
fn empty_input_is_truncated() {
    assert_eq!(Frame::decode(&[], MAX), Err(FrameError::Truncated));
    assert_eq!(Frame::decode(&[1], MAX), Err(FrameError::Truncated));
}

#[test]
fn unsupported_version_is_rejected() {
    for version in [0, 2, 0xFF] {
        assert_eq!(
            Frame::decode(&[version, 0x03], MAX),
            Err(FrameError::UnsupportedVersion { version })
        );
    }
}

#[test]
fn unknown_type_is_rejected() {
    for frame_type in [0x00, 0x07, 0xFF] {
        assert_eq!(
            Frame::decode(&[1, frame_type], MAX),
            Err(FrameError::UnknownType { frame_type })
        );
    }
}

#[test]
fn unknown_channel_is_rejected() {
    assert_eq!(
        Frame::decode(&[1, 0x02, 4, 0], MAX),
        Err(FrameError::UnknownChannel { channel: 4 })
    );
}

#[test]
fn malformed_lengths_are_rejected() {
    // Overlong zero.
    assert_eq!(
        Frame::decode(&[1, 0x02, 0, 0x80, 0x00], MAX),
        Err(FrameError::InvalidVarint)
    );
    // More than 64 bits.
    let mut wire = vec![1, 0x02, 0];
    wire.extend_from_slice(&[0xFF; 9]);
    wire.push(0x02);
    assert_eq!(Frame::decode(&wire, MAX), Err(FrameError::InvalidVarint));
}

#[test]
fn declared_length_beyond_input_is_truncated_without_allocating() {
    // Claims u64::MAX payload bytes in a 13-byte input.
    let mut wire = vec![1, 0x02, 0];
    wire.extend_from_slice(&[0xFF; 9]);
    wire.push(0x01);
    assert_eq!(Frame::decode(&wire, MAX), Err(FrameError::Truncated));
    // Claims 5 bytes, carries 4.
    assert_eq!(
        Frame::decode(&[1, 0x02, 1, 5, 9, 9, 9, 9], MAX),
        Err(FrameError::Truncated)
    );
}

#[test]
fn oversized_app_hello_length_is_rejected_before_reading() {
    let mut wire = HELLO_WIRE.to_vec();
    // Replace the length with 257 (0x81 0x02) and carry no blob at all.
    wire.pop();
    wire.extend_from_slice(&[0x81, 0x02]);
    assert_eq!(
        Frame::decode(&wire, MAX),
        Err(FrameError::AppHelloTooLong {
            len: MAX_APP_HELLO + 1,
            max: MAX_APP_HELLO,
        })
    );
}

#[test]
fn trailing_bytes_are_rejected() {
    assert_eq!(
        Frame::decode(&[1, 0x03, 0x00, 0x00], MAX),
        Err(FrameError::TrailingBytes { extra: 2 })
    );
    let mut wire = hello_wire();
    wire.push(0);
    assert_eq!(
        Frame::decode(&wire, MAX),
        Err(FrameError::TrailingBytes { extra: 1 })
    );
}

#[test]
fn frame_errors_convert_to_net_error() {
    let err = NetError::from(FrameError::Truncated);
    assert_eq!(err, NetError::InvalidFrame(FrameError::Truncated));
    assert!(err.to_string().contains("truncated"), "{err}");
}
