use std::sync::{Arc, Mutex};

use efp::{Receiver, ReceiverMode, Sender, SuperFrame};

#[test]
fn send_and_receive_roundtrip() {
    let received: Arc<Mutex<Vec<SuperFrame>>> = Arc::new(Mutex::new(Vec::new()));

    // Set up a receiver in run-to-completion mode.
    let rx_frames = Arc::clone(&received);
    let receiver = Receiver::new(100, 100, ReceiverMode::RunToCompletion, move |frame| {
        rx_frames.lock().unwrap().push(frame);
    })
    .expect("receiver init");

    // Set up a sender whose fragment callback feeds directly into the receiver.
    // We need to share the receiver across the send callback.  Because Receiver
    // is Sync, wrapping in Arc is sufficient.
    let receiver = Arc::new(receiver);
    let rx = Arc::clone(&receiver);
    let sender = Sender::new(1400, move |fragment, _stream_id| {
        // Feed each fragment into the receiver.  `from_source` = 0.
        rx.receive_fragment(fragment, 0).ok();
    })
    .expect("sender init");

    // Send a payload.
    let payload = vec![0xABu8; 5000];
    sender
        .send(
            &payload, 0x01, // privatedata
            1000, // pts
            1000, // dts
            0,    // code
            1,    // stream_id
            0,    // flags
        )
        .expect("send_data");

    // In RunToCompletion mode the superframe callback fires synchronously
    // once all fragments have been received, so the frame should be ready now.
    let frames = received.lock().unwrap();
    assert_eq!(
        frames.len(),
        1,
        "expected exactly one reassembled superframe"
    );

    let frame = &frames[0];
    assert_eq!(frame.data.len(), payload.len());
    assert_eq!(frame.data, payload);
    assert_eq!(frame.data_content, 0x01);
    assert_eq!(frame.pts, 1000);
    assert_eq!(frame.dts, 1000);
    assert_eq!(frame.stream_id, 1);
    assert!(!frame.broken);
}

#[test]
fn version_returns_valid_tuple() {
    let (major, minor) = efp::version();
    assert!(major > 0 || minor > 0, "version should be nonzero");
}

#[test]
fn empty_payload_roundtrip() {
    let received: Arc<Mutex<Vec<SuperFrame>>> = Arc::new(Mutex::new(Vec::new()));

    let rx_frames = Arc::clone(&received);
    let receiver = Receiver::new(100, 100, ReceiverMode::RunToCompletion, move |frame| {
        rx_frames.lock().unwrap().push(frame);
    })
    .expect("receiver init");

    let receiver = Arc::new(receiver);
    let rx = Arc::clone(&receiver);
    let sender = Sender::new(1400, move |fragment, _stream_id| {
        rx.receive_fragment(fragment, 0).ok();
    })
    .expect("sender init");

    // Send an empty payload — should still produce one superframe.
    sender
        .send(&[], 0x01, 0, 0, 0, 1, 0)
        .expect("send empty payload");

    let frames = received.lock().unwrap();
    assert_eq!(frames.len(), 1);
    assert!(frames[0].data.is_empty());
}

#[test]
fn large_payload_exceeding_mtu() {
    let received: Arc<Mutex<Vec<SuperFrame>>> = Arc::new(Mutex::new(Vec::new()));

    let rx_frames = Arc::clone(&received);
    let receiver = Receiver::new(100, 100, ReceiverMode::RunToCompletion, move |frame| {
        rx_frames.lock().unwrap().push(frame);
    })
    .expect("receiver init");

    let receiver = Arc::new(receiver);
    let rx = Arc::clone(&receiver);

    let fragment_count = Arc::new(Mutex::new(0u32));
    let fc = Arc::clone(&fragment_count);

    let sender = Sender::new(1400, move |fragment, _stream_id| {
        *fc.lock().unwrap() += 1;
        rx.receive_fragment(fragment, 0).ok();
    })
    .expect("sender init");

    // 100 KB payload — must be fragmented into many pieces.
    let payload = vec![0x42u8; 100_000];
    sender
        .send(&payload, 0x01, 5000, 5000, 0, 1, 0)
        .expect("send large payload");

    let count = *fragment_count.lock().unwrap();
    assert!(
        count > 1,
        "large payload should produce multiple fragments, got {count}"
    );

    let frames = received.lock().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, payload);
}

