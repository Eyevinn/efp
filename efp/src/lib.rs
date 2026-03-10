//! Safe, idiomatic Rust wrapper around the
//! [Elastic Frame Protocol](https://github.com/agilecontent/efp) C library.
//!
//! This crate wraps the low-level [`efp_sys`] bindings with safe Rust types,
//! closures for callbacks, and proper error handling.
//!
//! # Safety guarantees
//!
//! - FFI callbacks are wrapped in [`std::panic::catch_unwind`] to prevent
//!   panics from unwinding across the FFI boundary.
//! - Null data pointers from C++ are mapped to empty slices.
//! - Empty fragments are rejected before reaching C++ (which reads byte 0
//!   without a size check).
//! - MTU is validated to the range 256..=65535 to prevent C++ truncation.
//! - Embedded data size is validated to prevent `uint16_t` overflow.
//!
//! # Examples
//!
//! ```no_run
//! use efp::{Sender, Receiver, ReceiverMode};
//!
//! let sender = Sender::new(1400, |fragment, stream_id| {
//!     // handle each MTU-sized fragment
//! }).unwrap();
//!
//! let receiver = Receiver::new(5, 5, ReceiverMode::RunToCompletion, |frame| {
//!     println!("got {} bytes on stream {}", frame.data.len(), frame.stream_id);
//! }).unwrap();
//! ```

use std::ffi::c_void;
use std::fmt;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error codes returned by the EFP C library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EfpError {
    CApiFailure,
    Type2FrameOutOfBounds,
    DtsPtsDiffTooLarge,
    ReceiverNotRunning,
    Type1And3SizeError,
    IllegalEmbeddedData,
    MemoryAllocationError,
    ReservedStreamValue,
    ReservedCodeValue,
    ReservedDtsValue,
    ReservedPtsValue,
    BufferOutOfResources,
    BufferOutOfBounds,
    NotDefinedError,
    InternalCalculationError,
    FrameSizeMismatch,
    UnknownFrameType,
    TooLargeEmbeddedData,
    TooLargeFrame,
    DataNotJson,
    NoDataForKey,
    LessDataThanExpected,
    TooHighVersion,
    VersionNotSupported,
    SourceMissing,
    /// Initialization returned a null handle.
    InitFailed,
    /// An error code we don't recognise.
    Unknown(i16),
}

impl fmt::Display for EfpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for EfpError {}

impl EfpError {
    fn from_code(code: i16) -> Option<Self> {
        if code >= 0 {
            return None;
        }
        Some(match code {
            -1 => Self::CApiFailure,
            -2 => Self::Type2FrameOutOfBounds,
            -3 => Self::DtsPtsDiffTooLarge,
            -4 => Self::ReceiverNotRunning,
            -5 => Self::Type1And3SizeError,
            -6 => Self::IllegalEmbeddedData,
            -7 => Self::MemoryAllocationError,
            -8 => Self::ReservedStreamValue,
            -9 => Self::ReservedCodeValue,
            -10 => Self::ReservedDtsValue,
            -11 => Self::ReservedPtsValue,
            -12 => Self::BufferOutOfResources,
            -13 => Self::BufferOutOfBounds,
            -14 => Self::NotDefinedError,
            -15 => Self::InternalCalculationError,
            -16 => Self::FrameSizeMismatch,
            -17 => Self::UnknownFrameType,
            -18 => Self::TooLargeEmbeddedData,
            -19 => Self::TooLargeFrame,
            -20 => Self::DataNotJson,
            -21 => Self::NoDataForKey,
            -22 => Self::LessDataThanExpected,
            -23 => Self::TooHighVersion,
            -24 => Self::VersionNotSupported,
            -25 => Self::SourceMissing,
            other => Self::Unknown(other),
        })
    }
}

pub type Result<T> = std::result::Result<T, EfpError>;

