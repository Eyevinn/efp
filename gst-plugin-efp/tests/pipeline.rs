use std::sync::{Arc, Mutex};

use gst::prelude::*;

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
fn raw_video_roundtrip() {
    run_pipeline("videotestsrc num-buffers=30 ! efpmux ! efpdemux ! fakesink");
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
         ! efpmux mtu=500 ! efpdemux ! fakesink",
    );
}

#[test]
fn buffer_data_integrity() {
    init();

    let pipeline = gst::parse::launch(
        "videotestsrc num-buffers=5 pattern=0 ! video/x-raw,format=RGB,width=4,height=4 \
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
