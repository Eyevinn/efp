//! Tests for EFP's embedded-data channel, and specifically for attributing
//! embedded data to the media stream it arrived on.
//!
//! The C API's embedded-data callback takes `(data, size, data_type, pts, ctx)`
//! and has no stream-ID parameter, even though the C++ side has the carrying
//! frame in hand when it fires. `efp::split_embedded_data` extracts the blocks
//! on the Rust side of the boundary instead, so the frame and its embedded data
//! stay together and each block can name its stream.

use std::sync::{Arc, Mutex};

use efp::{Receiver, ReceiverMode, SuperFrame};

/// Feed `sends` through a sender and back into a receiver, returning everything
/// that came out. Each send is `(payload, stream_id, flags)`.
fn roundtrip(sends: &[(Vec<u8>, u8, u8)]) -> (Vec<efp::SuperFrame>, Vec<efp::EmbeddedData>) {
    let fragments = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb = Arc::clone(&fragments);
    let sender = efp::Sender::new(1400, move |fragment, _sid| {
        frag_cb.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();

    for (payload, stream_id, flags) in sends {
        sender
            .send(payload, 0x01, 5000, 5000, 0, *stream_id, *flags)
            .unwrap();
    }

    let frames = Arc::new(Mutex::new(Vec::new()));
    let frames_cb = Arc::clone(&frames);
    let embedded = Arc::new(Mutex::new(Vec::new()));
    let embedded_cb = Arc::clone(&embedded);

    let receiver = efp::Receiver::with_embedded(
        5,
        5,
        efp::ReceiverMode::RunToCompletion,
        move |frame| frames_cb.lock().unwrap().push(frame),
        Some(move |emb: efp::EmbeddedData| embedded_cb.lock().unwrap().push(emb)),
    )
    .unwrap();

    for frag in fragments.lock().unwrap().iter() {
        let _ = receiver.receive_fragment(frag, 0);
    }

    let f = frames.lock().unwrap().clone();
    let e = embedded.lock().unwrap().clone();
    (f, e)
}

/// The capability this change exists for: two media streams each carrying their
/// own embedded data, with every block attributable to its own stream.
#[test]
fn embedded_data_from_two_streams_is_attributed_separately() {
    let video = efp::add_embedded_data(b"describes-video", b"video-frame", 7, true).unwrap();
    let audio = efp::add_embedded_data(b"describes-audio", b"audio-frame", 9, true).unwrap();

    let (frames, embedded) = roundtrip(&[
        (video, 1, efp::FLAG_INLINE_PAYLOAD),
        (audio, 2, efp::FLAG_INLINE_PAYLOAD),
    ]);

    assert_eq!(frames.len(), 2, "both media frames should arrive");
    assert_eq!(embedded.len(), 2, "both embedded blocks should arrive");

    let mut seen: Vec<(u8, u8, Vec<u8>)> = embedded
        .iter()
        .map(|e| (e.stream_id, e.data_type, e.data.clone()))
        .collect();
    seen.sort();

    assert_eq!(
        seen,
        vec![
            (1, 7, b"describes-video".to_vec()),
            (2, 9, b"describes-audio".to_vec()),
        ],
        "each block must report the stream it rode in on"
    );
}

/// Several blocks on one frame arrive in wire order, all on the same stream.
#[test]
fn several_embedded_blocks_on_one_frame_all_arrive() {
    // `add_embedded_data` prepends, so the block written first ends up last on
    // the wire and the *first* call is the one that must carry the last-block
    // flag. Getting that backwards truncates the chain at the first block and
    // silently delivers the rest as media.
    let combined = efp::add_embedded_data(b"second", b"media", 20, true).unwrap();
    let combined = efp::add_embedded_data(b"first", &combined, 10, false).unwrap();

    let (frames, embedded) = roundtrip(&[(combined, 4, efp::FLAG_INLINE_PAYLOAD)]);

    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].data, b"media",
        "every embedded block must be stripped, not just the first"
    );
    assert_eq!(
        embedded
            .iter()
            .map(|e| (e.stream_id, e.data_type, e.data.clone()))
            .collect::<Vec<_>>(),
        vec![(4, 10, b"first".to_vec()), (4, 20, b"second".to_vec())]
    );
}

