use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

const DEFAULT_BUCKET_TIMEOUT: u32 = 5;
const DEFAULT_HOL_TIMEOUT: u32 = 5;

/// Maximum adapter buffer size (16 MiB). Reject input beyond this to prevent
/// unbounded memory growth from malformed length-prefixed streams.
const MAX_ADAPTER_SIZE: usize = 16 * 1024 * 1024;

/// Maximum allowed fragment size within a length-prefixed message (4 MiB).
const MAX_FRAGMENT_SIZE: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Settings & internal state
// ---------------------------------------------------------------------------

struct Settings {
    bucket_timeout: u32,
    hol_timeout: u32,
    threaded: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bucket_timeout: DEFAULT_BUCKET_TIMEOUT,
            hol_timeout: DEFAULT_HOL_TIMEOUT,
            threaded: false,
        }
    }
}

struct DemuxState {
    receiver: efp::Receiver,
    pending: Arc<Mutex<Vec<efp::SuperFrame>>>,
    pending_embedded: Arc<Mutex<Vec<efp::EmbeddedData>>>,
    adapter: Vec<u8>,
    /// Read cursor into adapter. Data before this offset has been consumed.
    adapter_offset: usize,
}

/// Per-srcpad state tracking.
struct SrcPadState {
    /// True until the first buffer has been pushed on this pad.
    needs_discont: bool,
    /// True until we have seen a keyframe (only relevant for H.264/H.265).
    waiting_for_keyframe: bool,
}

// ---------------------------------------------------------------------------
// Element definition
// ---------------------------------------------------------------------------

pub struct EfpDemux {
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<Option<DemuxState>>,
    srcpads: Mutex<HashMap<u8, gst::Pad>>,
    srcpad_state: Mutex<HashMap<u8, SrcPadState>>,
    /// 0-based counter for pad naming (GStreamer convention: src_0, src_1, ...).
    /// Separate from EFP stream IDs which start at 1.
    next_pad_index: Mutex<u32>,
    embedded_pad: Mutex<Option<gst::Pad>>,
}

unsafe impl Send for EfpDemux {}
unsafe impl Sync for EfpDemux {}

#[glib::object_subclass]
impl ObjectSubclass for EfpDemux {
    const NAME: &'static str = "GstEfpDemux";
    type Type = super::EfpDemux;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                let element = parent.unwrap().downcast_ref::<super::EfpDemux>().unwrap();
                element.imp().sink_chain(pad, buffer)
            })
            .event_function(|pad, parent, event| {
                let element = parent.unwrap().downcast_ref::<super::EfpDemux>().unwrap();
                element.imp().sink_event(pad, event)
            })
            .query_function(|pad, parent, query| {
                let element = parent.unwrap().downcast_ref::<super::EfpDemux>().unwrap();
                element.imp().sink_query(pad, query)
            })
            .build();

        Self {
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(None),
            srcpads: Mutex::new(HashMap::new()),
            srcpad_state: Mutex::new(HashMap::new()),
            next_pad_index: Mutex::new(0),
            embedded_pad: Mutex::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// GObject
// ---------------------------------------------------------------------------

impl ObjectImpl for EfpDemux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("bucket-timeout")
                    .nick("Bucket Timeout")
                    .blurb("Bucket timeout in units of 10 ms")
                    .minimum(1)
                    .maximum(1000)
                    .default_value(DEFAULT_BUCKET_TIMEOUT)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("hol-timeout")
                    .nick("HOL Timeout")
                    .blurb("Head-of-line timeout in units of 10 ms")
                    .minimum(1)
                    .maximum(1000)
                    .default_value(DEFAULT_HOL_TIMEOUT)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("threaded")
                    .nick("Threaded")
                    .blurb("Use a background thread for timeout handling (enables broken frame detection)")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "bucket-timeout" => self.settings.lock().unwrap().bucket_timeout = value.get().unwrap(),
            "hol-timeout" => self.settings.lock().unwrap().hol_timeout = value.get().unwrap(),
            "threaded" => self.settings.lock().unwrap().threaded = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let s = self.settings.lock().unwrap();
        match pspec.name() {
            "bucket-timeout" => s.bucket_timeout.to_value(),
            "hol-timeout" => s.hol_timeout.to_value(),
            "threaded" => s.threaded.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.obj().add_pad(&self.sinkpad).unwrap();
    }
}