/// Check a C-API return code, mapping negative values to `Err`.
fn check(code: i16) -> Result<()> {
    match EfpError::from_code(code) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Public re-exports / constants
// ---------------------------------------------------------------------------

/// Return the EFP library version as `(major, minor)`.
#[must_use]
pub fn version() -> (u8, u8) {
    let v = unsafe { efp_sys::efp_get_version() };
    ((v >> 8) as u8, (v & 0xff) as u8)
}

/// Build a 4-character content code (matches the C `EFP_CODE` macro).
///
/// Bytes are packed in big-endian (network) order: `c0` is the most
/// significant byte and `c3` the least significant.
#[must_use]
pub const fn code(c0: u8, c1: u8, c2: u8, c3: u8) -> u32 {
    ((c0 as u32) << 24) | ((c1 as u32) << 16) | ((c2 as u32) << 8) | (c3 as u32)
}

// ---------------------------------------------------------------------------
// Well-known content types from the EFP specification
// ---------------------------------------------------------------------------

/// Private / unspecified data.
pub const CONTENT_PRIVATE_DATA: u8 = 0x01;
/// H.264 / AVC video.
pub const CONTENT_H264: u8 = 0x83;
/// H.265 / HEVC video.
pub const CONTENT_H265: u8 = 0x84;
/// Opus audio.
pub const CONTENT_OPUS: u8 = 0x89;

/// Receiver operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverMode {
    /// Receiver runs its own background thread for timeout handling.
    Threaded,
    /// All work is done synchronously inside `receive_fragment`.
    RunToCompletion,
}

impl ReceiverMode {
    fn as_raw(self) -> u32 {
        match self {
            Self::Threaded => efp_sys::EFP_MODE_THREAD,
            Self::RunToCompletion => efp_sys::EFP_MODE_RUN_TO_COMPLETE,
        }
    }
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

type SendCallback = Box<dyn Fn(&[u8], u8) + Send + Sync>;

/// Callback context stored on the heap for the sender.
struct SenderCtx {
    cb: SendCallback,
}

/// A safe wrapper around `ElasticFrameProtocolSender`.
///
/// Fragments superframes into MTU-sized pieces and delivers them through
/// the closure provided at construction time.
pub struct Sender {
    handle: u64,
    /// Prevent the ctx from being dropped while the C object is alive.
    _ctx: Box<SenderCtx>,
}

// The C++ sender uses internal locking; safe to move / share across threads.
unsafe impl Send for Sender {}
unsafe impl Sync for Sender {}

/// C-compatible trampoline that forwards to the Rust closure.
///
/// Wraps the call in `catch_unwind` to prevent Rust panics from unwinding
/// across the FFI boundary (which is undefined behaviour).
unsafe extern "C" fn sender_trampoline(
    data: *const u8,
    size: usize,
    stream_id: u8,
    ctx: *mut c_void,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*(ctx as *const SenderCtx) };
        let slice = if data.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(data, size) }
        };
        (ctx.cb)(slice, stream_id);
    }));
    if result.is_err() {
        std::process::abort();
    }
}

/// Prepend embedded data to a frame payload, returning a new combined buffer.
///
/// This allocates a new `Vec` containing the embedded header, embedded data,
/// and the original frame data. Pass the returned buffer to [`Sender::send`]
/// instead of the raw frame data.
///
/// Returns [`EfpError::TooLargeEmbeddedData`] if `embedded` exceeds 65535
/// bytes (the C library stores the size in a `uint16_t` and truncates).
///
/// * `embedded` – The embedded payload bytes.
/// * `frame_data` – The main frame content.
/// * `data_type` – Embedded content type (see `ElasticFrameEmbeddedContentDefines`).
/// * `is_last` – `true` if this is the last (or only) embedded payload.
pub fn add_embedded_data(
    embedded: &[u8],
    frame_data: &[u8],
    data_type: u8,
    is_last: bool,
) -> Result<Vec<u8>> {
    if embedded.len() > u16::MAX as usize {
        return Err(EfpError::TooLargeEmbeddedData);
    }
    // The C API takes non-const pointers for pESrc/pDSrc even though it only
    // reads from them. We make owned copies to avoid casting &[u8] to *mut u8
    // which would violate Rust aliasing rules.
    let mut embedded_copy = embedded.to_vec();
    let mut frame_copy = frame_data.to_vec();

    // First call with null dst to query the required allocation size.
    let total = unsafe {
        efp_sys::efp_add_embedded_data(
            std::ptr::null_mut(),
            embedded_copy.as_mut_ptr(),
            frame_copy.as_mut_ptr(),
            embedded_copy.len(),
            frame_copy.len(),
            data_type,
            u8::from(is_last),
        )
    };

    let mut dst = vec![0u8; total];

    // Second call fills the destination buffer.
    unsafe {
        efp_sys::efp_add_embedded_data(
            dst.as_mut_ptr(),
            embedded_copy.as_mut_ptr(),
            frame_copy.as_mut_ptr(),
            embedded_copy.len(),
            frame_copy.len(),
            data_type,
            u8::from(is_last),
        );
    }

    Ok(dst)
}

