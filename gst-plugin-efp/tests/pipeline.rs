use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstefp::efpdemux::NormalizeSegment;
use gstefp::efpmux::TimestampMode;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstefp::plugin_desc::plugin_register_static().unwrap();
    });
}

fn run_pipeline(pipeline_str: &str) {
    init();
    let pipeline = gst::parse::launch(pipeline_str).unwrap();
    let bus = pipeline.bus().unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "pipeline error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn private_data_roundtrip() {
    // Non-standard payloads (raw video here) must be wrapped as
    // `application/x-efp-private` — that's the efpmux contract for anything
    // outside the natively-recognized {H.264, H.265, Opus} set.
    run_pipeline(
        "videotestsrc num-buffers=30 ! capssetter caps=application/x-efp-private join=false replace=true \
         ! efpmux ! efpdemux ! fakesink",
    );
}

#[test]
fn h264_roundtrip() {
    run_pipeline(
        "videotestsrc num-buffers=30 ! video/x-raw,width=320,height=240 \
         ! x264enc tune=zerolatency ! efpmux ! efpdemux ! h264parse ! fakesink",
    );
}

#[test]
fn opus_roundtrip() {
    run_pipeline(
        "audiotestsrc num-buffers=50 ! audio/x-raw,rate=48000,channels=1 \
         ! opusenc ! efpmux ! efpdemux ! fakesink",
    );
}

#[test]
fn custom_mtu() {
    run_pipeline(
        "videotestsrc num-buffers=10 ! video/x-raw,width=160,height=120 \
         ! capssetter caps=application/x-efp-private join=false replace=true \
         ! efpmux mtu=500 ! efpdemux ! fakesink",
    );
}

