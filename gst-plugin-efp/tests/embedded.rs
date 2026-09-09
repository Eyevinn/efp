//! Round-trip tests for EFP's embedded-data channel through `efpmux` and
//! `efpdemux`.
//!
//! `efpmux`'s `embed_%u` request pad had no test coverage at all: the only
//! embedded test drove `efpdemux` from hand-encoded frames, so nothing
//! exercised the send side or the two elements together.

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

/// One embedded buffer as it came out of `efpdemux`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Received {
    pad: String,
    stream_id: i32,
    data_type: i32,
    data: Vec<u8>,
}

/// One embedded-data source feeding an `embed_%u` pad.
struct EmbedTrack {
    stream_id: i32,
    data_type: i32,
    payloads: Vec<Vec<u8>>,
}

/// Build `media_tracks` media streams and `embeds` embedded-data streams into
/// one `efpmux ! efpdemux` pipeline, run it, and return everything that came
/// out of the demuxer's embedded pads.
///
/// Media stream IDs are allocated by `efpmux` from 1 in pad-request order, so
/// the first media track is stream 1, the second stream 2, and so on. Each
/// embedded payload is pushed before the media buffers, and several media
/// buffers follow, so the data rides out whichever frame arrives first after
/// it — the assertions do not depend on which one that is.
fn run(media_tracks: usize, embeds: &[EmbedTrack]) -> (Vec<Received>, Vec<String>) {
    init();

    let pipeline = gst::Pipeline::new();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    pipeline.add_many([&mux, &demux]).unwrap();
    mux.link(&demux).unwrap();

    let media_caps = gst::Caps::builder("application/x-efp-private").build();
    let mut media_srcs = Vec::new();
    for _ in 0..media_tracks {
        let src = gst::ElementFactory::make("appsrc")
            .property("caps", &media_caps)
            .property("format", gst::Format::Time)
            .property("is-live", false)
            .build()
            .unwrap();
        pipeline.add(&src).unwrap();
        // Linking requests a `sink_%u` pad, which is what allocates the EFP
        // stream ID, so link order fixes the stream numbering.
        src.link(&mux).unwrap();
        media_srcs.push(src.dynamic_cast::<gst_app::AppSrc>().unwrap());
    }

    let embed_template = mux.pad_template("embed_%u").unwrap();
    let mut embed_srcs = Vec::new();
    for embed in embeds {
        let caps = gst::Caps::builder("application/x-efp-embedded")
            .field("data-type", embed.data_type)
            .field("stream-id", embed.stream_id)
            .build();
        let src = gst::ElementFactory::make("appsrc")
            .property("caps", &caps)
            .property("format", gst::Format::Time)
            .property("is-live", false)
            .build()
            .unwrap();
        pipeline.add(&src).unwrap();
        // Request without a name or caps, the way a caller that only knows the
        // addressing at streaming time would.
        let pad = mux.request_pad(&embed_template, None, None).unwrap();
        src.static_pad("src").unwrap().link(&pad).unwrap();
        embed_srcs.push(src.dynamic_cast::<gst_app::AppSrc>().unwrap());
    }

    let received: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
    let pad_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let pipeline_weak = pipeline.downgrade();
    let received_cb = Arc::clone(&received);
    let pad_names_cb = Arc::clone(&pad_names);
    demux.connect_pad_added(move |_demux, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let name = pad.name().to_string();
        pad_names_cb.lock().unwrap().push(name.clone());

        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();

        if !name.starts_with("embedded_") {
            return;
        }

        let received = Arc::clone(&received_cb);
        let pad_name = name.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                // Read the caps at push time so a renegotiation between
                // buffers is visible per buffer rather than only at the end.
                let caps = pad.current_caps().expect("embedded pad must have caps");
                let s = caps.structure(0).unwrap();
                let map = buffer.map_readable().unwrap();
                received.lock().unwrap().push(Received {
                    pad: pad_name.clone(),
                    stream_id: s
                        .get::<i32>("stream-id")
                        .expect("caps must carry stream-id"),
                    data_type: s
                        .get::<i32>("data-type")
                        .expect("caps must carry data-type"),
                    data: map.as_slice().to_vec(),
                });
            }
            gst::PadProbeReturn::Ok
        });
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    for (i, src) in embed_srcs.iter().enumerate() {
        for payload in &embeds[i].payloads {
            src.push_buffer(gst::Buffer::from_slice(payload.clone()))
                .unwrap();
        }
    }

    // Enough media frames that the pending embedded data is flushed out
    // regardless of how the two appsrc streaming threads interleave.
    for src in &media_srcs {
        for i in 0..10u64 {
            let mut buffer = gst::Buffer::from_slice(format!("media-{i}").into_bytes());
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 40));
            src.push_buffer(buffer).unwrap();
        }
    }

    for src in &embed_srcs {
        let _ = src.end_of_stream();
    }
    for src in &media_srcs {
        let _ = src.end_of_stream();
    }

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                pipeline.set_state(gst::State::Null).unwrap();
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();

    let r = received.lock().unwrap().clone();
    let n = pad_names.lock().unwrap().clone();
    (r, n)
}

