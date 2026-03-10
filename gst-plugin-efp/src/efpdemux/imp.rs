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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bucket_timeout: DEFAULT_BUCKET_TIMEOUT,
            hol_timeout: DEFAULT_HOL_TIMEOUT,
        }
    }
}

struct DemuxState {
    receiver: efp::Receiver,
    pending: Arc<Mutex<Vec<efp::SuperFrame>>>,
    adapter: Vec<u8>,
    /// Read cursor into adapter. Data before this offset has been consumed.
    adapter_offset: usize,
}

// ---------------------------------------------------------------------------
// Element definition
// ---------------------------------------------------------------------------

pub struct EfpDemux {
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<Option<DemuxState>>,
    srcpads: Mutex<HashMap<u8, gst::Pad>>,
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
            .build();

        Self {
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(None),
            srcpads: Mutex::new(HashMap::new()),
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
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "bucket-timeout" => self.settings.lock().unwrap().bucket_timeout = value.get().unwrap(),
            "hol-timeout" => self.settings.lock().unwrap().hol_timeout = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let s = self.settings.lock().unwrap();
        match pspec.name() {
            "bucket-timeout" => s.bucket_timeout.to_value(),
            "hol-timeout" => s.hol_timeout.to_value(),
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

            vec![sink_template, src_template]
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

        let receiver = efp::Receiver::new(
            settings.bucket_timeout,
            settings.hol_timeout,
            efp::ReceiverMode::RunToCompletion,
            move |frame| {
                pending_cb.lock().unwrap().push(frame);
            },
        )
        .map_err(|e| glib::bool_error!("Failed to create EFP receiver: {e}"))?;

        *self.state.lock().unwrap() = Some(DemuxState {
            receiver,
            pending,
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
            // Would exceed MAX_ADAPTER_SIZE — likely a malformed stream.
            state.adapter.clear();
            state.adapter_offset = 0;
            return Err(gst::FlowError::Error);
        }

        state.adapter.extend_from_slice(map.as_slice());

        // Parse length-prefixed fragments: [4-byte BE len][fragment]...
        let buf = &state.adapter;
        let mut pos = state.adapter_offset;
        while buf.len() - pos >= 4 {
            let len = u32::from_be_bytes([
                buf[pos],
                buf[pos + 1],
                buf[pos + 2],
                buf[pos + 3],
            ]) as usize;

            if len == 0 || len > MAX_FRAGMENT_SIZE {
                // Fragment length exceeds MAX_FRAGMENT_SIZE — likely corrupt.
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
        drop(state_guard);

        for frame in frames {
            self.push_frame(frame)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn push_frame(&self, frame: efp::SuperFrame) -> Result<gst::FlowSuccess, gst::FlowError> {
        let srcpad = self.get_or_create_srcpad(frame.stream_id, frame.data_content)?;

        let mut buffer = gst::Buffer::from_mut_slice(frame.data);
        {
            let buf_ref = buffer.get_mut().unwrap();
            if frame.pts != u64::MAX {
                buf_ref.set_pts(gst::ClockTime::from_nseconds(frame.pts));
            }
            if frame.dts != u64::MAX {
                buf_ref.set_dts(gst::ClockTime::from_nseconds(frame.dts));
            }
        }

        srcpad.push(buffer)
    }

    fn get_or_create_srcpad(
        &self,
        stream_id: u8,
        content_type: u8,
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
        let pad_name = format!("src_{stream_id}");
        let templ = self.obj().pad_template("src_%u").unwrap();
        let pad = gst::Pad::builder_from_template(&templ)
            .name(&pad_name)
            .build();

        // Add pad to element first, then activate and push initial events.
        self.obj()
            .add_pad(&pad)
            .map_err(|_| gst::FlowError::Error)?;

        pad.set_active(true).map_err(|_| gst::FlowError::Error)?;

        // Send mandatory initial events before any buffers.
        let sid = format!("{:08x}-{stream_id}", self.obj().as_ptr() as usize);
        pad.push_event(gst::event::StreamStart::new(&sid));

        let caps = caps_for_content_type(content_type);
        pad.push_event(gst::event::Caps::new(&caps));

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        pad.push_event(gst::event::Segment::new(&segment));

        self.srcpads.lock().unwrap().insert(stream_id, pad.clone());
        Ok(pad)
    }

    /// Collect a snapshot of src pads without holding the lock during downstream calls.
    fn srcpad_snapshot(&self) -> Vec<gst::Pad> {
        self.srcpads.lock().unwrap().values().cloned().collect()
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Caps(_) => true,    // mux caps, not forwarded
            EventView::Segment(_) => true, // demux produces its own segments
            EventView::Eos(_) => {
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
}

fn caps_for_content_type(ct: u8) -> gst::Caps {
    match ct {
        efp::CONTENT_H264 => gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
        efp::CONTENT_H265 => gst::Caps::builder("video/x-h265")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
        efp::CONTENT_OPUS => gst::Caps::builder("audio/x-opus").build(),
        _ => gst::Caps::new_any(),
    }
}
