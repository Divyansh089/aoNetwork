# NET-04: Frame wire format version 1

Status: proposed for maintainer review.

## Layout

Every frame starts with a version byte (`1`) and a type byte. Fixed-width
integers are big-endian. Lengths are unsigned LEB128.

| Type | Byte | Body |
|---|---|---|
| `Hello` | `0x01` | `proto_min` u8, `proto_max` u8, `caps` u32, `max_frame` u32, `tag` 16 bytes, `app_hello` length, `app_hello` |
| `Data` | `0x02` | channel u8, payload length, payload |
| `Ping` | `0x03` | empty |
| `Pong` | `0x04` | empty |
| `Bye` | `0x05` | empty |
| `Credit` | `0x06` | `credits` u32 (reserved for NET-25) |

Channel bytes: `Control` 0, `Message` 1, `Telemetry` 2, `Bulk` 3.

## Decode rules

Checked in this order; each failure is a `FrameError`, convertible to
`NetError::InvalidFrame`.

1. Input longer than the receiver's `max_frame` is `Oversize`.
2. Version other than `1` is `UnsupportedVersion`; unknown type byte is
   `UnknownType`; channel byte above 3 is `UnknownChannel`.
3. A length must be canonical LEB128 (no trailing zero group, at most
   64 bits), else `InvalidVarint`. An `app_hello` length above 256 is
   `AppHelloTooLong`, checked before its bytes are read.
4. Any field or declared length running past the input is `Truncated`.
   Nothing is allocated until the bytes are known to be present.
5. Bytes after a complete frame are `TrailingBytes`.

Every accepted input re-encodes to the same bytes. Values in `Hello`
(including `proto_min > proto_max` and unknown `caps` bits) are carried
unchanged; the handshake (NET-10) decides whether they are acceptable.
Layout changes require a new frame version and a compatibility note.