/// The capability this change exists for: two media streams each carrying their
/// own embedded data, arriving on separate pads that name their stream.
///
/// Before this, `efpdemux` cached one `embedded` pad and returned it for every
/// stream and data type, and its caps carried no `stream-id` at all — so a
/// receiver got bytes it could not attribute to a media stream.
#[test]
fn each_stream_gets_its_own_embedded_pad() {
    let (received, pads) = run(
        2,
        &[
            EmbedTrack {
                stream_id: 1,
                data_type: 7,
                payloads: vec![b"describes-stream-1".to_vec()],
            },
            EmbedTrack {
                stream_id: 2,
                data_type: 9,
                payloads: vec![b"describes-stream-2".to_vec()],
            },
        ],
    );

    let mut embedded_pads: Vec<&String> =
        pads.iter().filter(|n| n.starts_with("embedded_")).collect();
    embedded_pads.sort();
    assert_eq!(
        embedded_pads,
        vec!["embedded_1", "embedded_2"],
        "each stream should get a pad named for it, got {pads:?}"
    );

    let mut seen: Vec<(String, i32, i32, Vec<u8>)> = received
        .iter()
        .map(|r| (r.pad.clone(), r.stream_id, r.data_type, r.data.clone()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            (
                "embedded_1".to_string(),
                1,
                7,
                b"describes-stream-1".to_vec()
            ),
            (
                "embedded_2".to_string(),
                2,
                9,
                b"describes-stream-2".to_vec()
            ),
        ]
    );
}

/// A single embedded block makes the whole trip with its addressing intact.
#[test]
fn one_embedded_block_survives_the_round_trip() {
    let (received, _) = run(
        1,
        &[EmbedTrack {
            stream_id: 1,
            data_type: 42,
            payloads: vec![b"provenance-manifest".to_vec()],
        }],
    );

    assert_eq!(received.len(), 1, "got {received:?}");
    assert_eq!(received[0].stream_id, 1);
    assert_eq!(received[0].data_type, 42);
    assert_eq!(received[0].data, b"provenance-manifest");
}

/// Several blocks queued for one stream all arrive.
///
/// `add_embedded_data` prepends, so the block the muxer writes first ends up
/// last on the wire and is the one that must carry the last-block flag.
/// Flagging the final iteration instead put the flag on the block the receiver
/// reads first, ending the chain there and delivering every earlier block into
/// the media stream as payload.
#[test]
fn several_blocks_queued_for_one_stream_all_arrive() {
    let (received, _) = run(
        1,
        &[EmbedTrack {
            stream_id: 1,
            data_type: 7,
            payloads: vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()],
        }],
    );

    let data: Vec<Vec<u8>> = received.iter().map(|r| r.data.clone()).collect();
    assert_eq!(
        data,
        vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()],
        "every queued block must arrive, in order"
    );
}

