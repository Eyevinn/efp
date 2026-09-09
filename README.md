# efp – Rust Bindings & GStreamer Plugin for Elastic Frame Protocol

Rust workspace providing safe bindings and a GStreamer plugin for the [Elastic Frame Protocol (EFP)](https://github.com/OwnZones/efp), a modern framing layer for media transport over IP networks.

EFP abstracts media payloads from the underlying transport (UDP, TCP, SRT, RIST, etc.) with ~0.5% overhead, 64-bit timestamps, and robust error detection — designed for lossy and out-of-order IP delivery.

## Workspace Structure

```
efp/
├── efp-sys/          – Raw FFI bindings (bindgen + cmake)
├── efp/              – Safe, idiomatic Rust wrapper
├── gst-plugin-efp/   – GStreamer elements (efpmux & efpdemux)
└── vendor/efp/       – Vendored C++17 library (git submodule)
```

### efp-sys

Low-level FFI bindings generated with `bindgen` from the EFP C API. The build script compiles the vendored C++ library via CMake and links it statically.

### efp

Safe Rust wrapper exposing:

- **Sender** – Fragments data into MTU-sized pieces, delivering them via callback.
- **Receiver** – Reassembles fragments with configurable bucket and head-of-line timeouts. Supports threaded and run-to-completion modes.
- **SuperFrame** – Reassembled frame carrying payload, timestamps (PTS/DTS), content type, stream ID, and flags.
- **Embedded data** – Optional metadata channel alongside frame data, carrying the data type and the stream ID of the frame it rode in on.
- Built-in content type constants for H.264, H.265, Opus, and private data.

### gst-plugin-efp

GStreamer plugin registering two elements:

- **efpmux** – Muxes media streams into EFP format.
- **efpdemux** – Demultiplexes EFP streams back into media.

#### Embedded data

Embedded data is addressed to a media stream: it is buffered until the next
frame on that stream and travels with it.

On `efpmux`, request an `embed_%u` sink pad and send caps carrying both fields.
Both are required — caps without them are rejected rather than defaulting to
stream 0, which no media stream is ever assigned:

```
application/x-efp-embedded, data-type=(int)7, stream-id=(int)1
```

Media stream IDs are allocated from 1 in sink-pad request order, so the first
media pad requested is stream 1. Data addressed to a stream that carries no
media is dropped with a warning, since there is no frame for it to ride out on.

On `efpdemux`, each stream that carries embedded data gets its own
`embedded_<stream-id>` src pad, whose caps carry `stream-id` and the current
`data-type`. A stream that later sends a different data type renegotiates.

## Building

Prerequisites: Rust toolchain, CMake, a C++17 compiler, and GStreamer development libraries (for the plugin).

```sh
# Clone with submodules
git clone --recurse-submodules https://github.com/Eyevinn/efp.git
cd efp

# Build all crates
cargo build

# Run tests
cargo test
```

## License

MIT
