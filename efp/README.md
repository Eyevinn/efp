# efp

Safe, idiomatic Rust wrapper around the [Elastic Frame Protocol](https://github.com/OwnZones/efp) (EFP) C library via [`efp-sys`](../efp-sys/).

EFP fragments large media frames ("superframes") into MTU-sized pieces for transport over UDP or similar channels, and reassembles them on the receiving side.

## Usage

```rust
use std::sync::Arc;
use efp::{Sender, Receiver, ReceiverMode};

// Create a receiver that prints reassembled frames.
let receiver = Arc::new(
    Receiver::new(5, 5, ReceiverMode::RunToCompletion, |frame| {
        println!(
            "received {} bytes, stream={}, pts={}",
            frame.data.len(),
            frame.stream_id,
            frame.pts,
        );
    })
    .expect("failed to create receiver"),
);

// Create a sender that feeds fragments into the receiver.
let rx = Arc::clone(&receiver);
let sender = Sender::new(1400, move |fragment, _stream_id| {
    rx.receive_fragment(fragment, 0).ok();
})
.expect("failed to create sender");

// Send a 5 KB frame.
let payload = vec![0u8; 5000];
sender.send(&payload, 0x01, 1000, 1000, 0, 1, 0).unwrap();
```

## API overview

### Sender

- `Sender::new(mtu, callback)` — Create a sender. MTU must be in the range 256..=65535.
- `sender.send(data, content_type, pts, dts, code, stream_id, flags)` — Fragment and send a superframe.

### Receiver

- `Receiver::new(bucket_timeout, hol_timeout, mode, callback)` — Create a receiver.
- `Receiver::with_embedded(...)` — Create a receiver with an embedded-data callback.
- `receiver.receive_fragment(fragment, from_source)` — Feed a fragment for reassembly.

### Free functions

- `version()` — Returns `(major, minor)` version tuple.
- `code(c0, c1, c2, c3)` — Build a 4-character content code (big-endian).
- `add_embedded_data(embedded, frame_data, data_type, is_last)` — Prepend embedded data to a frame. Returns `Err(TooLargeEmbeddedData)` if embedded exceeds 65535 bytes.

### Content type constants

| Constant | Value | Description |
|---|---|---|
| `CONTENT_PRIVATE_DATA` | `0x01` | Private / unspecified data |
| `CONTENT_H264` | `0x83` | H.264 / AVC video |
| `CONTENT_H265` | `0x84` | H.265 / HEVC video |
| `CONTENT_OPUS` | `0x89` | Opus audio |

### Error handling

All fallible operations return `Result<T, EfpError>`. Error variants map 1:1 to the C library's error codes, plus `InitFailed` (null handle) and `Unknown(i16)` for unrecognised codes.

## Safety

- All FFI callbacks are wrapped in `catch_unwind` + `abort()` to prevent panics from unwinding across the FFI boundary.
- Null pointer checks on callback data pointers.
- Empty fragment rejection before calling into C++ (which reads byte 0 without a size check).
- MTU validated on the Rust side to prevent C++ truncation of `u64` to `uint16_t`.
- Embedded data size validated to prevent `uint16_t` truncation in the C API.

## Building

Requires CMake, a C++17 compiler, and libclang (for bindgen). The vendored EFP source is compiled automatically.

```sh
cargo build -p efp
cargo test -p efp
```
