# efp-sys

Raw FFI bindings to the [Elastic Frame Protocol (EFP)](https://github.com/OwnZones/efp) C API.

This crate builds the EFP C++ library from source using `cmake`. The Rust bindings were generated with `bindgen` and are checked in as `src/bindings.rs`, so a normal build does not need `libclang`. Only the C API surface (`efp_c_api/elastic_frame_protocol_c_api.h`) is exposed.

## Prerequisites

- A C++17 compiler (GCC, Clang, or MSVC)
- CMake >= 3.10
- `libclang`, only with the `bindgen` feature

## Building

The EFP source is vendored as a git submodule at `vendor/efp`. After cloning, initialize it:

```sh
git submodule update --init --recursive
```

Then build with Cargo:

```sh
cargo build -p efp-sys
cargo test -p efp-sys
```

## Exposed API

| Function | Description |
|---|---|
| `efp_get_version()` | Returns library version as `(major << 8) \| minor` |
| `efp_init_send()` | Create a sender instance |
| `efp_init_receive()` | Create a receiver instance |
| `efp_end_send()` | Destroy a sender instance |
| `efp_end_receive()` | Destroy a receiver instance |
| `efp_send_data()` | Fragment and send a superframe |
| `efp_receive_fragment()` | Reassemble a received fragment |
| `efp_add_embedded_data()` | Prepend embedded data to a frame |

## Build script

The build script (`build.rs`) tracks vendored C++ source files for incremental rebuilds:

- `vendor/efp/ElasticFrameProtocol.cpp`
- `vendor/efp/ElasticFrameProtocol.h`
- `vendor/efp/efp_c_api/elastic_frame_protocol_c_api.h`
- `efp-cmake/CMakeLists.txt`

## Regenerating the bindings

After the C header changes, build with the `bindgen` feature and copy the generated file over the checked-in one:

```sh
cargo build -p efp-sys --features bindgen
cp target/debug/build/efp-sys-*/out/bindings.rs efp-sys/src/bindings.rs
```

With the feature on, `cargo test -p efp-sys --features bindgen` fails if `src/bindings.rs` no longer matches the header. CI runs with `--all-features`, so it catches a stale file.

## Platform support

- Linux (GCC/Clang)
- macOS (Clang)
- Windows (MSVC)

## License

MIT — same as the upstream EFP library.
