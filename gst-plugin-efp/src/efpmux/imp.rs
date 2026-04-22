use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

const DEFAULT_MTU: u32 = 1400;

/// Controls what value is written as the EFP wire PTS/DTS for each frame.
///
/// # Clock-time, not TAI specifically
///
/// EFP wire timestamps are **pipeline-clock values** — whatever the pipeline
/// clock reads at the moment of stamping. Whether that's TAI, UTC, PTP, NTP,
/// or monotonic depends on how the user configured the pipeline clock, and
/// how the host OS clock is disciplined (chrony, `ptp4l`/`phc2sys`, etc.).
/// EFP makes no claim about the interpretation; the "absolute" modes below
/// simply mean "matches the pipeline clock's reading at stamp time".
///
/// In the **common deployment** both ends run `clock-type=TAI` with chrony
/// or PTP disciplining the OS clock, so EFP timestamps end up being
/// absolute TAI. But the mechanism works for any clock choice as long as
/// sender and receiver share a time domain.
///
/// # Why these modes exist
///
/// The simplest way to put absolute pipeline-clock time on the wire is to
/// run the pipeline in direct-media-timing mode (`base_time=0`, so
/// `buffer.pts == clock.time()`). That breaks many pipelines though
/// (MPEG-TS demuxers, WHEP session sinks, aggregators assuming running-time
/// starts near 0). `AbsoluteFromRunningTime` bridges the gap: it reads
/// running-time PTS from the buffer and adds the pipeline's `base_time`
/// to produce the absolute value on the wire, while the rest of the
/// pipeline keeps working in normal running-time mode.
///
/// # Modes
///
/// - `Buffer` (default): pass `buffer.pts` / `buffer.dts` through unchanged.
///   Keeps the legacy wire semantics (running-time). Use this when feeding a
///   legacy receiver, or when the pipeline is in direct-media-timing mode
///   (where `buffer.pts` already *is* the absolute pipeline-clock time).
/// - `AbsoluteFromRunningTime` (recommended for cross-node sync): add the
///   pipeline's `base_time` to the incoming running-time PTS/DTS, producing
///   an absolute pipeline-clock timestamp on the wire. Pair with efpdemux's
///   `RebaseToRunningTime` on the receiver side for cross-node alignment
///   without forcing the pipeline into direct-media-timing mode.
#[derive(Debug, Eq, PartialEq, Clone, Copy, Default, glib::Enum)]
#[enum_type(name = "GstEfpMuxTimestampMode")]
#[repr(i32)]
pub enum TimestampMode {
    #[enum_value(name = "Passthrough buffer PTS/DTS", nick = "buffer")]
    Buffer = 0,
    #[enum_value(
        name = "Running time + pipeline base_time (absolute pipeline-clock)",
        nick = "absolute-from-running-time"
    )]
    #[default]
    AbsoluteFromRunningTime = 1,
}

// ---------------------------------------------------------------------------
// Settings & internal state
// ---------------------------------------------------------------------------

struct Settings {
    mtu: u32,
    timestamp_mode: TimestampMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mtu: DEFAULT_MTU,
            timestamp_mode: TimestampMode::default(),
        }
    }
}

struct PadState {
    stream_id: u8,
    content_type: u8,
    code: u32,
    eos: bool,
}

struct EmbedPadState {
    stream_id: u8,
    data_type: u8,
}

struct MuxState {
    sender: efp::Sender,
    /// Contiguous buffer of length-prefixed fragments: [4-byte BE len][data]...
    /// Avoids per-fragment heap allocation in the hot path.
    pending: Arc<Mutex<Vec<u8>>>,
}

/// Pending embedded data waiting to be prepended to the next frame on the
/// associated stream.
struct PendingEmbed {
    data: Vec<u8>,
    data_type: u8,
}

// ---------------------------------------------------------------------------
// Element definition
// ---------------------------------------------------------------------------

/// Allocates stream IDs 1..=255 and recycles released ones.
struct StreamIdAllocator {
    next: u8,
    free: Vec<u8>,
}

impl StreamIdAllocator {
    fn new() -> Self {
        Self {
            next: 1,
            free: Vec::new(),
        }
    }

