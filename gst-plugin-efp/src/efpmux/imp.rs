use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

const DEFAULT_MTU: u32 = 1400;

// ---------------------------------------------------------------------------
// Settings & internal state
// ---------------------------------------------------------------------------

struct Settings {
    mtu: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mtu: DEFAULT_MTU }
    }
}

struct PadState {
    stream_id: u8,
    content_type: u8,
    eos: bool,
}

struct MuxState {
    sender: efp::Sender,
    /// Contiguous buffer of length-prefixed fragments: [4-byte BE len][data]...
    /// Avoids per-fragment heap allocation in the hot path.
    pending: Arc<Mutex<Vec<u8>>>,
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
    stream_ids: Mutex<StreamIdAllocator>,
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
            stream_ids: Mutex::new(StreamIdAllocator::new()),
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
            vec![glib::ParamSpecUInt::builder("mtu")
                .nick("MTU")
                .blurb("Maximum Transmission Unit for EFP fragments (bytes)")
                .minimum(100)
                .maximum(65535)
                .default_value(DEFAULT_MTU)
                .mutable_ready()
                .build()]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "mtu" => self.settings.lock().unwrap().mtu = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "mtu" => self.settings.lock().unwrap().mtu.to_value(),
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

            let sink_caps = gst::Caps::new_any();
            let sink_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &sink_caps,
            )
            .unwrap();

            vec![src_template, sink_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let stream_id = self.stream_ids.lock().unwrap().allocate()?;

        let pad_name = name
            .map(String::from)
            .unwrap_or_else(|| format!("sink_{stream_id}"));

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

        let (stream_id, content_type) = {
            let pads = self.pads.lock().unwrap();
            let ps = match pads.get(pad) {
                Some(ps) => ps,
                None => return Err(gst::FlowError::Error),
            };
            (ps.stream_id, ps.content_type)
        };

        // EFP reserves u64::MAX for PTS/DTS; use 0 when GStreamer has no timestamp.
        let pts = buffer.pts().map_or(0, |t| t.nseconds());
        let dts = buffer.dts().map_or(0, |t| t.nseconds());
        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

        if let Err(_e) = state
            .sender
            .send(map.as_slice(), content_type, pts, dts, 0, stream_id, 0)
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
                    if let Some(ps) = self.pads.lock().unwrap().get_mut(pad) {
                        ps.content_type = ct;
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
}

fn content_type_from_caps(name: &str) -> u8 {
    match name {
        "video/x-h264" => efp::CONTENT_H264,
        "video/x-h265" => efp::CONTENT_H265,
        "audio/x-opus" => efp::CONTENT_OPUS,
        _ => efp::CONTENT_PRIVATE_DATA,
    }
}
