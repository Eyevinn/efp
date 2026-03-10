# gst-plugin-efp

GStreamer plugin providing **efpmux** and **efpdemux** elements for the
[Elastic Frame Protocol](https://github.com/OwnZones/efp) (EFP).

EFP is a multiplexing protocol that fragments elementary streams into
transport-ready superframes and reassembles them on the receiving side. The
transport layer (SRT, UDP, TCP, ...) is completely separate.

## Elements

| Element     | Description |
|-------------|-------------|
| **efpmux**  | Accepts multiple elementary stream sink pads and outputs a bytestream of length-prefixed EFP fragments on a single src pad. |
| **efpdemux**| Accepts a bytestream of EFP fragments on a single sink pad and exposes reassembled elementary streams on dynamic src pads. |

## Properties

### efpmux

| Property | Type | Default | Range | Description |
|----------|------|---------|-------|-------------|
| `mtu`    | uint | 1400    | 100 - 65535 | Maximum fragment size in bytes |

### efpdemux

| Property | Type | Default | Range | Description |
|----------|------|---------|-------|-------------|
| `bucket-timeout` | uint | 5 | 1 - 1000 | Bucket timeout (x 10 ms) |
| `hol-timeout`    | uint | 5 | 1 - 1000 | Head-of-line timeout (x 10 ms) |

## Wire format

The mux outputs and demux expects a bytestream of length-prefixed EFP fragments:

```
[4-byte BE length][fragment bytes][4-byte BE length][fragment bytes]...
```

This framing allows transport over any reliable bytestream (TCP, SRT, pipes).

## Content type mapping

| GStreamer caps | EFP content type |
|---|---|
| `video/x-h264` | `0x83` (H264) |
| `video/x-h265` | `0x84` (H265) |
| `audio/x-opus` | `0x89` (Opus) |
| anything else | `0x01` (private data) |

## Flush and discontinuity handling

- **DISCONT flag**: The demux reinitializes the EFP receiver on discontinuity to prevent assembling frames from fragments spanning the gap.
- **Flush events**: FlushStart is forwarded to all src pads. FlushStop reinitializes the receiver and forwards downstream.
- **EOS**: The mux waits for EOS on all sink pads before forwarding.

## Stream IDs

The mux allocates stream IDs 1-255 for sink pads and recycles them when pads are released. Pad names follow the pattern `sink_<N>` where N is the allocated stream ID.

## Example pipelines

**Send H.264 + Opus over SRT:**

```sh
gst-launch-1.0 \
  efpmux name=mux mtu=1316 ! srtsink uri=srt://:9000 \
  videotestsrc ! x264enc tune=zerolatency ! mux.sink_1 \
  audiotestsrc ! opusenc ! mux.sink_2
```

**Receive and decode:**

```sh
gst-launch-1.0 \
  srtsrc uri=srt://127.0.0.1:9000 ! efpdemux name=demux \
  demux.src_1 ! h264parse ! avdec_h264 ! autovideosink \
  demux.src_2 ! opusdec ! autoaudiosink
```

## Building

Requires GStreamer development libraries (>= 1.20), CMake, a C++17 compiler, and libclang.

```sh
cargo build -p gst-plugin-efp
```

To use the plugin without installing, point GStreamer at the build output:

```sh
export GST_PLUGIN_PATH=$PWD/target/debug
gst-inspect-1.0 efpmux
gst-inspect-1.0 efpdemux
```