    fn allocate(&mut self) -> Option<u8> {
        if let Some(id) = self.free.pop() {
            return Some(id);
        }
        if self.next == 0 {
            return None; // all 255 IDs exhausted and none returned
        }
        let id = self.next;
        self.next = if id == 255 { 0 } else { id + 1 };
        Some(id)
    }

    fn release(&mut self, id: u8) {
        debug_assert_ne!(id, 0, "stream ID 0 is reserved");
        self.free.push(id);
    }
}

pub struct EfpMux {
    srcpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<Option<MuxState>>,
    pads: Mutex<HashMap<gst::Pad, PadState>>,
    embed_pads: Mutex<HashMap<gst::Pad, EmbedPadState>>,
    /// Pending embedded data per stream ID, consumed by the next `sink_chain`.
    pending_embeds: Mutex<HashMap<u8, Vec<PendingEmbed>>>,
    stream_ids: Mutex<StreamIdAllocator>,
    /// 0-based counter for pad naming (GStreamer convention: sink_0, sink_1, ...).
    /// Separate from EFP stream IDs which start at 1.
    next_pad_index: Mutex<u32>,
    src_setup_done: AtomicBool,
}

// The EFP sender uses internal locking and our Mutex-guarded state
// ensures correct access — safe to use from multiple streaming threads.
unsafe impl Send for EfpMux {}
unsafe impl Sync for EfpMux {}