impl Sender {
    /// Create a new sender.
    ///
    /// * `mtu` – Maximum transmission unit (fragment size) in bytes.
    /// * `on_fragment` – Called for every fragment produced.  Receives the
    ///   fragment bytes and the stream ID.
    ///
    /// Minimum MTU required by the EFP protocol (must fit at least the header).
    pub const MIN_MTU: u64 = 256;
    /// Maximum MTU (C++ internally stores as `uint32_t`).
    pub const MAX_MTU: u64 = u16::MAX as u64;

    pub fn new<F>(mtu: u64, on_fragment: F) -> Result<Self>
    where
        F: Fn(&[u8], u8) + Send + Sync + 'static,
    {
        if !(Self::MIN_MTU..=Self::MAX_MTU).contains(&mtu) {
            return Err(EfpError::FrameSizeMismatch);
        }
        let ctx = Box::new(SenderCtx {
            cb: Box::new(on_fragment),
        });
        let ctx_ptr: *const SenderCtx = &*ctx;
        let handle =
            unsafe { efp_sys::efp_init_send(mtu, Some(sender_trampoline), ctx_ptr as *mut c_void) };
        if handle == 0 {
            return Err(EfpError::InitFailed);
        }
        Ok(Self { handle, _ctx: ctx })
    }

    /// Fragment and send a superframe.
    #[allow(clippy::too_many_arguments)]
    pub fn send(
        &self,
        data: &[u8],
        data_content: u8,
        pts: u64,
        dts: u64,
        code: u32,
        stream_id: u8,
        flags: u8,
    ) -> Result<()> {
        let rc = unsafe {
            efp_sys::efp_send_data(
                self.handle,
                data.as_ptr(),
                data.len(),
                data_content,
                pts,
                dts,
                code,
                stream_id,
                flags,
            )
        };
        check(rc)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        unsafe {
            efp_sys::efp_end_send(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// A reassembled superframe delivered by the receiver.
#[derive(Debug, Clone)]
pub struct SuperFrame {
    pub data: Vec<u8>,
    pub data_content: u8,
    pub broken: bool,
    pub pts: u64,
    pub dts: u64,
    pub code: u32,
    pub stream_id: u8,
    pub source: u8,
    pub flags: u8,
}

/// Embedded data extracted from a superframe.
#[derive(Debug, Clone)]
pub struct EmbeddedData {
    pub data: Vec<u8>,
    pub data_type: u8,
    pub pts: u64,
}

type SuperFrameCb = Box<dyn Fn(SuperFrame) + Send + Sync>;
type EmbeddedCb = Box<dyn Fn(EmbeddedData) + Send + Sync>;

struct ReceiverCtx {
    on_frame: SuperFrameCb,
    on_embedded: Option<EmbeddedCb>,
}

/// A safe wrapper around `ElasticFrameProtocolReceiver`.
///
/// Accepts fragments and reassembles them into superframes, delivered
/// through the closure provided at construction time.
pub struct Receiver {
    handle: u64,
    _ctx: Box<ReceiverCtx>,
}

unsafe impl Send for Receiver {}
unsafe impl Sync for Receiver {}

unsafe extern "C" fn receiver_frame_trampoline(
    data: *mut u8,
    size: usize,
    data_content: u8,
    broken: u8,
    pts: u64,
    dts: u64,
    code: u32,
    stream_id: u8,
    source: u8,
    flags: u8,
    ctx: *mut c_void,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*(ctx as *const ReceiverCtx) };
        let slice = if data.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(data, size) }
        };
        let frame = SuperFrame {
            data: slice.to_vec(),
            data_content,
            broken: broken != 0,
            pts,
            dts,
            code,
            stream_id,
            source,
            flags,
        };
        (ctx.on_frame)(frame);
    }));
    if result.is_err() {
        std::process::abort();
    }
}