#[test]
fn multiple_streams_distinguished() {
    let received: Arc<Mutex<Vec<SuperFrame>>> = Arc::new(Mutex::new(Vec::new()));

    let rx_frames = Arc::clone(&received);
    let receiver = Receiver::new(100, 100, ReceiverMode::RunToCompletion, move |frame| {
        rx_frames.lock().unwrap().push(frame);
    })
    .expect("receiver init");

    let receiver = Arc::new(receiver);
    let rx = Arc::clone(&receiver);
    let sender = Sender::new(1400, move |fragment, _stream_id| {
        rx.receive_fragment(fragment, 0).ok();
    })
    .expect("sender init");

    let payload_a = vec![0xAAu8; 500];
    let payload_b = vec![0xBBu8; 600];
    sender
        .send(&payload_a, 0x01, 100, 100, 0, 1, 0)
        .expect("send stream 1");
    sender
        .send(&payload_b, 0x02, 200, 200, 0, 2, 0)
        .expect("send stream 2");

    let frames = received.lock().unwrap();
    assert_eq!(frames.len(), 2);

    let s1 = frames.iter().find(|f| f.stream_id == 1).expect("stream 1");
    assert_eq!(s1.data, payload_a);
    assert_eq!(s1.pts, 100);

    let s2 = frames.iter().find(|f| f.stream_id == 2).expect("stream 2");
    assert_eq!(s2.data, payload_b);
    assert_eq!(s2.pts, 200);
}

#[test]
fn receiver_rejects_garbage_fragment() {
    let received: Arc<Mutex<Vec<SuperFrame>>> = Arc::new(Mutex::new(Vec::new()));

    let rx_frames = Arc::clone(&received);
    let receiver = Receiver::new(100, 100, ReceiverMode::RunToCompletion, move |frame| {
        rx_frames.lock().unwrap().push(frame);
    })
    .expect("receiver init");

    // Feed pure garbage — should not panic, may return error or be silently ignored.
    let garbage = vec![0xFF; 100];
    let _ = receiver.receive_fragment(&garbage, 0);

    // No valid superframe should be produced from garbage.
    let frames = received.lock().unwrap();
    assert!(
        frames.is_empty(),
        "garbage should not produce a valid frame"
    );
}

#[test]
fn mtu_validation() {
    // Too small
    let result = Sender::new(100, |_, _| {});
    assert_eq!(result.err(), Some(efp::EfpError::FrameSizeMismatch));

    // Too large
    let result = Sender::new(100_000, |_, _| {});
    assert_eq!(result.err(), Some(efp::EfpError::FrameSizeMismatch));

    // Just right
    let _sender = Sender::new(1400, |_, _| {}).expect("valid MTU");
}

#[test]
fn embedded_data_rejects_oversized() {
    let big = vec![0u8; 70_000]; // > u16::MAX
    let frame = vec![0u8; 100];
    let result = efp::add_embedded_data(&big, &frame, 1, true);
    assert_eq!(result.unwrap_err(), efp::EfpError::TooLargeEmbeddedData);
}

#[test]
fn receive_fragment_rejects_empty() {
    let receiver =
        efp::Receiver::new(100, 100, ReceiverMode::RunToCompletion, |_| {}).expect("receiver init");
    let result = receiver.receive_fragment(&[], 0);
    assert_eq!(result.unwrap_err(), efp::EfpError::FrameSizeMismatch);
}

#[test]
fn add_embedded_data_produces_larger_buffer() {
    let frame_data = vec![0xAB; 1000];
    let embedded = b"test-metadata";

    let combined =
        efp::add_embedded_data(embedded, &frame_data, 1, true).expect("add_embedded_data");
    assert!(
        combined.len() > frame_data.len() + embedded.len(),
        "combined buffer should include header overhead"
    );
}