impl GstObjectImpl for EfpDemux {}

// ---------------------------------------------------------------------------
// Element
// ---------------------------------------------------------------------------

impl ElementImpl for EfpDemux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "EFP Demuxer",
                "Codec/Demuxer",
                "Reassembles EFP fragments into elementary streams",
                "Eyevinn Technology",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst::Caps::builder("application/x-efp").build();
            let sink_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::new_any();
            let src_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &src_caps,
            )
            .unwrap();

            let embed_caps = gst::Caps::builder("application/x-efp-embedded")
                .field("data-type", gst::IntRange::new(0i32, 255i32))
                .build();
            let embed_template = gst::PadTemplate::new(
                "embedded",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &embed_caps,
            )
            .unwrap();

            vec![sink_template, src_template, embed_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            self.start().map_err(|_| gst::StateChangeError)?;
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::PausedToReady {
            self.stop();
        }

        Ok(ret)
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

impl EfpDemux {
    fn start(&self) -> Result<(), glib::BoolError> {
        let settings = self.settings.lock().unwrap();
        let pending = Arc::new(Mutex::new(Vec::<efp::SuperFrame>::new()));
        let pending_cb = Arc::clone(&pending);
        let pending_embedded = Arc::new(Mutex::new(Vec::<efp::EmbeddedData>::new()));
        let pending_embedded_cb = Arc::clone(&pending_embedded);

        let receiver = efp::Receiver::with_embedded(
            settings.bucket_timeout,
            settings.hol_timeout,
            if settings.threaded {
                efp::ReceiverMode::Threaded
            } else {
                efp::ReceiverMode::RunToCompletion
            },
            move |frame| {
                pending_cb.lock().unwrap().push(frame);
            },
            Some(move |embedded| {
                pending_embedded_cb.lock().unwrap().push(embedded);
            }),
        )
        .map_err(|e| glib::bool_error!("Failed to create EFP receiver: {e}"))?;

        *self.state.lock().unwrap() = Some(DemuxState {
            receiver,
            pending,
            pending_embedded,
            adapter: Vec::new(),
            adapter_offset: 0,
        });
        Ok(())
    }

    fn stop(&self) {
        *self.state.lock().unwrap() = None;
        let pads: Vec<gst::Pad> = self
            .srcpads
            .lock()
            .unwrap()
            .drain()
            .map(|(_, p)| p)
            .collect();
        for pad in pads {
            let _ = self.obj().remove_pad(&pad);
        }
        self.srcpad_state.lock().unwrap().clear();
        *self.next_pad_index.lock().unwrap() = 0;
        if let Some(pad) = self.embedded_pad.lock().unwrap().take() {
            let _ = self.obj().remove_pad(&pad);
        }
    }

    /// Reset the EFP receiver and adapter to discard stale in-flight data.
    /// Called on flush and discontinuity.
    fn reset_receiver(&self) -> std::result::Result<(), gst::FlowError> {
        // Re-create the receiver to flush its internal buckets. This is the
        // only reliable way since the C API has no "flush" method.
        self.stop();
        self.start().map_err(|_| gst::FlowError::Error)?;
        Ok(())
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // DISCONT flag means data discontinuity — old fragments in the
        // receiver's buckets are stale and would produce broken frames.
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            self.reset_receiver()?;
        }

        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let unread = state.adapter.len() - state.adapter_offset;
        if unread.saturating_add(map.as_slice().len()) > MAX_ADAPTER_SIZE {
            state.adapter.clear();
            state.adapter_offset = 0;
            return Err(gst::FlowError::Error);
        }

        state.adapter.extend_from_slice(map.as_slice());

        // Parse length-prefixed fragments: [4-byte BE len][fragment]...
        let buf = &state.adapter;
        let mut pos = state.adapter_offset;
        while buf.len() - pos >= 4 {
            let len =
                u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;

            if len == 0 || len > MAX_FRAGMENT_SIZE {
                state.adapter.clear();
                state.adapter_offset = 0;
                return Err(gst::FlowError::Error);
            }

            if buf.len() - pos < 4 + len {
                break; // incomplete fragment, wait for more data
            }

            let fragment = &buf[pos + 4..pos + 4 + len];
            pos += 4 + len;

            // Non-fatal EFP statuses (duplicate, too-old) are ignored
            let _ = state.receiver.receive_fragment(fragment, 0);
        }

        // Compact: remove consumed bytes only when >50% consumed to amortize memmove.
        state.adapter_offset = pos;
        if state.adapter_offset > state.adapter.len() / 2 {
            state.adapter.drain(..state.adapter_offset);
            state.adapter_offset = 0;
        }

        let frames: Vec<efp::SuperFrame> = state.pending.lock().unwrap().drain(..).collect();
        let embedded: Vec<efp::EmbeddedData> =
            state.pending_embedded.lock().unwrap().drain(..).collect();
        drop(state_guard);

        for frame in frames {
            self.push_frame(frame)?;
        }

        for emb in embedded {
            // Embedded data is best-effort — don't fail the pipeline if
            // downstream isn't linked to the embedded pad.
            let _ = self.push_embedded(emb);
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn push_frame(&self, frame: efp::SuperFrame) -> Result<gst::FlowSuccess, gst::FlowError> {
        let srcpad = self.get_or_create_srcpad(frame.stream_id, frame.data_content, frame.code)?;

        // Check if we should skip this frame (waiting for keyframe).
        let (needs_discont, should_skip) = {
            let mut states = self.srcpad_state.lock().unwrap();
            if let Some(ps) = states.get_mut(&frame.stream_id) {
                if ps.waiting_for_keyframe {
                    if !is_keyframe(&frame) {
                        // Drop non-keyframe data until we see a keyframe.
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    ps.waiting_for_keyframe = false;
                }
                let discont = ps.needs_discont;
                ps.needs_discont = false;
                (discont, false)
            } else {
                (false, false)
            }
        };

        if should_skip {
            return Ok(gst::FlowSuccess::Ok);
        }

        // Update segment start when we see the first PTS for this pad, so that
        // downstream running-time starts near 0 instead of using the raw PTS
        // (which may carry a large offset from the sender). Mirrors tsdemux.
        if frame.pts != u64::MAX {
            let pts = gst::ClockTime::from_nseconds(frame.pts);
            let need_update = {
                let seg = srcpad.sticky_event::<gst::event::Segment>(0);
                match seg {
                    Some(seg_ev) => {
                        let s = seg_ev.segment().clone();
                        let s = s.downcast::<gst::ClockTime>().ok();
                        let start = s
                            .as_ref()
                            .and_then(|s| s.start())
                            .unwrap_or(gst::ClockTime::ZERO);
                        // Update if segment start is still 0 but PTS is large
                        start == gst::ClockTime::ZERO && pts > gst::ClockTime::from_seconds(10)
                    }
                    None => false,
                }
            };
            if need_update {
                let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
                segment.set_start(pts);
                segment.set_position(pts);
                srcpad.push_event(gst::event::Segment::new(&segment));
            }
        }

        let mut buffer = gst::Buffer::from_mut_slice(frame.data);
        {
            let buf_ref = buffer.get_mut().unwrap();
            if frame.pts != u64::MAX {
                buf_ref.set_pts(gst::ClockTime::from_nseconds(frame.pts));
            }
            if frame.dts != u64::MAX {
                buf_ref.set_dts(gst::ClockTime::from_nseconds(frame.dts));
            }
            if needs_discont {
                buf_ref.set_flags(gst::BufferFlags::DISCONT);
            }
            if frame.broken {
                buf_ref.set_flags(gst::BufferFlags::CORRUPTED);
            }
        }

        srcpad.push(buffer)
    }

    fn push_embedded(&self, emb: efp::EmbeddedData) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pad = self.get_or_create_embedded_pad(emb.data_type)?;

        let mut buffer = gst::Buffer::from_mut_slice(emb.data);
        {
            let buf_ref = buffer.get_mut().unwrap();
            if emb.pts != u64::MAX {
                buf_ref.set_pts(gst::ClockTime::from_nseconds(emb.pts));
            }
        }

        pad.push(buffer)
    }

    fn get_or_create_embedded_pad(&self, data_type: u8) -> Result<gst::Pad, gst::FlowError> {
        {
            let guard = self.embedded_pad.lock().unwrap();
            if let Some(pad) = guard.as_ref() {
                return Ok(pad.clone());
            }
        }

        let templ = self.obj().pad_template("embedded").unwrap();
        let pad = gst::Pad::builder_from_template(&templ)
            .name("embedded")
            .build();

        // Activate pad and push sticky events BEFORE add_pad so that
        // pad-added signal handlers can query caps to determine media type.
        pad.set_active(true).map_err(|_| gst::FlowError::Error)?;

        let sid = format!("{:08x}-embedded", self.obj().as_ptr() as usize);
        pad.push_event(gst::event::StreamStart::new(&sid));

        let caps = gst::Caps::builder("application/x-efp-embedded")
            .field("data-type", data_type as i32)
            .build();
        pad.push_event(gst::event::Caps::new(&caps));

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        pad.push_event(gst::event::Segment::new(&segment));

        self.obj()
            .add_pad(&pad)
            .map_err(|_| gst::FlowError::Error)?;

        *self.embedded_pad.lock().unwrap() = Some(pad.clone());
        Ok(pad)
    }

    fn get_or_create_srcpad(
        &self,
        stream_id: u8,
        content_type: u8,
        code: u32,
    ) -> Result<gst::Pad, gst::FlowError> {
        // Fast path: pad already exists.
        {
            let srcpads = self.srcpads.lock().unwrap();
            if let Some(pad) = srcpads.get(&stream_id) {
                return Ok(pad.clone());
            }
        }
        // Slow path: create pad without holding srcpads lock to avoid
        // deadlocks when downstream events re-enter this element.
        let pad_name = {
            let mut idx = self.next_pad_index.lock().unwrap();
            let n = *idx;
            *idx += 1;
            format!("src_{n}")
        };
        let templ = self.obj().pad_template("src_%u").unwrap();
        let pad = gst::Pad::builder_from_template(&templ)
            .name(&pad_name)
            .build();

        // Activate pad and push sticky events BEFORE add_pad so that
        // pad-added signal handlers can query caps to determine media type.
        // This matches the pattern used by tsdemux and other GStreamer demuxers.
        pad.set_active(true).map_err(|_| gst::FlowError::Error)?;

        let sid = format!("{:08x}-{stream_id}", self.obj().as_ptr() as usize);
        pad.push_event(gst::event::StreamStart::new(&sid));

        let caps = caps_for_content_type(content_type, code);
        pad.push_event(gst::event::Caps::new(&caps));

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        pad.push_event(gst::event::Segment::new(&segment));

        self.obj()
            .add_pad(&pad)
            .map_err(|_| gst::FlowError::Error)?;

        // Track per-pad state: mark first buffer as DISCONT and wait for
        // keyframe on video streams (like tsdemux does).
        let wait_keyframe = content_type == efp::CONTENT_H264 || content_type == efp::CONTENT_H265;
        self.srcpad_state.lock().unwrap().insert(
            stream_id,
            SrcPadState {
                needs_discont: true,
                waiting_for_keyframe: wait_keyframe,
            },
        );

        self.srcpads.lock().unwrap().insert(stream_id, pad.clone());
        Ok(pad)
    }

    /// Collect a snapshot of all src pads (including embedded) without holding
    /// the lock during downstream calls.
    fn srcpad_snapshot(&self) -> Vec<gst::Pad> {
        let mut pads: Vec<gst::Pad> = self.srcpads.lock().unwrap().values().cloned().collect();
        if let Some(pad) = self.embedded_pad.lock().unwrap().as_ref() {
            pads.push(pad.clone());
        }
        pads
    }

    /// Drain any pending frames/embedded data from the receiver's background
    /// thread that haven't been pushed downstream yet.
    fn drain_pending(&self) {
        let state_guard = self.state.lock().unwrap();
        let Some(state) = state_guard.as_ref() else {
            return;
        };
        let frames: Vec<efp::SuperFrame> = state.pending.lock().unwrap().drain(..).collect();
        let embedded: Vec<efp::EmbeddedData> =
            state.pending_embedded.lock().unwrap().drain(..).collect();
        drop(state_guard);

        for frame in frames {
            let _ = self.push_frame(frame);
        }
        for emb in embedded {
            let _ = self.push_embedded(emb);
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Caps(_) => true,    // mux caps, not forwarded
            EventView::Segment(_) => true, // demux produces its own segments
            EventView::Eos(_) => {
                // Drain any remaining frames that the receiver's background
                // thread produced after the last sink_chain call.
                self.drain_pending();
                for p in self.srcpad_snapshot() {
                    p.push_event(gst::event::Eos::new());
                }
                true
            }
            EventView::FlushStart(_) => {
                for p in self.srcpad_snapshot() {
                    p.push_event(gst::event::FlushStart::new());
                }
                true
            }
            EventView::FlushStop(e) => {
                // Re-create receiver to flush stale EFP buckets.
                let _ = self.reset_receiver();
                let reset_time = e.resets_time();
                for p in self.srcpad_snapshot() {
                    p.push_event(gst::event::FlushStop::new(reset_time));
                }
                true
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;
        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                // Report the EFP reassembly latency so downstream elements
                // can account for it (mirrors tsdemux latency query handling).
                let settings = self.settings.lock().unwrap();
                let latency_ms = (settings.bucket_timeout + settings.hol_timeout) as u64 * 10;
                let min_latency = gst::ClockTime::from_mseconds(latency_ms);
                q.set(true, min_latency, gst::ClockTime::NONE);
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

/// Check if an EFP SuperFrame contains a keyframe.
/// For H.264/H.265 byte-stream, scan NAL units for IDR/CRA/BLA types.
fn is_keyframe(frame: &efp::SuperFrame) -> bool {
    match frame.data_content {
        efp::CONTENT_H264 => h264_has_idr(&frame.data),
        efp::CONTENT_H265 => h265_has_idr(&frame.data),
        // Non-video content types are always "keyframes" (no dependency chain).
        _ => true,
    }
}

/// Scan H.264 byte-stream for IDR NAL units (type 5).
fn h264_has_idr(data: &[u8]) -> bool {
    for_each_nal(data, |nal| {
        let nal_type = nal[0] & 0x1F;
        nal_type == 5 // IDR slice
    })
}

/// Scan H.265 byte-stream for IDR/CRA/BLA NAL units.
fn h265_has_idr(data: &[u8]) -> bool {
    for_each_nal(data, |nal| {
        if nal.len() < 2 {
            return false;
        }
        let nal_type = (nal[0] >> 1) & 0x3F;
        // IDR_W_RADL(19), IDR_N_LP(20), CRA(21), BLA_W_LP(16), BLA_W_RADL(17), BLA_N_LP(18)
        (16..=21).contains(&nal_type)
    })
}

/// Iterate over NAL units in Annex B byte-stream format, calling `pred` for
/// each NAL body. Returns true if `pred` returns true for any NAL.
fn for_each_nal(data: &[u8], mut pred: impl FnMut(&[u8]) -> bool) -> bool {
    let mut i = 0;
    let len = data.len();
    while i < len {
        // Find start code: 00 00 01 or 00 00 00 01
        if i + 2 < len && data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                i += 3;
            } else if i + 3 < len && data[i + 2] == 0 && data[i + 3] == 1 {
                i += 4;
            } else {
                i += 1;
                continue;
            }
            // i now points to NAL body; find end
            let start = i;
            while i < len {
                if i + 2 < len
                    && data[i] == 0
                    && data[i + 1] == 0
                    && (data[i + 2] == 1 || (i + 3 < len && data[i + 2] == 0 && data[i + 3] == 1))
                {
                    break;
                }
                i += 1;
            }
            if start < len && pred(&data[start..i]) {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

fn caps_for_content_type(ct: u8, code: u32) -> gst::Caps {
    let mut builder = match ct {
        efp::CONTENT_H264 => gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au"),
        efp::CONTENT_H265 => gst::Caps::builder("video/x-h265")
            .field("stream-format", "byte-stream")
            .field("alignment", "au"),
        efp::CONTENT_OPUS => gst::Caps::builder("audio/x-opus")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .field("channel-mapping-family", 0i32),
        _ => gst::Caps::builder("application/x-efp-private").field("content-type", ct as i32),
    };
    if code != 0 {
        builder = builder.field("efp-code", code_to_fourcc(code));
    }
    builder.build()
}

fn code_to_fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}