/// Pins the `ElasticEmbeddedHeader` wire layout that `split_embedded_data`
/// decodes. The C++ sender memcpy's the struct, so the layout is the platform's
/// rather than a declared wire format; this asserts that what the C library
/// writes is what we read back.
#[test]
fn embedded_header_roundtrips_through_the_c_library() {
    let payload = b"abcdefghij";
    let combined = efp::add_embedded_data(payload, b"media", 42, true).unwrap();

    let (blocks, payload_start) =
        efp::split_embedded_data(&combined).expect("the C library's own header must parse");

    assert_eq!(blocks, vec![(42u8, payload.to_vec())]);
    assert_eq!(
        &combined[payload_start..],
        b"media",
        "the media payload must start where the preamble ends"
    );
    assert_eq!(
        payload_start,
        combined.len() - b"media".len(),
        "header plus data must account for the whole preamble"
    );
}

/// A frame flagged as carrying embedded data but whose preamble is garbage is
/// dropped rather than delivered with the garbage prepended to the media.
#[test]
fn a_malformed_embedded_preamble_drops_the_frame() {
    // Type byte 0 is `illegal` in the C++ enum.
    let bogus = vec![
        0u8, 0, 4, 0, b'x', b'y', b'z', b'w', b'm', b'e', b'd', b'i', b'a',
    ];

    assert!(
        efp::split_embedded_data(&bogus).is_err(),
        "type 0 must be rejected"
    );

    let (frames, embedded) = roundtrip(&[(bogus, 1, efp::FLAG_INLINE_PAYLOAD)]);

    assert!(
        embedded.is_empty(),
        "nothing should be reported as embedded"
    );
    assert!(
        frames.is_empty(),
        "the frame boundary is unknown, so the frame must not be delivered"
    );
}

/// A block whose declared size runs past the end of the frame is rejected
/// rather than read out of bounds.
#[test]
fn an_embedded_block_longer_than_the_frame_is_rejected() {
    let mut truncated = efp::add_embedded_data(b"payload", b"media", 5, true).unwrap();
    truncated.truncate(6);

    assert!(
        efp::split_embedded_data(&truncated).is_err(),
        "a block that runs past the end of the frame must be rejected"
    );
}

/// A broken frame's preamble cannot be trusted, so it is not parsed. This
/// matches the C++, which skips extraction when the frame is marked broken.
#[test]
fn embedded_data_is_not_extracted_from_a_broken_frame() {
    let combined = efp::add_embedded_data(b"metadata", &vec![0u8; 4000], 3, true).unwrap();

    let fragments = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb = Arc::clone(&fragments);
    let sender = efp::Sender::new(500, move |fragment, _sid| {
        frag_cb.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();
    sender
        .send(&combined, 0x01, 5000, 5000, 0, 1, efp::FLAG_INLINE_PAYLOAD)
        .unwrap();

    let frames = Arc::new(Mutex::new(Vec::<SuperFrame>::new()));
    let frames_cb = Arc::clone(&frames);
    let embedded = Arc::new(Mutex::new(Vec::<efp::EmbeddedData>::new()));
    let embedded_cb = Arc::clone(&embedded);
    let receiver = efp::Receiver::with_embedded(
        1,
        1,
        efp::ReceiverMode::RunToCompletion,
        move |frame| frames_cb.lock().unwrap().push(frame),
        Some(move |emb: efp::EmbeddedData| embedded_cb.lock().unwrap().push(emb)),
    )
    .unwrap();

    // Drop a middle fragment so the frame is reassembled as broken.
    let frags = fragments.lock().unwrap().clone();
    assert!(frags.len() > 2, "payload must span several fragments");
    for (i, frag) in frags.iter().enumerate() {
        if i == 1 {
            continue;
        }
        let _ = receiver.receive_fragment(frag, 0);
    }
    drop(receiver);

    assert!(
        embedded.lock().unwrap().is_empty(),
        "a broken frame's preamble must not be parsed"
    );
}

/// Without an embedded callback the frame is delivered exactly as the sender
/// built it, preamble included — matching the C library's behaviour when no
/// embedded callback is registered.
#[test]
fn without_an_embedded_callback_the_preamble_stays_on_the_frame() {
    let combined = efp::add_embedded_data(b"metadata", b"media", 3, true).unwrap();

    let fragments = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb = Arc::clone(&fragments);
    let sender = efp::Sender::new(1400, move |fragment, _sid| {
        frag_cb.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();
    sender
        .send(&combined, 0x01, 5000, 5000, 0, 1, efp::FLAG_INLINE_PAYLOAD)
        .unwrap();

    let frames = Arc::new(Mutex::new(Vec::<SuperFrame>::new()));
    let frames_cb = Arc::clone(&frames);
    let receiver = Receiver::new(5, 5, ReceiverMode::RunToCompletion, move |frame| {
        frames_cb.lock().unwrap().push(frame);
    })
    .unwrap();

    for frag in fragments.lock().unwrap().iter() {
        let _ = receiver.receive_fragment(frag, 0);
    }

    let f = frames.lock().unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].data, combined);
}