#[test]
fn embedded_data_roundtrip() {
    let frame_payload = b"video-frame-data";
    let embed_payload = b"metadata-payload";

    let combined = efp::add_embedded_data(embed_payload, frame_payload, 42, true).unwrap();

    let fragments = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb = Arc::clone(&fragments);
    let sender = efp::Sender::new(1400, move |fragment, _sid| {
        frag_cb.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();
    sender
        .send(&combined, 0x01, 5000, 5000, 0, 1, efp::FLAG_INLINE_PAYLOAD)
        .unwrap();

    let frags = fragments.lock().unwrap().clone();

    let frames = Arc::new(Mutex::new(Vec::<efp::SuperFrame>::new()));
    let frames_cb = Arc::clone(&frames);
    let embedded = Arc::new(Mutex::new(Vec::<efp::EmbeddedData>::new()));
    let embedded_cb = Arc::clone(&embedded);

    let receiver = efp::Receiver::with_embedded(
        5,
        5,
        efp::ReceiverMode::RunToCompletion,
        move |frame| {
            frames_cb.lock().unwrap().push(frame);
        },
        Some(move |emb: efp::EmbeddedData| {
            embedded_cb.lock().unwrap().push(emb);
        }),
    )
    .unwrap();

    for frag in &frags {
        let _ = receiver.receive_fragment(frag, 0);
    }

    let f = frames.lock().unwrap();
    let e = embedded.lock().unwrap();
    assert_eq!(f.len(), 1, "should get one frame");
    assert_eq!(
        e.len(),
        1,
        "should get one embedded data (got {} frames, {} embedded)",
        f.len(),
        e.len()
    );
    assert_eq!(e[0].data, embed_payload);
    assert_eq!(e[0].data_type, 42);
}

#[test]
fn broken_frame_detection() {
    // Fragment a large payload, drop one fragment, send the rest + a complete frame.
    // The receiver should deliver the incomplete frame with broken=true.
    let fragments = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb = Arc::clone(&fragments);
    let sender = Sender::new(1400, move |fragment, _sid| {
        frag_cb.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();

    let payload = vec![0xAB; 10_000];
    sender.send(&payload, 0x01, 1000, 1000, 0, 1, 0).unwrap();

    let frags = fragments.lock().unwrap().clone();
    assert!(frags.len() > 2, "need multiple fragments");

    // Send a second complete frame.
    let fragments2 = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frag_cb2 = Arc::clone(&fragments2);
    let sender2 = Sender::new(1400, move |fragment, _sid| {
        frag_cb2.lock().unwrap().push(fragment.to_vec());
    })
    .unwrap();
    sender2
        .send(&[0xCD; 500], 0x01, 2000, 2000, 0, 1, 0)
        .unwrap();
    let frags2 = fragments2.lock().unwrap().clone();

    let received: Arc<Mutex<Vec<SuperFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let rx = Arc::clone(&received);
    let receiver = Receiver::new(1, 1, ReceiverMode::Threaded, move |frame| {
        rx.lock().unwrap().push(frame);
    })
    .unwrap();

    // Feed incomplete frame (skip fragment 1).
    for (i, frag) in frags.iter().enumerate() {
        if i == 1 {
            continue;
        }
        let _ = receiver.receive_fragment(frag, 0);
    }

    // Sleep to let timeout expire (1 * 10ms = 10ms timeout, wait 200ms).
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Feed complete frame.
    for frag in &frags2 {
        let _ = receiver.receive_fragment(frag, 0);
    }

    // Wait for threaded processing.
    std::thread::sleep(std::time::Duration::from_millis(100));

    let frames = received.lock().unwrap();
    assert!(!frames.is_empty(), "should get at least one frame");
    let has_broken = frames.iter().any(|f| f.broken);
    assert!(has_broken, "should have a broken frame");
}