/// The efpmux sink template pins H.264 to byte-stream + AU alignment, which
/// forces h264parse to negotiate byte-stream regardless of upstream preference.
/// This protects the wire from avc-framed bytes that would later fail in the
/// receiver-side h264parse (which only sees byte-stream caps).
#[test]
fn mux_sink_template_forces_h264_byte_stream() {
    init();

    let pipeline = gst::parse::launch(
        "videotestsrc num-buffers=5 ! video/x-raw,width=160,height=120,framerate=25/1 \
         ! x264enc tune=zerolatency ! h264parse ! efpmux name=mux ! fakesink",
    )
    .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    // Wait for negotiation to settle. EOS arrives quickly with 5 buffers.
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    // Inspect the mux's sink pad caps — must be byte-stream after negotiation.
    let mux = pipeline
        .downcast_ref::<gst::Pipeline>()
        .unwrap()
        .by_name("mux")
        .expect("mux element in pipeline");
    let sink_pad = mux
        .iterate_sink_pads()
        .into_iter()
        .find_map(|p| p.ok())
        .expect("mux has a sink pad after playing");
    let caps = sink_pad
        .current_caps()
        .expect("negotiated caps on mux sink pad");
    let structure = caps.structure(0).expect("caps has at least one structure");
    assert_eq!(structure.name(), "video/x-h264");
    assert_eq!(
        structure.get::<String>("stream-format").unwrap(),
        "byte-stream",
        "efpmux sink must negotiate byte-stream (got {:?})",
        structure.get::<String>("stream-format")
    );

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn buffer_data_integrity() {
    init();

    let pipeline = gst::parse::launch(
        "videotestsrc num-buffers=5 pattern=0 ! video/x-raw,format=RGB,width=4,height=4 \
         ! capssetter caps=application/x-efp-private join=false replace=true \
         ! efpmux ! efpdemux ! appsink name=sink",
    )
    .unwrap();

    let sink = pipeline
        .downcast_ref::<gst::Pipeline>()
        .unwrap()
        .by_name("sink")
        .unwrap()
        .dynamic_cast::<gst_app::AppSink>()
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut count = 0u32;
    while let Ok(sample) = sink.pull_sample() {
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        // 4x4 RGB = 48 bytes per frame
        assert_eq!(map.len(), 48, "frame {count} has wrong size");
        count += 1;
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert_eq!(count, 5, "expected 5 frames from appsink");
}

/// INLINE_PAYLOAD flag — must be set when the frame contains embedded data.
const INLINE_PAYLOAD: u8 = efp::FLAG_INLINE_PAYLOAD;

/// Helper: run an EFP sender, collect length-prefixed fragments as a single Vec.
fn efp_encode_with_flags(
    payload: &[u8],
    content_type: u8,
    pts: u64,
    stream_id: u8,
    code: u32,
    flags: u8,
) -> Vec<u8> {
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let out_cb = Arc::clone(&out);
    let sender = efp::Sender::new(1400, move |fragment, _sid| {
        let mut buf = out_cb.lock().unwrap();
        buf.extend_from_slice(&(fragment.len() as u32).to_be_bytes());
        buf.extend_from_slice(fragment);
    })
    .unwrap();
    sender
        .send(payload, content_type, pts, pts, code, stream_id, flags)
        .unwrap();
    drop(sender); // release Arc reference held by callback
    Arc::try_unwrap(out).unwrap().into_inner().unwrap()
}

fn efp_encode(payload: &[u8], content_type: u8, pts: u64, stream_id: u8, code: u32) -> Vec<u8> {
    efp_encode_with_flags(payload, content_type, pts, stream_id, code, 0)
}

#[test]
fn broken_frame_flagged_as_corrupted() {
    // Create a large payload that fragments into multiple pieces, then drop
    // some fragments so the receiver produces a broken superframe.
    init();

    let fragments = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb = Arc::clone(&fragments);
    let sender = efp::Sender::new(1400, move |fragment, _sid| {
        frag_cb.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();

    let payload = vec![0xAB; 10_000]; // will produce ~8 fragments
    sender.send(&payload, 0x01, 1000, 1000, 0, 1, 0).unwrap();

    let frags = fragments.lock().unwrap();
    assert!(frags.len() > 2, "need multiple fragments to drop one");

    // Build EFP bytestream but skip the second fragment.
    let mut incomplete_data = Vec::new();
    for (i, frag) in frags.iter().enumerate() {
        if i == 1 {
            continue; // drop fragment
        }
        incomplete_data.extend_from_slice(&(frag.len() as u32).to_be_bytes());
        incomplete_data.extend_from_slice(frag);
    }
    drop(frags);

    // Build a second complete frame to trigger the receiver's timeout.
    let good_data = efp_encode(&[0xCD; 500], 0x01, 2000, 1, 0);

    let results = Arc::new(Mutex::new(Vec::<(Vec<u8>, gst::BufferFlags)>::new()));
    let results_cb = Arc::clone(&results);

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc").build().unwrap();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    let appsink = gst::ElementFactory::make("appsink")
        .property("async", false)
        .build()
        .unwrap();

    // Use threaded mode with short timeouts so the incomplete frame times
    // out in the receiver's background thread.
    demux.set_property("threaded", true);
    demux.set_property("bucket-timeout", 1u32);
    demux.set_property("hol-timeout", 1u32);

    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().unwrap();
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                results_cb
                    .lock()
                    .unwrap()
                    .push((map.as_slice().to_vec(), buffer.flags()));
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let appsink = appsink.upcast::<gst::Element>();

    pipeline.add_many([&appsrc, &demux, &appsink]).unwrap();
    appsrc.link(&demux).unwrap();

    let appsink_weak = appsink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if pad.name().starts_with("src_") {
            if let Some(sink) = appsink_weak.upgrade() {
                let sinkpad = sink.static_pad("sink").unwrap();
                if !sinkpad.is_linked() {
                    pad.link(&sinkpad).unwrap();
                }
            }
        }
    });

    let caps = gst::Caps::builder("application/x-efp").build();
    appsrc.set_property("caps", &caps);
    appsrc.set_property("format", gst::Format::Bytes);

    pipeline.set_state(gst::State::Playing).unwrap();

    let src = appsrc.dynamic_cast::<gst_app::AppSrc>().unwrap();

    // Push the incomplete frame first.
    let buf1 = gst::Buffer::from_slice(incomplete_data);
    src.push_buffer(buf1).unwrap();

    // Sleep to let the bucket timeout expire (10ms timeout + margin).
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Push the second complete frame.
    let buf2 = gst::Buffer::from_slice(good_data);
    src.push_buffer(buf2).unwrap();

    // Small delay before EOS so the receiver's background thread delivers
    // the broken frame and drain_pending picks it up.
    std::thread::sleep(std::time::Duration::from_millis(100));
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(_) => break,
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    let out = results.lock().unwrap().clone();
    assert!(!out.is_empty(), "should produce at least one frame");

    let has_corrupted = out
        .iter()
        .any(|(_, flags)| flags.contains(gst::BufferFlags::CORRUPTED));
    assert!(
        has_corrupted,
        "at least one frame should have CORRUPTED flag, got {:?}",
        out.iter().map(|(_, f)| f).collect::<Vec<_>>()
    );
}

#[test]
fn code_field_roundtrip() {
    // Encode with a specific code value and verify it appears in demux output caps.
    let code = efp::code(b'T', b'E', b'S', b'T');
    let data = efp_encode(b"hello", 0x01, 1000, 1, code);

    init();

    let caps_seen = Arc::new(Mutex::new(None::<gst::Caps>));
    let caps_cb = Arc::clone(&caps_seen);

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc").build().unwrap();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    let appsink = gst::ElementFactory::make("appsink").build().unwrap();

    // Capture caps from samples in the callback.
    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                if caps_cb.lock().unwrap().is_none() {
                    if let Some(caps) = sample.caps() {
                        *caps_cb.lock().unwrap() = Some(caps.to_owned());
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let appsink = appsink.upcast::<gst::Element>();

    pipeline.add_many([&appsrc, &demux, &appsink]).unwrap();
    appsrc.link(&demux).unwrap();

    let appsink_weak = appsink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if pad.name().starts_with("src_") {
            if let Some(sink) = appsink_weak.upgrade() {
                let sinkpad = sink.static_pad("sink").unwrap();
                if !sinkpad.is_linked() {
                    pad.link(&sinkpad).unwrap();
                }
            }
        }
    });

    let src_caps = gst::Caps::builder("application/x-efp").build();
    appsrc.set_property("caps", &src_caps);
    appsrc.set_property("format", gst::Format::Bytes);

    pipeline.set_state(gst::State::Playing).unwrap();

    let src = appsrc.dynamic_cast::<gst_app::AppSrc>().unwrap();
    let buf = gst::Buffer::from_slice(data);
    src.push_buffer(buf).unwrap();
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let caps = caps_seen.lock().unwrap();
    let caps = caps.as_ref().expect("should have received caps");
    let s = caps.structure(0).unwrap();
    let efp_code = s
        .get::<&str>("efp-code")
        .expect("caps should have efp-code");
    assert_eq!(efp_code, "TEST", "code field should round-trip as 'TEST'");
}

#[test]
fn embedded_data_output() {
    // Create a frame with embedded data and verify the demux outputs it
    // on the embedded pad.
    let frame_payload = b"video-frame-data";
    let embed_payload = b"metadata-payload";

    // Build frame with embedded data prepended.
    let combined = efp::add_embedded_data(embed_payload, frame_payload, 42, true).unwrap();

    // Encode via EFP sender with INLINE_PAYLOAD flag to signal embedded data.
    let data = efp_encode_with_flags(&combined, 0x01, 5000, 1, 0, INLINE_PAYLOAD);

    init();

    let embed_buffers = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let embed_cb = Arc::clone(&embed_buffers);

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc").build().unwrap();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    let fakesink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .build()
        .unwrap();
    let embed_sink = gst::ElementFactory::make("appsink")
        .name("embed_sink")
        .property("async", false)
        .build()
        .unwrap();

    let embed_sink = embed_sink.dynamic_cast::<gst_app::AppSink>().unwrap();
    embed_sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().unwrap();
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                embed_cb.lock().unwrap().push(map.as_slice().to_vec());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let embed_sink = embed_sink.upcast::<gst::Element>();

    pipeline
        .add_many([&appsrc, &demux, &fakesink, &embed_sink])
        .unwrap();
    appsrc.link(&demux).unwrap();

    let fakesink_weak = fakesink.downgrade();
    let embed_sink_weak = embed_sink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        let name = pad.name();
        if name.starts_with("src_") {
            if let Some(sink) = fakesink_weak.upgrade() {
                let sinkpad = sink.static_pad("sink").unwrap();
                if !sinkpad.is_linked() {
                    pad.link(&sinkpad).unwrap();
                }
            }
        } else if name.as_str() == "embedded" {
            if let Some(sink) = embed_sink_weak.upgrade() {
                let sinkpad = sink.static_pad("sink").unwrap();
                if !sinkpad.is_linked() {
                    pad.link(&sinkpad).unwrap();
                }
            }
        }
    });

    let src_caps = gst::Caps::builder("application/x-efp").build();
    appsrc.set_property("caps", &src_caps);
    appsrc.set_property("format", gst::Format::Bytes);

    pipeline.set_state(gst::State::Playing).unwrap();

    let src = appsrc.dynamic_cast::<gst_app::AppSrc>().unwrap();
    let buf = gst::Buffer::from_slice(data);
    src.push_buffer(buf).unwrap();
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let bufs = embed_buffers.lock().unwrap();
    assert!(
        !bufs.is_empty(),
        "should have received embedded data on the embedded pad"
    );
    assert_eq!(bufs[0], embed_payload, "embedded data content should match");
}