#[glib::object_subclass]
impl ObjectSubclass for EfpMux {
    const NAME: &'static str = "GstEfpMux";
    type Type = super::EfpMux;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                let element = parent.unwrap().downcast_ref::<super::EfpMux>().unwrap();
                element.imp().src_event(pad, event)
            })
            .build();

        Self {
            srcpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(None),
            pads: Mutex::new(HashMap::new()),
            embed_pads: Mutex::new(HashMap::new()),
            pending_embeds: Mutex::new(HashMap::new()),
            stream_ids: Mutex::new(StreamIdAllocator::new()),
            next_pad_index: Mutex::new(0),
            src_setup_done: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// GObject
// ---------------------------------------------------------------------------

impl ObjectImpl for EfpMux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("mtu")
                    .nick("MTU")
                    .blurb("Maximum Transmission Unit for EFP fragments (bytes)")
                    .minimum(100)
                    .maximum(65535)
                    .default_value(DEFAULT_MTU)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<TimestampMode>("timestamp-mode")
                    .nick("Timestamp mode")
                    .blurb(
                        "How to derive the wire PTS/DTS for each frame. \
                        'buffer' (default) passes the incoming buffer PTS/DTS \
                        through unchanged — correct when the pipeline is in \
                        direct-media-timing mode, or when the receiver does \
                        not need absolute time. \
                        'absolute-from-running-time' adds the pipeline's \
                        base_time to the incoming timestamps, so the wire \
                        carries absolute pipeline-clock values (which is TAI \
                        when the pipeline runs on a TAI system clock, etc.). \
                        Use this on any pipeline that is not in \
                        direct-media-timing mode when you want receivers to \
                        see absolute sender clock-time.",
                    )
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "mtu" => self.settings.lock().unwrap().mtu = value.get().unwrap(),
            "timestamp-mode" => self.settings.lock().unwrap().timestamp_mode = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "mtu" => self.settings.lock().unwrap().mtu.to_value(),
            "timestamp-mode" => self.settings.lock().unwrap().timestamp_mode.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.obj().add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for EfpMux {}

// ---------------------------------------------------------------------------
// Element
// ---------------------------------------------------------------------------

impl ElementImpl for EfpMux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "EFP Muxer",
                "Codec/Muxer",
                "Muxes elementary streams into an EFP fragment bytestream",
                "Eyevinn Technology",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_caps = gst::Caps::builder("application/x-efp").build();
            let src_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            // Enumerate accepted sink caps. Video types are pinned to
            // byte-stream + AU alignment so upstream parsers (h264parse /
            // h265parse) negotiate the wire format we actually transport —
            // the receiver's efpdemux unconditionally advertises byte-stream
            // on its src pads, so avc-framed bytes break h264parse downstream.
            // Opus is natively recognized (CONTENT_OPUS). Anything else must
            // be wrapped as `application/x-efp-private` by the caller; that
            // single choice replaces the former `ANY` to make the wire
            // contract explicit at the plugin boundary.
            let sink_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", "byte-stream")
                        .field("alignment", "au")
                        .build(),
                )
                .structure(
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", "byte-stream")
                        .field("alignment", "au")
                        .build(),
                )
                .structure(gst::Structure::builder("audio/x-opus").build())
                .structure(gst::Structure::builder("application/x-efp-private").build())
                .build();
            let sink_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &sink_caps,
            )
            .unwrap();

            let embed_caps = gst::Caps::builder("application/x-efp-embedded")
                .field("data-type", gst::IntRange::new(0i32, 255i32))
                .field("stream-id", gst::IntRange::new(0i32, 255i32))
                .build();
            let embed_template = gst::PadTemplate::new(
                "embed_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &embed_caps,
            )
            .unwrap();

            vec![src_template, sink_template, embed_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let templ_name = templ.name_template();
        if templ_name == "embed_%u" {
            return self.request_embed_pad(templ, name, caps);
        }

        let stream_id = self.stream_ids.lock().unwrap().allocate()?;

        let pad_name = name.map(String::from).unwrap_or_else(|| {
            let mut idx = self.next_pad_index.lock().unwrap();
            let n = *idx;
            *idx += 1;
            format!("sink_{n}")
        });

        let pad = gst::Pad::builder_from_template(templ)
            .name(pad_name)
            .chain_function(|pad, parent, buffer| {
                let element = parent.unwrap().downcast_ref::<super::EfpMux>().unwrap();
                element.imp().sink_chain(pad, buffer)
            })
            .event_function(|pad, parent, event| {
                let element = parent.unwrap().downcast_ref::<super::EfpMux>().unwrap();
                element.imp().sink_event(pad, event)
            })
            .build();

        self.pads.lock().unwrap().insert(
            pad.clone(),
            PadState {
                stream_id,
                content_type: 0x01, // updated on caps event
                code: 0,
                eos: false,
            },
        );

        self.obj().add_pad(&pad).ok()?;
        pad.set_active(true).ok()?;
        Some(pad)
    }

    fn release_pad(&self, pad: &gst::Pad) {
        if let Some(ps) = self.pads.lock().unwrap().remove(pad) {
            self.stream_ids.lock().unwrap().release(ps.stream_id);
        }
        self.embed_pads.lock().unwrap().remove(pad);
        let _ = self.obj().remove_pad(pad);
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

impl EfpMux {
    fn start(&self) -> Result<(), glib::BoolError> {
        let mtu = self.settings.lock().unwrap().mtu;
        let pending = Arc::new(Mutex::new(Vec::<u8>::new()));
        let pending_cb = Arc::clone(&pending);

        let sender = efp::Sender::new(mtu as u64, move |fragment, _stream_id| {
            let mut buf = pending_cb.lock().unwrap();
            buf.extend_from_slice(&(fragment.len() as u32).to_be_bytes());
            buf.extend_from_slice(fragment);
        })
        .map_err(|e| glib::bool_error!("Failed to create EFP sender: {e}"))?;

        *self.state.lock().unwrap() = Some(MuxState { sender, pending });
        self.src_setup_done.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&self) {
        *self.state.lock().unwrap() = None;
        self.src_setup_done.store(false, Ordering::SeqCst);
    }

    /// Push stream-start / caps / segment on the src pad (once).
    fn ensure_src_setup(&self) {
        if self.src_setup_done.swap(true, Ordering::SeqCst) {
            return;
        }

        let stream_id = format!("{:08x}", self.obj().as_ptr() as usize);
        self.srcpad
            .push_event(gst::event::StreamStart::new(&stream_id));

        let caps = gst::Caps::builder("application/x-efp").build();
        self.srcpad.push_event(gst::event::Caps::new(&caps));

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        self.srcpad.push_event(gst::event::Segment::new(&segment));
    }

    /// Collect a snapshot of sink pads without holding the lock.
    fn sink_pad_snapshot(&self) -> Vec<gst::Pad> {
        self.pads.lock().unwrap().keys().cloned().collect()
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.ensure_src_setup();

        let state_guard = self.state.lock().unwrap();
        let state = match state_guard.as_ref() {
            Some(s) => s,
            None => return Err(gst::FlowError::NotNegotiated),
        };

        let (stream_id, content_type, code) = {
            let pads = self.pads.lock().unwrap();
            let ps = match pads.get(pad) {
                Some(ps) => ps,
                None => return Err(gst::FlowError::Error),
            };
            (ps.stream_id, ps.content_type, ps.code)
        };

        // EFP reserves u64::MAX for PTS/DTS; use 0 when GStreamer has no timestamp.
        // In AbsoluteFromRunningTime mode, add the pipeline's base_time to map
        // running-time buffer PTS/DTS back to absolute pipeline-clock values
        // on the wire. With a TAI-configured pipeline clock (chrony-/PTP-
        // disciplined), those are absolute TAI — but the mechanism itself is
        // clock-agnostic: it embeds whatever clock-time the local pipeline
        // runs on. See efpdemux `RebaseToRunningTime` for the inverse at the
        // receiver.
        let timestamp_mode = self.settings.lock().unwrap().timestamp_mode;
        let base_time_ns = match timestamp_mode {
            TimestampMode::Buffer => 0,
            TimestampMode::AbsoluteFromRunningTime => {
                self.obj().base_time().map(|b| b.nseconds()).unwrap_or(0)
            }
        };
        // When the buffer has no DTS (common for raw video sources) we must
        // derive one from PTS — EFP's protocol check rejects frames where
        // `pts - dts >= u32::MAX` (~4.3s). Defaulting DTS to 0 worked before
        // only because PTS was small (running-time). Now that PTS can carry
        // absolute pipeline-clock values, `0` gives a huge diff. Using PTS as
        // the fallback DTS makes the diff zero, which is always safe.
        let buffer_pts = buffer.pts();
        let buffer_dts = buffer.dts().or(buffer_pts);
        let pts = buffer_pts.map_or(0, |t| t.nseconds().saturating_add(base_time_ns));
        let dts = buffer_dts.map_or(0, |t| t.nseconds().saturating_add(base_time_ns));
        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

        // Prepend any pending embedded data for this stream.
        let frame_data = {
            let mut embeds = self.pending_embeds.lock().unwrap();
            if let Some(pending) = embeds.remove(&stream_id) {
                let mut combined = map.as_slice().to_vec();
                for (i, emb) in pending.iter().enumerate() {
                    let is_last = i == pending.len() - 1;
                    combined = efp::add_embedded_data(&emb.data, &combined, emb.data_type, is_last)
                        .map_err(|_| gst::FlowError::Error)?;
                }
                Some(combined)
            } else {
                None
            }
        };

        let send_data = frame_data.as_deref().unwrap_or(map.as_slice());
        let flags: u8 = if frame_data.is_some() {
            efp::FLAG_INLINE_PAYLOAD
        } else {
            0
        };
        if let Err(_e) =
            state
                .sender
                .send(send_data, content_type, pts, dts, code, stream_id, flags)
        {
            return Err(gst::FlowError::Error);
        }

        let data = {
            let mut pending = state.pending.lock().unwrap();
            if pending.is_empty() {
                return Ok(gst::FlowSuccess::Ok);
            }
            std::mem::take(&mut *pending)
        };
        drop(state_guard);

        // Push all fragments as one GStreamer buffer — single allocation, single push.
        let mut outbuf = gst::Buffer::from_mut_slice(data);
        {
            let outref = outbuf.get_mut().unwrap();
            outref.set_pts(buffer.pts());
            outref.set_dts(buffer.dts());
        }
        self.srcpad.push(outbuf)
    }

    /// Handle upstream events arriving on the src pad (from downstream).
    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Qos(_) => {
                // Swallow QoS events — the muxer has no meaningful way to
                // translate them back to individual input streams, and
                // forwarding them causes upstream sources to throttle/freeze.
                true
            }
            EventView::CustomUpstream(e) => {
                let s = e.structure().unwrap();
                if s.name().as_str() == "GstForceKeyUnit" {
                    // Forward force-key-unit to all sink pads so upstream
                    // encoders can produce a keyframe (mirrors mpegtsmux).
                    for sink_pad in self.sink_pad_snapshot() {
                        sink_pad.push_event(event.clone());
                    }
                    true
                } else {
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                }
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Caps(e) => {
                if let Some(s) = e.caps().structure(0) {
                    let ct = content_type_from_caps(s.name().as_str());
                    let code = s
                        .get::<&str>("efp-code")
                        .ok()
                        .and_then(|v| {
                            let b = v.as_bytes();
                            if b.len() == 4 {
                                Some(efp::code(b[0], b[1], b[2], b[3]))
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if let Some(ps) = self.pads.lock().unwrap().get_mut(pad) {
                        ps.content_type = ct;
                        ps.code = code;
                    }
                }
                true // don't forward — the mux produces its own caps
            }
            EventView::Segment(_) => true, // don't forward — mux has its own segment
            EventView::FlushStart(_) => self.srcpad.push_event(gst::event::FlushStart::new()),
            EventView::FlushStop(_e) => {
                // Reset the EFP sender state by re-creating it.
                let _ = self.start();
                self.srcpad.push_event(gst::event::FlushStop::new(true))
            }
            EventView::Eos(_) => {
                let all_eos = {
                    let mut pads = self.pads.lock().unwrap();
                    if let Some(ps) = pads.get_mut(pad) {
                        ps.eos = true;
                    }
                    pads.values().all(|ps| ps.eos)
                };
                if all_eos {
                    self.srcpad.push_event(event)
                } else {
                    true
                }
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn request_embed_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        // Extract stream-id and data-type from caps.
        let (stream_id, data_type) = caps
            .and_then(|c| c.structure(0))
            .map(|s| {
                let sid = s.get::<i32>("stream-id").unwrap_or(0) as u8;
                let dt = s.get::<i32>("data-type").unwrap_or(0) as u8;
                (sid, dt)
            })
            .unwrap_or((0, 0));

        let pad_name = name
            .map(String::from)
            .unwrap_or_else(|| format!("embed_{stream_id}"));

        let pad = gst::Pad::builder_from_template(templ)
            .name(pad_name)
            .chain_function(|pad, parent, buffer| {
                let element = parent.unwrap().downcast_ref::<super::EfpMux>().unwrap();
                element.imp().embed_chain(pad, buffer)
            })
            .event_function(|pad, parent, event| {
                // Accept caps/segment, default for rest.
                use gst::EventView;
                match event.view() {
                    EventView::Caps(e) => {
                        // Update stream-id and data-type from renegotiated caps.
                        if let Some(s) = e.caps().structure(0) {
                            let element = parent.unwrap().downcast_ref::<super::EfpMux>().unwrap();
                            let imp = element.imp();
                            if let Some(eps) = imp.embed_pads.lock().unwrap().get_mut(pad) {
                                eps.stream_id =
                                    s.get::<i32>("stream-id").unwrap_or(eps.stream_id as i32) as u8;
                                eps.data_type =
                                    s.get::<i32>("data-type").unwrap_or(eps.data_type as i32) as u8;
                            }
                        }
                        true
                    }
                    EventView::Segment(_) => true,
                    EventView::Eos(_) => true, // embed EOS doesn't affect the main stream
                    _ => gst::Pad::event_default(pad, parent, event),
                }
            })
            .build();

        self.embed_pads.lock().unwrap().insert(
            pad.clone(),
            EmbedPadState {
                stream_id,
                data_type,
            },
        );

        self.obj().add_pad(&pad).ok()?;
        pad.set_active(true).ok()?;
        Some(pad)
    }

    fn embed_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (stream_id, data_type) = {
            let eps = self.embed_pads.lock().unwrap();
            let state = eps.get(pad).ok_or(gst::FlowError::Error)?;
            (state.stream_id, state.data_type)
        };

        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

        self.pending_embeds
            .lock()
            .unwrap()
            .entry(stream_id)
            .or_default()
            .push(PendingEmbed {
                data: map.as_slice().to_vec(),
                data_type,
            });

        Ok(gst::FlowSuccess::Ok)
    }
}

fn content_type_from_caps(name: &str) -> u8 {
    match name {
        "video/x-h264" => efp::CONTENT_H264,
        "video/x-h265" => efp::CONTENT_H265,
        "audio/x-opus" => efp::CONTENT_OPUS,
        _ => efp::CONTENT_PRIVATE_DATA,
    }
}
