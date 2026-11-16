//! Versioned wire codecs. Radio framing is handled by the transport adapter.

mod beacon;
mod frame;
mod varint;

pub use beacon::{BEACON_LEN, BEACON_VERSION, Beacon, BeaconFlags};
pub use frame::{AppHello, FRAME_VERSION, Frame, Hello, MAX_APP_HELLO};