#[test]
fn normalize_segment_property_roundtrip() {
    init();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();

    let value: NormalizeSegment = demux.property("normalize-segment");
    assert_eq!(value, NormalizeSegment::Auto, "default should be Auto");

    demux.set_property_from_str("normalize-segment", "never");
    let value: NormalizeSegment = demux.property("normalize-segment");
    assert_eq!(value, NormalizeSegment::Never);

    demux.set_property_from_str("normalize-segment", "always");
    let value: NormalizeSegment = demux.property("normalize-segment");
    assert_eq!(value, NormalizeSegment::Always);
}

/// Run a frame with a large PTS through the demux with a given
/// normalize-segment setting, and return the sticky segment's `start` value
/// observed on the outgoing src pad after the frame has been pushed.
///
/// If `clock_type` is `Some`, a dedicated `GstSystemClock` instance with that
/// type is used as the pipeline clock (does not touch the global singleton).
fn observed_segment_start(mode: &str, clock_type: Option<gst::ClockType>) -> gst::ClockTime {
    init();

    // 20 seconds in nanoseconds — large enough to trigger the Always path.
    let pts = 20 * gst::ClockTime::SECOND.nseconds();
    let data = efp_encode(b"payload", 0x01, pts, 1, 0);

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc").build().unwrap();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    let fakesink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();

    demux.set_property_from_str("normalize-segment", mode);

    if let Some(ct) = clock_type {
        let clock: gst::SystemClock = glib::Object::builder().property("clock-type", ct).build();
        pipeline.use_clock(Some(&clock));
    }

    pipeline.add_many([&appsrc, &demux, &fakesink]).unwrap();
    appsrc.link(&demux).unwrap();

    let observed = Arc::new(Mutex::new(None::<gst::ClockTime>));
    let observed_cb = Arc::clone(&observed);
    let fakesink_weak = fakesink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if !pad.name().starts_with("src_") {
            return;
        }
        let obs = Arc::clone(&observed_cb);
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                if let gst::EventView::Segment(seg) = event.view() {
                    if let Ok(s) = seg.segment().clone().downcast::<gst::ClockTime>() {
                        *obs.lock().unwrap() = s.start();
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        if let Some(sink) = fakesink_weak.upgrade() {
            let sinkpad = sink.static_pad("sink").unwrap();
            if !sinkpad.is_linked() {
                pad.link(&sinkpad).unwrap();
            }
        }
    });

    let src_caps = gst::Caps::builder("application/x-efp").build();
    appsrc.set_property("caps", &src_caps);
    appsrc.set_property("format", gst::Format::Bytes);

    pipeline.set_state(gst::State::Playing).unwrap();

    let src = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();
    let buf = gst::Buffer::from_slice(data);
    src.push_buffer(buf).unwrap();
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let result = observed.lock().unwrap().unwrap_or(gst::ClockTime::ZERO);
    result
}

