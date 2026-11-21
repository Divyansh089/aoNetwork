//! Codec inputs, shared by `codec.rs` and `instructions.rs`.

use ao_network::{
    AppHello, Beacon, BeaconFlags, Channel, FRAME_VERSION, Frame, Hello, LocalTag, MAX_APP_HELLO,
    NetworkConfig,
};

/// The largest payload whose `Data` frame fits the default `max_frame`.
pub const LARGEST: usize = 16 * 1024 - 5;

/// `Data` payload sizes: a small control message, a typical message and
/// the largest default frame.
pub const DATA_LENS: [usize; 3] = [16, 1024, LARGEST];

pub fn max_frame() -> usize {
    NetworkConfig::default().max_frame()
}

pub fn beacon() -> Beacon {
    Beacon {
        flags: BeaconFlags {
            accepting: true,
            relay_willing: true,
            low_battery: false,
        },
        degree: 3,
        max_links: 5,
        hint: [0xA5; 8],
        digest: [0x5A; 4],
    }
}

pub fn data(len: usize) -> Frame {
    Frame::Data {
        channel: Channel::Message,
        payload: vec![0xD7; len],
    }
}

/// A `Hello` with the largest `app_hello`, as sent once per link.
pub fn hello() -> Frame {
    Frame::Hello(Hello {
        proto_min: FRAME_VERSION,
        proto_max: FRAME_VERSION,
        caps: 0,
        max_frame: 16 * 1024,
        tag: LocalTag::new([0x7E; 16]),
        app_hello: AppHello::new(vec![0x3C; MAX_APP_HELLO]).expect("within the limit"),
    })
}