unsafe extern "C" fn receiver_embedded_trampoline(
    data: *mut u8,
    size: usize,
    data_type: u8,
    pts: u64,
    ctx: *mut c_void,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*(ctx as *const ReceiverCtx) };
        if let Some(ref cb) = ctx.on_embedded {
            let slice = if data.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(data, size) }
            };
            let embedded = EmbeddedData {
                data: slice.to_vec(),
                data_type,
                pts,
            };
            cb(embedded);
        }
    }));
    if result.is_err() {
        std::process::abort();
    }
}

impl Receiver {
    /// Create a new receiver.
    ///
    /// * `bucket_timeout` – Bucket timeout in units of 10 ms.
    /// * `hol_timeout` – Head-of-line timeout in units of 10 ms.
    /// * `mode` – [`ReceiverMode::Threaded`] or [`ReceiverMode::RunToCompletion`].
    /// * `on_frame` – Called for each reassembled superframe.
    pub fn new<F>(
        bucket_timeout: u32,
        hol_timeout: u32,
        mode: ReceiverMode,
        on_frame: F,
    ) -> Result<Self>
    where
        F: Fn(SuperFrame) + Send + Sync + 'static,
    {
        Self::with_embedded(
            bucket_timeout,
            hol_timeout,
            mode,
            on_frame,
            None::<fn(EmbeddedData)>,
        )
    }

    /// Create a new receiver with an optional embedded-data callback.
    pub fn with_embedded<F, G>(
        bucket_timeout: u32,
        hol_timeout: u32,
        mode: ReceiverMode,
        on_frame: F,
        on_embedded: Option<G>,
    ) -> Result<Self>
    where
        F: Fn(SuperFrame) + Send + Sync + 'static,
        G: Fn(EmbeddedData) + Send + Sync + 'static,
    {
        let ctx = Box::new(ReceiverCtx {
            on_frame: Box::new(on_frame),
            on_embedded: on_embedded.map(|f| Box::new(f) as EmbeddedCb),
        });
        let ctx_ptr: *const ReceiverCtx = &*ctx;

        let embedded_cb = if ctx.on_embedded.is_some() {
            Some(
                receiver_embedded_trampoline
                    as unsafe extern "C" fn(*mut u8, usize, u8, u64, *mut c_void),
            )
        } else {
            None
        };

        let handle = unsafe {
            efp_sys::efp_init_receive(
                bucket_timeout,
                hol_timeout,
                Some(receiver_frame_trampoline),
                embedded_cb,
                ctx_ptr as *mut c_void,
                mode.as_raw(),
            )
        };
        if handle == 0 {
            return Err(EfpError::InitFailed);
        }
        Ok(Self { handle, _ctx: ctx })
    }

    /// Feed a fragment into the receiver for reassembly.
    ///
    /// Returns [`EfpError::FrameSizeMismatch`] if `fragment` is empty, since
    /// the C++ implementation reads the first byte without a size check.
    pub fn receive_fragment(&self, fragment: &[u8], from_source: u8) -> Result<()> {
        if fragment.is_empty() {
            return Err(EfpError::FrameSizeMismatch);
        }
        let rc = unsafe {
            efp_sys::efp_receive_fragment(
                self.handle,
                fragment.as_ptr(),
                fragment.len(),
                from_source,
            )
        };
        check(rc)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        unsafe {
            efp_sys::efp_end_receive(self.handle);
        }
    }
}