#[test]
fn normalize_segment_always_rewrites_segment_start() {
    let start = observed_segment_start("always", None);
    let expected = gst::ClockTime::from_seconds(20);
    assert_eq!(
        start, expected,
        "with normalize-segment=always, segment.start should be updated to PTS"
    );
}

#[test]
fn normalize_segment_never_preserves_zero_start() {
    let start = observed_segment_start("never", None);
    assert_eq!(
        start,
        gst::ClockTime::ZERO,
        "with normalize-segment=never, segment.start should remain at 0"
    );
}

#[test]
fn normalize_segment_auto_monotonic_clock_rewrites() {
    // Monotonic clock signals "no interest in absolute time" → normalize.
    let start = observed_segment_start("auto", Some(gst::ClockType::Monotonic));
    assert_eq!(
        start,
        gst::ClockTime::from_seconds(20),
        "Auto + monotonic clock should rewrite segment.start to PTS"
    );
}

#[test]
fn normalize_segment_auto_realtime_clock_leaves_segment_default() {
    // Realtime clock resolves Auto to RebaseToRunningTime. The rebase path
    // leaves the outgoing segment at its default (start=0) because buffer
    // PTS is small after the base_time subtraction — the Always-style
    // segment rewrite is not needed. See
    // `normalize_segment_auto_realtime_rebases_to_small_pts` for the PTS
    // side of the same contract.
    let start = observed_segment_start("auto", Some(gst::ClockType::Realtime));
    assert_eq!(
        start,
        gst::ClockTime::ZERO,
        "Auto + realtime clock resolves to RebaseToRunningTime; segment.start stays at 0"
    );
}

