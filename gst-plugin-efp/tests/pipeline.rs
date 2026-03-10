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