/// A data-type change on an existing stream renegotiates the pad's caps.
///
/// The pad's caps used to be stamped once at creation and never revisited, so
/// a second data type flowed out of a pad still advertising the first.
#[test]
fn a_data_type_change_renegotiates_the_embedded_pad() {
    init();

    let pipeline = gst::Pipeline::new();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    let demux = gst::ElementFactory::make("efpdemux").build().unwrap();
    pipeline.add_many([&mux, &demux]).unwrap();
    mux.link(&demux).unwrap();

    let media = gst::ElementFactory::make("appsrc")
        .property(
            "caps",
            gst::Caps::builder("application/x-efp-private").build(),
        )
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    pipeline.add(&media).unwrap();
    media.link(&mux).unwrap();
    let media = media.dynamic_cast::<gst_app::AppSrc>().unwrap();

    let embed_caps = |data_type: i32| {
        gst::Caps::builder("application/x-efp-embedded")
            .field("data-type", data_type)
            .field("stream-id", 1i32)
            .build()
    };
    let embed = gst::ElementFactory::make("appsrc")
        .property("caps", embed_caps(7))
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    pipeline.add(&embed).unwrap();
    let templ = mux.pad_template("embed_%u").unwrap();
    let embed_pad = mux.request_pad(&templ, None, None).unwrap();
    embed.static_pad("src").unwrap().link(&embed_pad).unwrap();
    let embed = embed.dynamic_cast::<gst_app::AppSrc>().unwrap();

    /// `(data-type advertised by the pad's caps, buffer bytes)`.
    type TypedBlock = (i32, Vec<u8>);

    let seen: Arc<Mutex<Vec<TypedBlock>>> = Arc::new(Mutex::new(Vec::new()));
    let pipeline_weak = pipeline.downgrade();
    let seen_cb = Arc::clone(&seen);
    demux.connect_pad_added(move |_demux, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();

        if !pad.name().starts_with("embedded_") {
            return;
        }
        let seen = Arc::clone(&seen_cb);
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                let caps = pad.current_caps().unwrap();
                let dt = caps.structure(0).unwrap().get::<i32>("data-type").unwrap();
                let map = buffer.map_readable().unwrap();
                seen.lock().unwrap().push((dt, map.as_slice().to_vec()));
            }
            gst::PadProbeReturn::Ok
        });
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    let push_media = |i: u64| {
        let mut buffer = gst::Buffer::from_slice(format!("media-{i}").into_bytes());
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_mseconds(i * 40));
        media.push_buffer(buffer).unwrap();
    };

    embed
        .push_buffer(gst::Buffer::from_slice(b"type-7"))
        .unwrap();
    for i in 0..6 {
        push_media(i);
    }
    embed.set_caps(Some(&embed_caps(11)));
    embed
        .push_buffer(gst::Buffer::from_slice(b"type-11"))
        .unwrap();
    for i in 6..12 {
        push_media(i);
    }

    let _ = embed.end_of_stream();
    let _ = media.end_of_stream();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                pipeline.set_state(gst::State::Null).unwrap();
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(7, b"type-7".to_vec()), (11, b"type-11".to_vec())],
        "the second block must arrive under its own data type, not the first's"
    );
}

/// Caps without `stream-id` are rejected instead of silently addressing stream
/// 0, which no sink pad is ever allocated — so the data used to be buffered for
/// the life of the pipeline with no error and no output.
#[test]
fn embed_caps_without_addressing_are_rejected() {
    init();

    let pipeline = gst::Pipeline::new();
    let mux = gst::ElementFactory::make("efpmux").build().unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .build()
        .unwrap();
    pipeline.add_many([&mux, &sink]).unwrap();
    mux.link(&sink).unwrap();

    let embed = gst::ElementFactory::make("appsrc")
        // No stream-id, and no data-type.
        .property(
            "caps",
            gst::Caps::builder("application/x-efp-embedded").build(),
        )
        .property("format", gst::Format::Time)
        .property("is-live", false)
        .build()
        .unwrap();
    pipeline.add(&embed).unwrap();
    let templ = mux.pad_template("embed_%u").unwrap();
    let embed_pad = mux.request_pad(&templ, None, None).unwrap();
    embed.static_pad("src").unwrap().link(&embed_pad).unwrap();
    let embed = embed.dynamic_cast::<gst_app::AppSrc>().unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let _ = embed.push_buffer(gst::Buffer::from_slice(b"unaddressed"));

    let bus = pipeline.bus().unwrap();
    let mut errored = false;
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(5)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(_) => {
                errored = true;
                break;
            }
            MessageView::Eos(..) => break,
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();

    assert!(
        errored,
        "embed caps missing stream-id/data-type must fail negotiation, not default to stream 0"
    );
}

/// Data addressed to a stream that carries no media produces no output and no
/// error: it can only leave the muxer on a media frame of that stream, and
/// there is no such stream.
///
/// This is a boundary statement, not a guard for the accompanying fix. The
/// muxer now drops such data with a warning instead of queueing it forever,
/// but both behaviours deliver nothing, and the difference — whether
/// `pending_embeds` grows for the life of the pipeline — is not observable
/// from outside the element. Bounding that memory is covered only by reading
/// the code.
#[test]
fn embedded_data_for_a_stream_with_no_media_produces_nothing() {
    let (received, pads) = run(
        1,
        &[EmbedTrack {
            // Stream 1 is the only media stream; nothing carries stream 9.
            stream_id: 9,
            data_type: 7,
            payloads: vec![b"nowhere-to-go".to_vec()],
        }],
    );

    assert!(
        received.is_empty(),
        "data for a stream with no media cannot be delivered, got {received:?}"
    );
    assert!(
        !pads.iter().any(|n| n.starts_with("embedded_")),
        "no embedded pad should appear, got {pads:?}"
    );
}