/// End-to-end sync contract: buffers pushed through efpmux → efpdemux with
/// `normalize-segment=never` must retain their absolute PTS on the output
/// side, and the outgoing segment.start must remain 0 so that downstream
/// running-time equals absolute PTS. This is the foundation that lets a
/// consumer line up two independent demux outputs by running-time.
#[test]
fn pts_preservation_roundtrip_with_normalize_never() {
    init();

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc")
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    // Explicitly pick passthrough on the mux side so the test validates the
    // "PTS survives the round-trip unchanged" contract. The new default
    // (absolute-from-running-time) would add base_time, breaking equality.
    mux.set_property_from_str("timestamp-mode", "buffer");
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    demux.set_property_from_str("normalize-segment", "never");
    let appsink = gst::ElementFactory::make("appsink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();

    let caps = gst::Caps::builder("application/x-efp-private")
        .field("content-type", 0x20i32)
        .build();
    appsrc.set_property("caps", &caps);

    pipeline
        .add_many([&appsrc, &mux, &demux, &appsink])
        .unwrap();
    appsrc.link(&mux).unwrap();
    mux.link(&demux).unwrap();

    // Capture buffer PTS on the appsink side.
    let appsink_cast = appsink.clone().dynamic_cast::<gst_app::AppSink>().unwrap();
    let received = Arc::new(Mutex::new(Vec::<gst::ClockTime>::new()));
    let received_cb = Arc::clone(&received);
    appsink_cast.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().unwrap();
                if let Some(pts) = buffer.pts() {
                    received_cb.lock().unwrap().push(pts);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Capture sticky segment on the demuxer's src pad.
    let segment_start = Arc::new(Mutex::new(None::<gst::ClockTime>));
    let segment_start_cb = Arc::clone(&segment_start);
    let appsink_weak = appsink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if !pad.name().starts_with("src_") {
            return;
        }
        let seg = Arc::clone(&segment_start_cb);
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                if let gst::EventView::Segment(ev) = event.view() {
                    if let Ok(s) = ev.segment().clone().downcast::<gst::ClockTime>() {
                        *seg.lock().unwrap() = s.start();
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        if let Some(sink) = appsink_weak.upgrade() {
            let sinkpad = sink.static_pad("sink").unwrap();
            if !sinkpad.is_linked() {
                pad.link(&sinkpad).unwrap();
            }
        }
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    // Push three buffers with large absolute PTS (simulating a wallclock-
    // stamped sender). 20.000s, 20.040s, 20.080s (40 ms apart).
    let base_pts = gst::ClockTime::from_seconds(20);
    let step = gst::ClockTime::from_mseconds(40);
    let pushed: Vec<gst::ClockTime> = (0..3).map(|i| base_pts + step * (i as u64)).collect();

    let src = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();
    for pts in &pushed {
        let mut buf = gst::Buffer::from_slice(b"payload".to_vec());
        buf.get_mut().unwrap().set_pts(*pts);
        buf.get_mut().unwrap().set_dts(*pts);
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let rx = received.lock().unwrap().clone();
    assert_eq!(
        rx, pushed,
        "output PTS must equal input PTS when normalize-segment=never"
    );

    let seg_start = segment_start
        .lock()
        .unwrap()
        .unwrap_or(gst::ClockTime::ZERO);
    assert_eq!(
        seg_start,
        gst::ClockTime::ZERO,
        "outgoing segment.start must remain 0 so running-time == absolute PTS"
    );
}

#[test]
fn timestamp_mode_property_roundtrip() {
    init();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();

    let value: TimestampMode = mux.property("timestamp-mode");
    assert_eq!(
        value,
        TimestampMode::AbsoluteFromRunningTime,
        "default should be AbsoluteFromRunningTime"
    );

    mux.set_property_from_str("timestamp-mode", "buffer");
    let value: TimestampMode = mux.property("timestamp-mode");
    assert_eq!(value, TimestampMode::Buffer);

    mux.set_property_from_str("timestamp-mode", "absolute-from-running-time");
    let value: TimestampMode = mux.property("timestamp-mode");
    assert_eq!(value, TimestampMode::AbsoluteFromRunningTime);
}

/// Mux in `absolute-from-running-time` mode must add the pipeline's base_time
/// to the PTS embedded in the EFP wire frame, so receivers that share the
/// same clock domain can reconstruct absolute sender timing.
///
/// The on-wire EFP-encoded PTS lives inside the payload bytes — the
/// GStreamer buffer PTS on mux.src is set from the input buffer and does
/// not reflect the rewrite. To read the wire PTS back cheaply we chain a
/// `demux normalize-segment=never` which decodes the EFP frame and sets
/// `buffer.pts` directly to the wire PTS without any rebasing.
#[test]
fn mux_abs_mode_writes_absolute_wire_pts() {
    init();

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc")
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    mux.set_property_from_str("timestamp-mode", "absolute-from-running-time");
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    demux.set_property_from_str("normalize-segment", "never");
    let appsink = gst::ElementFactory::make("appsink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();

    let caps = gst::Caps::builder("application/x-efp-private")
        .field("content-type", 0x20i32)
        .build();
    appsrc.set_property("caps", &caps);

    pipeline
        .add_many([&appsrc, &mux, &demux, &appsink])
        .unwrap();
    appsrc.link(&mux).unwrap();
    mux.link(&demux).unwrap();

    let appsink_cast = appsink.clone().dynamic_cast::<gst_app::AppSink>().unwrap();
    let received = Arc::new(Mutex::new(Vec::<gst::ClockTime>::new()));
    let received_cb = Arc::clone(&received);
    appsink_cast.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                if let Some(pts) = sample.buffer().and_then(|b| b.pts()) {
                    received_cb.lock().unwrap().push(pts);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let appsink_weak = appsink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if !pad.name().starts_with("src_") {
            return;
        }
        if let Some(sink) = appsink_weak.upgrade() {
            let sinkpad = sink.static_pad("sink").unwrap();
            if !sinkpad.is_linked() {
                pad.link(&sinkpad).unwrap();
            }
        }
    });

    pipeline.set_state(gst::State::Playing).unwrap();
    let base_time = pipeline.base_time().expect("pipeline base_time after Playing");

    let input_pts: Vec<gst::ClockTime> = (0..3)
        .map(|i| gst::ClockTime::from_mseconds(40 * i as u64))
        .collect();

    let src = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();
    for pts in &input_pts {
        let mut buf = gst::Buffer::from_slice(b"payload".to_vec());
        buf.get_mut().unwrap().set_pts(*pts);
        buf.get_mut().unwrap().set_dts(*pts);
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let observed = received.lock().unwrap().clone();
    assert_eq!(
        observed.len(),
        input_pts.len(),
        "mux should emit one wire frame per input buffer"
    );
    for (i, (out, inp)) in observed.iter().zip(input_pts.iter()).enumerate() {
        assert_eq!(
            *out,
            *inp + base_time,
            "frame {}: wire PTS must equal input PTS + pipeline base_time",
            i
        );
    }
}

/// End-to-end: mux in absolute mode + demux in rebase mode, both in the same
/// pipeline. Since mux adds base_time and demux subtracts the same base_time,
/// the round-trip is the identity transform — downstream sees the input
/// running-time unchanged, even though the wire carried absolute clock-time.
#[test]
fn mux_abs_to_demux_rebase_roundtrips_running_time() {
    init();

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc")
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    mux.set_property_from_str("timestamp-mode", "absolute-from-running-time");
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    demux.set_property_from_str("normalize-segment", "rebase-to-running-time");
    let appsink = gst::ElementFactory::make("appsink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();

    let caps = gst::Caps::builder("application/x-efp-private")
        .field("content-type", 0x20i32)
        .build();
    appsrc.set_property("caps", &caps);

    pipeline
        .add_many([&appsrc, &mux, &demux, &appsink])
        .unwrap();
    appsrc.link(&mux).unwrap();
    mux.link(&demux).unwrap();

    let appsink_cast = appsink.clone().dynamic_cast::<gst_app::AppSink>().unwrap();
    let received = Arc::new(Mutex::new(Vec::<gst::ClockTime>::new()));
    let received_cb = Arc::clone(&received);
    appsink_cast.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                if let Some(pts) = sample.buffer().and_then(|b| b.pts()) {
                    received_cb.lock().unwrap().push(pts);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let appsink_weak = appsink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if !pad.name().starts_with("src_") {
            return;
        }
        if let Some(sink) = appsink_weak.upgrade() {
            let sinkpad = sink.static_pad("sink").unwrap();
            if !sinkpad.is_linked() {
                pad.link(&sinkpad).unwrap();
            }
        }
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    // Push small running-time PTS values. The mux will add base_time on the
    // wire, and the demux will subtract it on the way out — round-trip is
    // expected to reproduce these exact values.
    let input_pts: Vec<gst::ClockTime> = (0..3)
        .map(|i| gst::ClockTime::from_mseconds(40 * i as u64))
        .collect();

    let src = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();
    for pts in &input_pts {
        let mut buf = gst::Buffer::from_slice(b"payload".to_vec());
        buf.get_mut().unwrap().set_pts(*pts);
        buf.get_mut().unwrap().set_dts(*pts);
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let rx = received.lock().unwrap().clone();
    assert_eq!(
        rx, input_pts,
        "abs-mux → rebase-demux round-trip must reproduce the input running-time PTS exactly"
    );
}

/// Auto mode on a wall-clock pipeline must resolve to RebaseToRunningTime
/// (not legacy passthrough). Verifies the output PTS is small running-time,
/// which is only true if the demux subtracted base_time.
#[test]
fn normalize_segment_auto_realtime_rebases_to_small_pts() {
    init();

    let pipeline = gst::Pipeline::new();
    let appsrc = gst::ElementFactory::make("appsrc")
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    mux.set_property_from_str("timestamp-mode", "absolute-from-running-time");
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    // Auto is the default — state it explicitly for the reader.
    demux.set_property_from_str("normalize-segment", "auto");
    let appsink = gst::ElementFactory::make("appsink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();

    // Fresh Realtime SystemClock (does not touch the global singleton).
    let clock: gst::SystemClock = glib::Object::builder()
        .property("clock-type", gst::ClockType::Realtime)
        .build();
    pipeline.use_clock(Some(&clock));

    let caps = gst::Caps::builder("application/x-efp-private")
        .field("content-type", 0x20i32)
        .build();
    appsrc.set_property("caps", &caps);

    pipeline
        .add_many([&appsrc, &mux, &demux, &appsink])
        .unwrap();
    appsrc.link(&mux).unwrap();
    mux.link(&demux).unwrap();

    let appsink_cast = appsink.clone().dynamic_cast::<gst_app::AppSink>().unwrap();
    let received = Arc::new(Mutex::new(Vec::<gst::ClockTime>::new()));
    let received_cb = Arc::clone(&received);
    appsink_cast.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                if let Some(pts) = sample.buffer().and_then(|b| b.pts()) {
                    received_cb.lock().unwrap().push(pts);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let appsink_weak = appsink.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        if !pad.name().starts_with("src_") {
            return;
        }
        if let Some(sink) = appsink_weak.upgrade() {
            let sinkpad = sink.static_pad("sink").unwrap();
            if !sinkpad.is_linked() {
                pad.link(&sinkpad).unwrap();
            }
        }
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    let input_pts: Vec<gst::ClockTime> = (0..3)
        .map(|i| gst::ClockTime::from_mseconds(40 * i as u64))
        .collect();

    let src = appsrc.clone().dynamic_cast::<gst_app::AppSrc>().unwrap();
    for pts in &input_pts {
        let mut buf = gst::Buffer::from_slice(b"payload".to_vec());
        buf.get_mut().unwrap().set_pts(*pts);
        buf.get_mut().unwrap().set_dts(*pts);
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let rx = received.lock().unwrap().clone();
    assert_eq!(
        rx, input_pts,
        "auto + realtime clock must resolve to RebaseToRunningTime and reproduce input running-time PTS"
    );
}
