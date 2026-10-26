//! The windowless browser's picture: the render handler and the shared ring.
//!
//! CEF delivers each paint as BGRA pixels for the view's rect; the handler
//! copies the dirty rects into a POSIX shared-memory object named after the
//! host's port, so whoever drives the host can take the same bytes without a
//! socket hop per frame. A monotonic frame counter makes the ring a seqlock:
//! odd while a paint is being written, even when the whole view is readable.

#![allow(unsafe_code, reason = "the ring is a shared-memory mapping written through CEF's C ABI")]
#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::ffi::CString;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{
    Browser, ImplRenderHandler, PaintElementType, Rect, RenderHandler, ScreenInfo,
    WrapRenderHandler,
};

/// The frame budget's sides: 3840x2160 device pixels of room, and a frame
/// past their AREA is dropped rather than copied past the mapping. **A
/// surface's CSS layout is not bounded by them** - the ring cuts the raster
/// to keep it, and only a layout past the budget at 1x stops at the budget.
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2160;

const MAGIC: u32 = 0x4345_4652;
/// **Bumped whenever the header grows or its fields move.** The pixel base is
/// `HEADER_LEN`, so a reader drawing an older host's ring would shift every
/// scanline; `Ring::open` refuses a version or length it does not own and the
/// host is replaced instead.
const VERSION: u32 = 5;

/// What the host is doing, for a client's state line.
pub const STATE_STARTING: u32 = 0;
pub const STATE_LIVE: u32 = 1;
pub const STATE_FAILED: u32 = 2;

#[repr(C)]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    /// **The layout's own length, written by the host.** macOS rounds a
    /// segment's size up to a page, so `st_size` cannot tell two header
    /// layouts apart; this field can, without a manual version bump.
    pub header_len: AtomicU32,
    /// The adopted paint size, in device pixels: what `on_paint` writes and
    /// the draw loop reads. **The view's rect is derived from `request_*`,
    /// never from these** - below a raster of 1 the paint is larger than the
    /// request, and dividing it by the request's scale inflates the page.
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// The display's device pixel ratio, times 1000.
    pub scale_milli: AtomicU32,
    /// The paint size and scale the reader last asked for, and the sequence
    /// number that says it changed.
    pub request_width: AtomicU32,
    pub request_height: AtomicU32,
    pub request_scale_milli: AtomicU32,
    pub request_seq: AtomicU64,
    pub state: AtomicU32,
    pub visible: AtomicU32,
    pub focused: AtomicU32,
    /// Odd while a frame is being written, even when it is readable.
    pub frame: AtomicU64,
    pub paints: AtomicU64,
    /// The driver's cue that a call is about to drive the page: the host
    /// re-asserts the page visible when it changes.
    pub show_seq: AtomicU64,
    /// The cue the host has applied, so the driver can wait (bounded) before
    /// it drives the call - a pump already in flight must not serve the call
    /// against the page the cue is about to show.
    pub show_applied: AtomicU64,
    /// The service's request that the next encoded frame be full quality: a
    /// viewer has attached, and its first draw is the crisp one.
    pub key_seq: AtomicU64,
    /// Whether encoding is paused: **1 with no viewer attached**, so a host
    /// nobody is watching paints without spending CPU on WebP. The service
    /// clears it when the first viewer attaches and sets it when the last
    /// one leaves.
    pub paused: AtomicU32,
    /// The host's own pid, written by the host at creation: what proves to
    /// the server that the endpoint it probed belongs to the child it
    /// spawned rather than to whatever answered the port first.
    pub pid: AtomicU32,
}

pub const HEADER_LEN: usize = std::mem::size_of::<Header>();

/// A ring this build created or can safely cue: the header and its pixels.
pub const RING_LEN: usize = HEADER_LEN + MAX_WIDTH as usize * MAX_HEIGHT as usize * 4;

/// The ring, as the host owns it and a driver maps it.
pub struct Ring {
    pub name: String,
    ptr: *mut u8,
    len: usize,
}

// The ring is mapped shared memory: both processes address the same bytes.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    /// Create (or replace) the segment for `name` and write its header.
    pub fn create(name: &str, width: u32, height: u32) -> Result<Self, String> {
        let width = width.clamp(1, MAX_WIDTH);
        let height = height.clamp(1, MAX_HEIGHT);
        let len = RING_LEN;
        let cname =
            CString::new(name).map_err(|why| format!("the ring name is not a C string: {why}"))?;
        // SAFETY: POSIX shm calls with a valid name and length.
        unsafe {
            let existed = libc::shm_unlink(cname.as_ptr()) == 0;
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            if fd < 0 {
                return Err(format!(
                    "the frame ring {name} cannot be created: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::ftruncate(fd, i64::try_from(len).unwrap_or(i64::MAX)) != 0 {
                let why = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("the frame ring {name} cannot be sized: {why}"));
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            if ptr == libc::MAP_FAILED {
                return Err(format!(
                    "the frame ring {name} cannot be mapped: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if existed {
                // A segment left by a dead host: replaced, not reused.
                eprintln!("cef host: replaced a stale frame ring ({name})");
            }
            let ring = Ring { name: name.to_owned(), ptr: ptr.cast(), len };
            let header = ring.header();
            std::ptr::write(
                header,
                Header {
                    magic: AtomicU32::new(MAGIC),
                    version: AtomicU32::new(VERSION),
                    header_len: AtomicU32::new(u32::try_from(HEADER_LEN).unwrap_or(u32::MAX)),
                    width: AtomicU32::new(width),
                    height: AtomicU32::new(height),
                    scale_milli: AtomicU32::new(1_000),
                    request_width: AtomicU32::new(width),
                    request_height: AtomicU32::new(height),
                    request_scale_milli: AtomicU32::new(1_000),
                    request_seq: AtomicU64::new(0),
                    state: AtomicU32::new(STATE_STARTING),
                    visible: AtomicU32::new(0),
                    focused: AtomicU32::new(0),
                    frame: AtomicU64::new(0),
                    paints: AtomicU64::new(0),
                    show_seq: AtomicU64::new(0),
                    show_applied: AtomicU64::new(0),
                    key_seq: AtomicU64::new(0),
                    // **A host starts paused**: no viewer is attached yet,
                    // and nothing should be encoded until one is.
                    paused: AtomicU32::new(1),
                    pid: AtomicU32::new(0),
                },
            );
            Ok(ring)
        }
    }

    /// Map a ring a host already created.
    pub fn open(name: &str) -> Result<Self, String> {
        let cname =
            CString::new(name).map_err(|why| format!("the ring name is not a C string: {why}"))?;
        // SAFETY: POSIX shm calls with a valid name.
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                return Err(format!(
                    "the frame ring {name} is not there: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, std::ptr::addr_of_mut!(stat)) != 0 {
                libc::close(fd);
                return Err(format!("the frame ring {name} cannot be read"));
            }
            let len = usize::try_from(stat.st_size).unwrap_or(0);
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            if ptr == libc::MAP_FAILED {
                return Err(format!("the frame ring {name} cannot be mapped"));
            }
            let ring = Ring { name: name.to_owned(), ptr: ptr.cast(), len };
            // **A ring this build did not write is refused, not read.** The
            // pixel base is the header's length, so another build's ring would
            // draw shifted rows; refusing it sends the caller down the
            // existing replace-the-host path. The mapping is Drop's to unmap;
            // the name stays, because a foreign host's ring is not ours to
            // unlink - the replacement's own create replaces it.
            if !ring.current() {
                drop(ring);
                return Err(format!("the frame ring {name} was written by another build"));
            }
            Ok(ring)
        }
    }

    /// Whether this mapping is one this build wrote: a long-enough mapping,
    /// the magic, the self-described header length and the version. Checked
    /// before any field is read through [`Self::header`].
    fn current(&self) -> bool {
        // **`>=`, not `==`**: macOS rounds a POSIX segment up to a 16 KiB
        // page, so a live ring's size is the requested one rounded. A ring
        // smaller than this header is refused here; the length field is what
        // separates another build's header when the requested sizes round to
        // the same page.
        if self.len < RING_LEN {
            return false;
        }
        // SAFETY: the length check above proves the mapping holds a header.
        let header = unsafe { &*self.header() };
        header.magic.load(Ordering::Acquire) == MAGIC
            && usize::try_from(header.header_len.load(Ordering::Acquire)).unwrap_or(0) == HEADER_LEN
            && header.version.load(Ordering::Acquire) == VERSION
    }

    /// Remove the segment's name. The bytes stay alive for anyone still
    /// mapped; a later create starts fresh instead of finding a dead one.
    pub fn unlink(name: &str) {
        if let Ok(cname) = CString::new(name) {
            // SAFETY: a valid name.
            unsafe {
                libc::shm_unlink(cname.as_ptr());
            }
        }
    }

    pub fn header(&self) -> *mut Header {
        self.ptr.cast()
    }

    pub fn set_state(&self, state: u32) {
        // SAFETY: the header is mapped for this process's whole life.
        unsafe {
            (*self.header()).state.store(state, Ordering::Release);
        }
    }

    pub fn state(&self) -> u32 {
        // SAFETY: `open` refuses any other layout.
        unsafe { (*self.header()).state.load(Ordering::Acquire) }
    }

    pub fn set_visible(&self, visible: bool, focused: bool) {
        // SAFETY: as above.
        unsafe {
            let header = &*self.header();
            header.visible.store(u32::from(visible), Ordering::Release);
            header.focused.store(u32::from(focused), Ordering::Release);
        }
    }

    /// Ask the host for another paint size (device pixels) and scale. The
    /// host adopts it on its next pump and tells CEF.
    ///
    /// **The surface's layout is what the caller names, and the frame budget
    /// only limits the raster** (`budgeted_request`).
    pub fn request(&self, width: u32, height: u32, scale: f64) {
        let (width, height, scale_milli) = budgeted_request(width, height, scale_milli(scale));
        // SAFETY: the header is mapped; the sequence is published last so the
        // host never reads a half-written request.
        unsafe {
            let header = &*self.header();
            header.request_width.store(width, Ordering::Relaxed);
            header.request_height.store(height, Ordering::Relaxed);
            header.request_scale_milli.store(scale_milli, Ordering::Relaxed);
            header.request_seq.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Cue the host to re-assert the page visible before a call drives it,
    /// answering this cue's sequence for [`Self::shown`].
    ///
    /// Only a ring that passed `create` or [`Self::open`]'s checks reaches
    /// here, so the header is always this build's layout.
    pub fn show(&self) -> u64 {
        // SAFETY: as above; `open` refuses any other layout.
        unsafe { (*self.header()).show_seq.fetch_add(1, Ordering::AcqRel) + 1 }
    }

    /// Whether the host has applied `seq` - the wait before a call drives
    /// the page, so a pump already in flight cannot serve it hidden.
    pub fn shown(&self, seq: u64) -> bool {
        // SAFETY: as above.
        unsafe { (*self.header()).show_applied.load(Ordering::Acquire) >= seq }
    }

    /// Record the host's pid: the server reads it to know whose endpoint it
    /// probed.
    pub fn set_pid(&self, pid: u32) {
        // SAFETY: the header is mapped; a single store needs no sequence.
        unsafe {
            (*self.header()).pid.store(pid, Ordering::Release);
        }
    }

    /// The pid the host wrote into this ring.
    pub fn pid(&self) -> u32 {
        // SAFETY: `open` refuses any other layout.
        unsafe { (*self.header()).pid.load(Ordering::Acquire) }
    }

    /// Pause or resume frame encoding. **A host with no viewer is paused**,
    /// so the encode cost is paid only while somebody is watching.
    pub fn pause(&self, paused: bool) {
        // SAFETY: the header is mapped; a single store needs no sequence.
        unsafe {
            (*self.header()).paused.store(u32::from(paused), Ordering::Release);
        }
    }

    /// Ask the host for a full-quality frame on its next paint: a viewer has
    /// attached, and its first draw should be the crisp one.
    pub fn key(&self) -> u64 {
        // SAFETY: as above; `open` refuses any other layout.
        unsafe { (*self.header()).key_seq.fetch_add(1, Ordering::AcqRel) + 1 }
    }

    /// The whole view's pixels, one byte per channel.
    ///
    /// # Safety
    ///
    /// The caller only reads within `width * height * 4` bytes of the live
    /// view.
    pub unsafe fn pixels(&self) -> *const u8 {
        // SAFETY: the pixels follow the header inside the same mapping.
        unsafe { self.ptr.add(HEADER_LEN) }
    }

    /// Copy one paint into the ring. `width`/`height` are the live view size
    /// the buffer was painted at; a buffer of another size is dropped rather
    /// than written into the wrong rectangle.
    ///
    /// # Safety
    ///
    /// `buffer` is a BGRA frame of `width * height` pixels.
    pub unsafe fn write_frame(
        &self,
        rects: Option<&[Rect]>,
        buffer: *const u8,
        width: u32,
        height: u32,
    ) {
        let header = self.header();
        // SAFETY: the header is mapped and owned by this process for writing.
        let frame = unsafe { &*header };
        if width != frame.width.load(Ordering::Relaxed)
            || height != frame.height.load(Ordering::Relaxed)
        {
            return;
        }
        let row_width = usize::try_from(width).unwrap_or(0);
        // SAFETY: the ring holds MAX_WIDTH*MAX_HEIGHT pixels after the
        // header; every rect is clipped to the live view.
        unsafe {
            let pixels = self.ptr.add(HEADER_LEN);
            frame.frame.fetch_add(1, Ordering::AcqRel);
            match rects {
                Some(rects) if !rects.is_empty() => {
                    for rect in rects {
                        let x = usize::try_from(rect.x.max(0)).unwrap_or(0);
                        let y = usize::try_from(rect.y.max(0)).unwrap_or(0);
                        let w = usize::try_from(rect.width.max(0)).unwrap_or(0);
                        let h = usize::try_from(rect.height.max(0)).unwrap_or(0);
                        let x1 = (x + w).min(row_width);
                        let y1 = (y + h).min(usize::try_from(height).unwrap_or(0));
                        if x1 <= x {
                            continue;
                        }
                        for row in y..y1 {
                            let offset = (row * row_width + x) * 4;
                            std::ptr::copy_nonoverlapping(
                                buffer.add(offset),
                                pixels.add(offset),
                                (x1 - x) * 4,
                            );
                        }
                    }
                }
                _ => {
                    std::ptr::copy_nonoverlapping(
                        buffer,
                        pixels,
                        row_width * usize::try_from(height).unwrap_or(0) * 4,
                    );
                }
            }
            frame.frame.fetch_add(1, Ordering::AcqRel);
            frame.paints.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the map this object created.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// One painted view, as the host hands it on: the BGRA buffer, its size, and
/// whether the frame is a state change the stream must not coalesce.
///
/// **A popup's frame is not a repaint of the view.** The host's frame stream
/// drops paints that arrive inside one frame interval, which is right for a
/// page animating faster than the encoder: the next paint subsumes the last.
/// A popup paint is the ONE frame that carries the menu - CEF then paints the
/// view without it - so a coalesced popup is a menu nobody ever sees.
pub type PaintSink = Arc<dyn Fn(&[u8], u32, u32, bool) + Send + Sync>;

/// The last view paint, kept so a popup can be drawn over it.
#[derive(Default)]
struct ViewPaint {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

/// One popup paint and where it sits in the view, in device pixels.
struct PopupPaint {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    x: i64,
    y: i64,
}

/// What the handler composes a frame from.
#[derive(Default)]
struct PaintState {
    view: ViewPaint,
    /// The popup's bounds from `on_popup_size`, in the page's own DIP.
    bounds: Option<Rect>,
    popup: Option<PopupPaint>,
    /// The view with the popup drawn over it; reused, since a frame is
    /// megabytes and a fresh buffer per popup paint would churn.
    composed: Vec<u8>,
}

impl PaintState {
    /// Take one view paint.
    fn take_view(&mut self, pixels: &[u8], width: u32, height: u32) {
        self.view.pixels.clear();
        self.view.pixels.extend_from_slice(pixels);
        self.view.width = width;
        self.view.height = height;
    }

    /// Take one popup paint, placed by the bounds CEF named for it.
    fn take_popup(&mut self, pixels: &[u8], width: u32, height: u32) {
        // **The bounds are DIP; the paint is device pixels.** The paint's own
        // width over the bounds' is the scale CEF drew at, so the placement
        // follows the buffer rather than a scale read at another time.
        let scale = self
            .bounds
            .as_ref()
            .filter(|bounds| bounds.width > 0)
            .map_or(1.0, |bounds| f64::from(width) / f64::from(bounds.width));
        let (x, y) = self.bounds.as_ref().map_or((0, 0), |bounds| {
            (
                i64::from(rounded_i32(f64::from(bounds.x) * scale, i32::MAX)),
                i64::from(rounded_i32(f64::from(bounds.y) * scale, i32::MAX)),
            )
        });
        self.popup = Some(PopupPaint { pixels: pixels.to_vec(), width, height, x, y });
    }

    /// The frame to hand on: the last view, with the open popup over it.
    fn frame(&mut self) -> Option<(&[u8], u32, u32)> {
        let (width, height) = (self.view.width, self.view.height);
        if width == 0 || height == 0 || self.view.pixels.is_empty() {
            return None;
        }
        let Some(popup) = &self.popup else {
            return Some((&self.view.pixels, width, height));
        };
        self.composed.clear();
        self.composed.extend_from_slice(&self.view.pixels);
        blit_popup(&mut self.composed, width, height, popup);
        Some((&self.composed, width, height))
    }
}

/// Draw one popup buffer over a view buffer, clipping to the view.
///
/// **The popup is a separate paint in windowless rendering.** CEF hands it
/// as its own buffer over the view, and a host that drops it leaves a select
/// open with nothing drawn - the clicks keep going to the invisible popup
/// and the page reads as frozen.
fn blit_popup(view: &mut [u8], width: u32, height: u32, popup: &PopupPaint) {
    let (view_width, view_height) = (i64::from(width), i64::from(height));
    let (popup_width, popup_height) = (i64::from(popup.width), i64::from(popup.height));
    if popup_width <= 0 || popup_height <= 0 {
        return;
    }
    let left = popup.x.max(0);
    let top = popup.y.max(0);
    let right = (popup.x + popup_width).min(view_width);
    let bottom = (popup.y + popup_height).min(view_height);
    if right <= left || bottom <= top {
        return;
    }
    let columns = usize::try_from(right - left).unwrap_or(0);
    for row in top..bottom {
        let source_row = usize::try_from(row - popup.y).unwrap_or(0);
        let source_x = usize::try_from(left - popup.x).unwrap_or(0);
        let source_start = (source_row * usize::try_from(popup_width).unwrap_or(0) + source_x) * 4;
        let target_start = (usize::try_from(row).unwrap_or(0)
            * usize::try_from(width).unwrap_or(0)
            + usize::try_from(left).unwrap_or(0))
            * 4;
        let span = columns * 4;
        let (Some(source_end), Some(target_end)) =
            (source_start.checked_add(span), target_start.checked_add(span))
        else {
            return;
        };
        let (Some(source), Some(target)) =
            (popup.pixels.get(source_start..source_end), view.get_mut(target_start..target_end))
        else {
            return;
        };
        target.copy_from_slice(source);
    }
}

/// Hand the composed frame to the ring and to the server's stream.
///
/// `distinct` marks a frame the stream must not coalesce: a popup appearing,
/// moving or closing.
fn emit(
    ring: &Ring,
    sink: Option<&PaintSink>,
    state: &mut PaintState,
    dirty: Option<&[Rect]>,
    distinct: bool,
    expected: Option<(u32, u32)>,
) {
    let Some((pixels, width, height)) = state.frame() else { return };
    // The composed buffer is a BGRA view of exactly width*height.
    paint(ring, dirty, pixels.as_ptr(), width, height, expected);
    // **The frame stream and the ring see the same frame.** A size the ring
    // would refuse is still a frame with its own dimensions on the wire.
    if let Some(sink) = sink {
        sink(pixels, width, height, distinct);
    }
}

/// One tab's view size, in the ring's own units: device pixels and the
/// milli-scale. **A tab may be laid out apart from the profile's request**
/// while a take records it; every other tab reads what the controller asked
/// for.
#[derive(Debug)]
pub struct ViewSize {
    width: std::sync::atomic::AtomicU32,
    height: std::sync::atomic::AtomicU32,
    scale_milli: std::sync::atomic::AtomicU32,
}

impl ViewSize {
    /// The size a tab is born at: the profile's current request.
    pub fn of_ring(ring: &Ring) -> Arc<Self> {
        // SAFETY: the header is mapped for this process's whole life.
        let header = unsafe { &*ring.header() };
        Arc::new(Self {
            width: std::sync::atomic::AtomicU32::new(header.request_width.load(Ordering::Relaxed)),
            height: std::sync::atomic::AtomicU32::new(
                header.request_height.load(Ordering::Relaxed),
            ),
            scale_milli: std::sync::atomic::AtomicU32::new(
                header.request_scale_milli.load(Ordering::Relaxed),
            ),
        })
    }

    /// **Every path that names a tab's size lands here** - the profile's
    /// resize and a take's own - so the frame budget is spent here once, and
    /// a raw surface request cannot outgrow the ring on either of them.
    pub fn set(&self, width: u32, height: u32, scale: f64) {
        let (width, height, scale_milli) = budgeted_request(width, height, scale_milli(scale));
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
        self.scale_milli.store(scale_milli, Ordering::Relaxed);
    }

    fn get(&self) -> (u32, u32, u32) {
        (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
            self.scale_milli.load(Ordering::Relaxed),
        )
    }
}

/// The paint size a tab's own layout expects, when it has one.
fn expected_of(size: Option<&Arc<ViewSize>>) -> Option<(u32, u32)> {
    size.map(|size| {
        let (width, height, scale_milli) = size.get();
        painted_size(width, height, scale_milli)
    })
}

cef::wrap_render_handler! {
    struct OsrRenderHandler {
        ring: Arc<Ring>,
        paint: Option<PaintSink>,
        state: Arc<Mutex<PaintState>>,
        // This tab's own size; `None` reads the ring's request, which is what
        // a handler built without a tab does.
        size: Option<Arc<ViewSize>>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            rect.x = 0;
            rect.y = 0;
            // **The view rect is DIP; the paint buffer is device pixels.**
            // **It is derived from the REQUEST, never the adopted paint size**:
            // below a raster of 1 the paint is larger than the request, and
            // dividing that by the request's scale would inflate the page and
            // stale every later frame. A tab's own size wins while it is
            // recorded; every other tab reads the ring's request.
            let (request_w, request_h, scale_milli) = if let Some(size) = &self.size {
                size.get()
            } else {
                // SAFETY: the header is mapped for this process's whole life.
                let header = unsafe { &*self.ring.header() };
                (
                    header.request_width.load(Ordering::Relaxed),
                    header.request_height.load(Ordering::Relaxed),
                    header.request_scale_milli.load(Ordering::Relaxed),
                )
            };
            let (width, height) = view_size(request_w, request_h, scale_milli);
            rect.width = width;
            rect.height = height;
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            info: Option<&mut ScreenInfo>,
        ) -> i32 {
            let Some(info) = info else { return 0 };
            let (request_w, request_h, scale_milli) = if let Some(size) = &self.size {
                size.get()
            } else {
                // SAFETY: the header is mapped for this process's whole life.
                let header = unsafe { &*self.ring.header() };
                (
                    header.request_width.load(Ordering::Relaxed),
                    header.request_height.load(Ordering::Relaxed),
                    header.request_scale_milli.load(Ordering::Relaxed),
                )
            };
            info.device_scale_factor = screen_scale(scale_milli);
            info.depth = 24;
            info.depth_per_component = 8;
            info.is_monochrome = 0;
            let (width, height) = view_size(request_w, request_h, scale_milli);
            info.rect = Rect { x: 0, y: 0, width, height };
            info.available_rect = info.rect.clone();
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {
            if show != 0 {
                return;
            }
            // **A closed popup leaves the frame at once.** CEF repaints the
            // view underneath, but until it does the viewer would keep the
            // last composited frame with a menu nobody can click.
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.popup = None;
            state.bounds = None;
            emit(&self.ring, self.paint.as_ref(), &mut state, None, true, expected_of(self.size.as_ref()));
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.bounds = Some(rect.clone());
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            let width = u32::try_from(width.max(0)).unwrap_or(0);
            let height = u32::try_from(height.max(0)).unwrap_or(0);
            if width == 0 || height == 0 || buffer.is_null() {
                return;
            }
            let Some(len) = usize::try_from(width)
                .ok()
                .zip(usize::try_from(height).ok())
                .and_then(|(width, height)| width.checked_mul(height))
                .and_then(|pixels| pixels.checked_mul(4))
            else {
                return;
            };
            // SAFETY: CEF hands a BGRA buffer of exactly width*height.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match type_ {
                PaintElementType::VIEW => {
                    state.take_view(pixels, width, height);
                    emit(
                        &self.ring,
                        self.paint.as_ref(),
                        &mut state,
                        dirty_rects,
                        false,
                        expected_of(self.size.as_ref()),
                    );
                }
                PaintElementType::POPUP => {
                    state.take_popup(pixels, width, height);
                    // A popup paint is a full composite - the view's dirty
                    // rects are not the popup's - and the one frame that
                    // carries the menu, so the stream must not coalesce it.
                    emit(&self.ring, self.paint.as_ref(), &mut state, None, true, expected_of(self.size.as_ref()));
                }
                _ => {}
            }
        }
    }
}

/// The last size each drop reason logged, so a frozen surface writes one line
/// rather than one per frame.
static LAST_STALE_PAINT: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_OVER_BUDGET_PAINT: AtomicU64 = AtomicU64::new(u64::MAX);

/// Whether this drop is the first of its size since the last one logged.
fn note_drop(last: &AtomicU64, width: u32, height: u32) -> bool {
    let size = (u64::from(width) << 32) | u64::from(height);
    last.swap(size, Ordering::Relaxed) != size
}

/// Whether a frame's pixels fit the ring's mapping. The bound is in pixels,
/// not in either axis: a tall portrait frame within the budget is drawn, and
/// one past it is dropped rather than copied past the mapping.
fn within_budget(width: u32, height: u32) -> bool {
    u64::from(width) * u64::from(height) <= u64::from(MAX_WIDTH) * u64::from(MAX_HEIGHT)
}

/// Copy one delivered frame into the ring, unless it is a frame from before
/// the last resize or one the ring cannot hold.
fn paint(
    ring: &Ring,
    dirty_rects: Option<&[Rect]>,
    buffer: *const u8,
    width: u32,
    height: u32,
    expected: Option<(u32, u32)>,
) {
    // **A tab laid out apart from the profile's request expects its own
    // size**: a take records at 1280x720 while its viewer surfaces ask for
    // another size, and the profile-wide request would drop every take frame
    // as stale.
    let expected = expected.unwrap_or_else(|| {
        // SAFETY: the header is mapped for this process's whole life.
        let header = unsafe { &*ring.header() };
        painted_size(
            header.request_width.load(Ordering::Relaxed),
            header.request_height.load(Ordering::Relaxed),
            header.request_scale_milli.load(Ordering::Relaxed),
        )
    });
    // **A paint at a size the driver no longer asks for is a frame from
    // before the resize, and it is dropped.** Adopting its size undid the
    // newer request and left the page laid out at the old size under a fresh
    // layer rect - the full page squashed into a band once the layer had
    // shrunk (#2470). The next paint carries the requested size.
    if !paint_matches_request((width, height), expected) {
        if note_drop(&LAST_STALE_PAINT, width, height) {
            eprintln!(
                "cef host: dropped a paint for a stale size (event_name \
                 cef_surface_paint_stale_dropped, delivered {width}x{height}, expected {}x{})",
                expected.0, expected.1,
            );
        }
        return;
    }
    // **The ring holds the pixels only while the frame fits its budget.** A
    // view past the cap is not a frame this surface can draw; copying it would
    // run past the mapping.
    if !within_budget(width, height) {
        if note_drop(&LAST_OVER_BUDGET_PAINT, width, height) {
            eprintln!(
                "cef host: dropped a paint past the ring's budget (event_name \
                 cef_surface_paint_over_budget, delivered {width}x{height})"
            );
        }
        return;
    }
    // **The delivered size is the view's size.** The host adopts what CEF
    // actually painted, so the ring always describes its frame.
    // SAFETY: the header is mapped for this process's whole life.
    let header = unsafe { &*ring.header() };
    header.width.store(width, Ordering::Relaxed);
    header.height.store(height, Ordering::Relaxed);
    // SAFETY: CEF hands a BGRA frame of exactly this size, and the budget
    // check above keeps the copy inside the mapping.
    unsafe {
        ring.write_frame(dirty_rects, buffer, width, height);
    }
}

/// The render handler: the view's size and the paint destination.
pub fn handler(ring: Arc<Ring>, paint: Option<PaintSink>) -> RenderHandler {
    OsrRenderHandler::new(ring, paint, Arc::new(Mutex::new(PaintState::default())), None)
}

/// One tab's render handler, with the size it lays out at.
pub fn handler_for_tab(
    ring: Arc<Ring>,
    paint: Option<PaintSink>,
    size: Arc<ViewSize>,
) -> RenderHandler {
    OsrRenderHandler::new(ring, paint, Arc::new(Mutex::new(PaintState::default())), Some(size))
}

/// A scale as the ring stores it, for a per-tab resize line.
pub(crate) fn scale_milli(scale: f64) -> u32 {
    rounded_u32((scale * 1000.0).clamp(100.0, 8_000.0), 1_000)
}

/// The request a surface's device pixels become, with the ring's frame budget
/// held.
///
/// **The budget caps the pixels painted, never the CSS size the page lays out
/// at**: cutting a 2x surface's request halved the page's layout and every
/// click landed right of the pixel under the pointer (measured on a live host
/// at 2048x1112). The raster gives instead; only a layout past the budget at
/// 1x is itself cut, since no frame can hold it.
fn budgeted_request(
    request_width: u32,
    request_height: u32,
    request_milli: u32,
) -> (u32, u32, u32) {
    let scale = (f64::from(request_milli) / 1000.0).clamp(0.1, 8.0);
    let budget = f64::from(MAX_WIDTH) * f64::from(MAX_HEIGHT);
    let mut layout_width = rounded_u32(f64::from(request_width) / scale, 1).max(1);
    let mut layout_height = rounded_u32(f64::from(request_height) / scale, 1).max(1);
    // The margin absorbs the paint's own rounding up past an exact budget.
    let area = f64::from(layout_width) * f64::from(layout_height);
    if area > budget {
        let shrink = (budget / area).sqrt() * 0.998;
        layout_width = rounded_u32(f64::from(layout_width) * shrink, 1).max(1);
        layout_height = rounded_u32(f64::from(layout_height) * shrink, 1).max(1);
    }
    // The largest raster whose painted frame the ring holds, never past the
    // caller's own scale.
    let area = f64::from(layout_width) * f64::from(layout_height);
    let fit = (budget / area).sqrt();
    let mut raster_milli = scale_milli(scale.min(fit));
    loop {
        let painted_width =
            rounded_u32(f64::from(layout_width) * f64::from(raster_milli) / 1000.0, 1).max(1);
        let painted_height =
            rounded_u32(f64::from(layout_height) * f64::from(raster_milli) / 1000.0, 1).max(1);
        if raster_milli <= 100
            || f64::from(painted_width) * f64::from(painted_height) <= budget * 0.999
        {
            break;
        }
        raster_milli -= 1;
    }
    (
        rounded_u32(f64::from(layout_width) * f64::from(raster_milli) / 1000.0, 1).max(1),
        rounded_u32(f64::from(layout_height) * f64::from(raster_milli) / 1000.0, 1).max(1),
        raster_milli,
    )
}

/// Whether a paint is for the view size the driver last asked for.
///
/// **The slack is the DIP round trip's own error**, not a tolerance for a
/// different view: `view_rect` divides device pixels by the scale and
/// `screen_info` multiplies back, so a fractional scale can land a pixel or two
/// away from the request. A paint further off than that was handed to CEF
/// before the resize reached it.
fn paint_matches_request(delivered: (u32, u32), expected: (u32, u32)) -> bool {
    const SLACK: u32 = 2;
    delivered.0.abs_diff(expected.0) <= SLACK && delivered.1.abs_diff(expected.1) <= SLACK
}

/// The DIP view CEF lays the page out at: the request divided by its raster.
/// **Derived from the request, never from an adopted paint size** - below a
/// raster of 1 the paint is larger than the request, and treating that as the
/// view would inflate the page and stale every later frame.
fn view_size(request_width: u32, request_height: u32, scale_milli: u32) -> (i32, i32) {
    let scale = (f64::from(scale_milli) / 1000.0).clamp(0.1, 8.0);
    (
        rounded_i32(f64::from(request_width) / scale, i32::MAX),
        rounded_i32(f64::from(request_height) / scale, i32::MAX),
    )
}

/// A rounded device-pixel count from a computed size.
///
/// **The cast is the only arithmetic route from a float to a count**, and
/// every caller derives its value from the ring's own clamped request sizes,
/// so the result is bounded by `MAX_WIDTH * 8`; a non-finite or negative
/// value takes `fallback` rather than wrapping.
fn rounded_u32(value: f64, fallback: u32) -> u32 {
    if !value.is_finite() || value < 0.0 {
        return fallback;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded by the ring's request caps and checked non-negative"
    )]
    let count = value.round() as u32;
    count
}

/// A rounded DIP size from a computed view, with the same bound as
/// [`rounded_u32`].
fn rounded_i32(value: f64, fallback: i32) -> i32 {
    if !value.is_finite() || value > f64::from(i32::MAX) {
        return fallback;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded by the ring's request caps and checked in range"
    )]
    let points = value.round() as i32;
    points
}

/// The device scale CEF paints the view at: the raster, never below 1.
fn screen_scale(scale_milli: u32) -> f32 {
    let milli = u16::try_from(scale_milli.clamp(100, 8_000)).unwrap_or(1_000);
    (f32::from(milli) / 1000.0).clamp(0.1, 8.0).max(1.0)
}

/// The buffer size CEF paints for a requested view: the DIP view times the
/// screen scale. **A raster below 1 therefore paints LARGER than requested**,
/// and comparing the delivered frame to the raw request would drop every
/// frame (a 1440x2520 takeover at 1x requests 1234x2160 and is painted at
/// 1440x2520).
fn painted_size(request_width: u32, request_height: u32, scale_milli: u32) -> (u32, u32) {
    let (view_width, view_height) = view_size(request_width, request_height, scale_milli);
    let factor = f64::from(screen_scale(scale_milli));
    (
        rounded_u32(f64::from(view_width) * factor, u32::MAX),
        rounded_u32(f64::from(view_height) * factor, u32::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The frame budget never moves the page's layout.** A surface's device
    /// pixels stop at the budget; the CSS size the page lays out at does not.
    /// A desktop takeover of 2048x1112 at 2x asks for 4096x2224, past the
    /// ring's 3840x2160, and every gesture the surface sent landed right of
    /// the pixel under the pointer while the layout was clamped to 1920x1080.
    #[test]
    fn a_surface_past_the_frame_budget_keeps_its_layout_size() {
        let name = "/forge-cef-test-budget";
        let ring = Ring::create(name, 4, 4).expect("a test ring");
        let request = |width, height, scale| {
            ring.request(width, height, scale);
            // SAFETY: the header is mapped for this process's whole life.
            let header = unsafe { &*ring.header() };
            let (width, height, scale) = (
                header.request_width.load(Ordering::Relaxed),
                header.request_height.load(Ordering::Relaxed),
                header.request_scale_milli.load(Ordering::Relaxed),
            );
            (view_size(width, height, scale), painted_size(width, height, scale))
        };

        // The desktop takeover: 2048x1112 CSS at 2x.
        let (view, painted) = request(4096, 2224, 2.0);
        assert_eq!(view, (2048, 1112), "the page keeps the surface's CSS size");
        assert!(
            f64::from(painted.0) * f64::from(painted.1)
                <= f64::from(MAX_WIDTH) * f64::from(MAX_HEIGHT),
            "the frame stays inside the ring's budget, {painted:?}"
        );
        assert!(
            (f64::from(painted.0) / f64::from(painted.1) - 2048.0 / 1112.0).abs() < 0.01,
            "and keeps the surface's shape, {painted:?}"
        );

        // A portrait phone: 390x780 CSS at 3x.
        let (view, painted) = request(1170, 2340, 3.0);
        assert_eq!(view, (390, 780), "the phone's page keeps its own size");
        assert_eq!(painted, (1170, 2340), "inside the area budget, the raster is untouched");

        // A surface inside the budget is untouched.
        let (view, painted) = request(1920, 1080, 2.0);
        assert_eq!((view, painted), ((960, 540), (1920, 1080)), "no budget, no change");

        // Past the budget in CSS pixels even at 1x there is no frame to hold
        // the layout, so the layout is what stops.
        let (view, painted) = request(5120, 2880, 1.0);
        assert!(
            f64::from(painted.0) * f64::from(painted.1)
                <= f64::from(MAX_WIDTH) * f64::from(MAX_HEIGHT),
            "the ceiling holds the frame, {painted:?}"
        );
        assert!(
            (f64::from(view.0) / f64::from(view.1) - 5120.0 / 2880.0).abs() < 0.01,
            "and keeps the surface's shape, {view:?}"
        );

        // **A take's own resize names a tab's size too**, on the same raw
        // surface request: restoring the controller's view must not outgrow
        // the ring either.
        let size = ViewSize::of_ring(&ring);
        size.set(4096, 2224, 2.0);
        let (width, height, scale) = size.get();
        assert_eq!(view_size(width, height, scale), (2048, 1112), "the take's restore keeps it");
        let painted = painted_size(width, height, scale);
        assert!(
            f64::from(painted.0) * f64::from(painted.1)
                <= f64::from(MAX_WIDTH) * f64::from(MAX_HEIGHT),
            "and stays paintable, {painted:?}"
        );
        Ring::unlink(name);
    }

    /// **The frame is the bytes and the counter is a seqlock.** A reader
    /// takes an even counter and the pixels behind it; a frame of another
    /// size must not be written into a rectangle the header does not name,
    /// which is the dropped-paint defect the adoption in `on_paint` fixes.
    #[test]
    fn a_frame_is_written_whole_and_never_at_another_size() {
        let name = "/forge-cef-test-frame";
        let ring = Ring::create(name, 4, 4).expect("a test ring");
        let pixels: Vec<u8> =
            (0..4 * 4 * 4).map(|at| u8::try_from(at).unwrap_or_default()).collect();
        // SAFETY: the buffer is a 4x4 BGRA frame.
        unsafe { ring.write_frame(None, pixels.as_ptr(), 4, 4) };
        let header = unsafe { &*ring.header() };
        assert_eq!(header.frame.load(Ordering::Acquire), 2, "one write opens and closes the lock");
        assert_eq!(header.paints.load(Ordering::Relaxed), 1, "one paint is counted");
        // SAFETY: the ring holds the 4x4 pixels the header names.
        let written = unsafe { std::slice::from_raw_parts(ring.pixels(), pixels.len()) };
        assert_eq!(written, &pixels[..], "the frame's bytes are the ring's");

        // SAFETY: as above; this frame is the wrong size for the header.
        unsafe { ring.write_frame(None, pixels.as_ptr(), 3, 4) };
        assert_eq!(header.paints.load(Ordering::Relaxed), 1, "a mismatched frame is not written");
        Ring::unlink(name);
    }

    /// **The handlers themselves read the request.** Setting the adopted
    /// paint size, as `on_paint` does, must not move the view: below a raster
    /// of 1 the paint is larger than the request, and dividing it by the
    /// request's scale would inflate the page and stale every later frame.
    #[test]
    fn the_view_derives_from_the_request_not_the_adopted_paint() {
        let name = "/forge-cef-test-view";
        let ring = Arc::new(Ring::create(name, 4, 4).expect("a test ring"));
        // A 1440x2520 view at 1x: request 1234x2160, raster 0.857.
        ring.request(1234, 2160, 0.857);
        // The host adopted the painted frame, as `on_paint` would.
        // SAFETY: the ring is mapped for the test's use of it.
        unsafe {
            let header = &*ring.header();
            header.width.store(1440, Ordering::Relaxed);
            header.height.store(2520, Ordering::Relaxed);
            header.scale_milli.store(857, Ordering::Relaxed);
        }
        // The handler's own impl, not the CEF wrapper: `new` returns a
        // `RenderHandler` whose methods forward through a CEF object this test
        // never wraps, so the body would never run.
        let handler = OsrRenderHandler {
            ring: Arc::clone(&ring),
            paint: None,
            state: Arc::new(Mutex::new(PaintState::default())),
            size: None,
            cef_object: std::ptr::null_mut(),
        };
        let mut rect = Rect { x: 0, y: 0, width: 0, height: 0 };
        handler.view_rect(None, Some(&mut rect));
        assert_eq!(
            (rect.width, rect.height),
            (1440, 2520),
            "the view is the request's CSS size, not the adopted paint's",
        );
        let mut info = ScreenInfo::default();
        handler.screen_info(None, Some(&mut info));
        assert_eq!(info.device_scale_factor, 1.0, "the display paints at 1x");
        assert_eq!(
            (info.rect.width, info.rect.height),
            (1440, 2520),
            "and the screen is the same view",
        );
        Ring::unlink(name);
    }

    /// **`paint` compares against the painted size, not the raw request.** A
    /// raster below 1 delivers a frame larger than the request; a raw-request
    /// comparison would drop it and freeze the surface.
    #[test]
    fn a_raster_below_one_paint_is_accepted() {
        let name = "/forge-cef-test-paint-raster";
        let ring = Ring::create(name, 8, 8).expect("a test ring");
        // Request 2x4 at 0.5x: the view is 4x8 and CEF paints it at 1x.
        ring.request(2, 4, 0.5);
        let buffer = [0_u8; 4 * 8 * 4];
        paint(&ring, None, buffer.as_ptr(), 4, 8, None);
        // SAFETY: the ring is mapped for the test's use of it.
        let header = unsafe { &*ring.header() };
        assert_eq!(
            header.paints.load(Ordering::Relaxed),
            1,
            "the painted frame is the request's view, not its raw size",
        );
        Ring::unlink(name);
    }

    /// **A frozen surface writes one line, not one per frame.** The drop log
    /// is keyed on the size, so a repeat of the same stale frame is silent.
    #[test]
    fn a_repeated_drop_logs_once() {
        let last = AtomicU64::new(u64::MAX);
        assert!(note_drop(&last, 100, 50), "the first drop of a size is logged");
        assert!(!note_drop(&last, 100, 50), "the same size again is not");
        assert!(note_drop(&last, 100, 51), "a new size is");
    }

    /// **A frame painted for an older view size is not this view's size.** The
    /// request is device pixels, and a fractional scale can land the DIP round
    /// trip a pixel off; that is the slack. A frame further off was painted
    /// before the resize reached CEF, and adopting it undid the resize - the
    /// full page then sits under a layer sized for the newer request.
    #[test]
    fn a_stale_paint_size_is_not_adopted() {
        assert!(
            paint_matches_request((3072, 1668), (3072, 1668)),
            "the frame CEF paints at the request",
        );
        assert!(
            paint_matches_request((3073, 1668), (3072, 1668)),
            "the DIP round trip's one-pixel error",
        );
        assert!(
            !paint_matches_request((2160, 1242), (3072, 1668)),
            "a frame from before the resize",
        );
        assert!(!paint_matches_request((2160, 1668), (3072, 1668)), "one axis stale is stale");
        assert!(
            !paint_matches_request((3072, 1596), (3072, 1668)),
            "the other axis stale is stale",
        );
        assert!(
            !paint_matches_request((3075, 1668), (3072, 1668)),
            "one pixel past the slack is a different size",
        );
    }

    /// **The painted size is the view CEF laid out, not the raw request.** A
    /// takeover past the ring's budget gets a raster below 1, so CEF paints
    /// the view at 1.0 - larger than the request. Comparing against the raw
    /// request dropped every frame on such a display.
    #[test]
    fn a_raster_below_one_paints_the_view_at_one_to_one() {
        // 1440x2520 at 1x: raster 0.857, request 1234x2160, painted 1440x2520.
        let expected = painted_size(1234, 2160, 857);
        assert_eq!(expected, (1440, 2520), "the view is painted at the display's 1x");
        assert!(
            paint_matches_request((1440, 2520), expected),
            "a raster-below-1 frame is not stale",
        );
        // 5120x1440 at 1x: raster 0.75, request 3840x1080, painted 5120x1440.
        assert_eq!(painted_size(3840, 1080, 750), (5120, 1440));
        // A 1.5x raster is exact: 3072x1668 stays 3072x1668.
        assert_eq!(painted_size(3072, 1668, 1500), (3072, 1668));
    }

    /// **Adopting a paint never becomes the next layout.** `view_rect` and
    /// `screen_info` derive the DIP from the REQUEST, so the frame CEF stores
    /// below a raster of 1 cannot inflate the page; the next paint is the same
    /// size and is accepted. Dividing the adopted paint by the request's scale
    /// would ask for 1680x2940 and drop every later frame.
    #[test]
    fn a_raster_below_one_does_not_inflate_the_view_after_adoption() {
        let (view_width, view_height) = view_size(1234, 2160, 857);
        assert_eq!((view_width, view_height), (1440, 2520), "the view is the request's CSS size");
        assert_eq!(screen_scale(857), 1.0, "the display paints at 1x");
        let expected = painted_size(1234, 2160, 857);
        let inflated = rounded_i32(f64::from(expected.0) / 0.857, 0);
        assert_eq!(inflated, 1680, "what the adopted size would have asked for");
        assert_ne!(view_width, inflated, "the paint size is not turned into the view again");
        assert!(
            paint_matches_request(expected, painted_size(1234, 2160, 857)),
            "a later frame at the same size is still accepted",
        );
    }

    /// **The drop and the adoption are the handler's own behaviour.** A stale
    /// paint must not reach the ring; the paint at the requested size must,
    /// and the pixels it leaves are the buffer's.
    #[test]
    fn a_stale_paint_never_reaches_the_ring() {
        let name = "/forge-cef-test-stale-paint";
        let ring = Ring::create(name, 8, 8).expect("a test ring");
        let buffer: Vec<u8> =
            (0..8 * 8 * 4).map(|at| u8::try_from(at % 256).unwrap_or_default()).collect();
        paint(&ring, None, buffer.as_ptr(), 4, 4, None);
        let header = unsafe { &*ring.header() };
        assert_eq!(header.paints.load(Ordering::Relaxed), 0, "a stale frame is not adopted");
        assert_eq!(header.width.load(Ordering::Relaxed), 8, "and does not overwrite the size");
        paint(&ring, None, buffer.as_ptr(), 8, 8, None);
        assert_eq!(header.paints.load(Ordering::Relaxed), 1, "the matching frame is written");
        assert_eq!(header.width.load(Ordering::Relaxed), 8);
        Ring::unlink(name);
    }

    /// **A frame past the ring's budget is dropped, not copied.** The budget is
    /// in pixels: a size within the DIP slack of the request can still be past
    /// it, and the copy would run past the mapping.
    #[test]
    fn a_paint_past_the_ring_budget_is_dropped() {
        let name = "/forge-cef-test-paint-budget";
        let ring = Ring::create(name, MAX_WIDTH, MAX_HEIGHT).expect("a test ring");
        let buffer = [0_u8; 4 * 4 * 4];
        // The request is clamped to the budget; one pixel past it is within the
        // slack and still unrepresentable.
        let header = unsafe { &*ring.header() };
        header.request_width.store(MAX_WIDTH, Ordering::Relaxed);
        header.request_height.store(MAX_HEIGHT, Ordering::Relaxed);
        header.request_scale_milli.store(1_000, Ordering::Relaxed);
        paint(&ring, None, buffer.as_ptr(), MAX_WIDTH, MAX_HEIGHT + 1, None);
        assert_eq!(header.paints.load(Ordering::Relaxed), 0, "past the budget is dropped");
        assert_eq!(header.width.load(Ordering::Relaxed), MAX_WIDTH, "and the size is not adopted");
        assert!(within_budget(MAX_WIDTH, MAX_HEIGHT), "exactly the budget fits");
        assert!(!within_budget(MAX_WIDTH, MAX_HEIGHT + 1), "one row past does not");
        assert!(!within_budget(MAX_WIDTH + 1, MAX_HEIGHT), "one column past does not");
        assert!(within_budget(1440, 2520), "a tall portrait frame within the budget fits");
        Ring::unlink(name);
    }

    /// **The show cue is a sequence the host reads and acknowledges.** The
    /// wait is what stops a pump already in flight from serving a call
    /// against the hidden page.
    #[test]
    fn the_show_cue_advances_a_sequence_the_host_acknowledges() {
        let name = "/forge-cef-test-show";
        let ring = Ring::create(name, 4, 4).expect("a test ring");
        // SAFETY: the ring is mapped for this process and holds a header.
        let header = unsafe { &*ring.header() };
        assert_eq!(header.show_seq.load(Ordering::Acquire), 0, "a fresh host has shown nothing");
        assert_eq!(ring.show(), 1, "the first cue is sequence one");
        assert!(!ring.shown(1), "nothing is applied before the host runs");
        header.show_applied.store(1, Ordering::Release);
        assert!(ring.shown(1), "the applied cue is acknowledged");
        assert!(!ring.shown(2), "a later cue is not");
        // **`>=`, not `==`**: a later cue's application keeps the earlier
        // one acknowledged, which is what a driver waiting on its own
        // sequence relies on.
        header.show_applied.store(2, Ordering::Release);
        assert!(ring.shown(1), "an earlier cue stays applied once a later one is");
        assert!(ring.shown(2), "the current cue is applied");
        assert_eq!(ring.show(), 2, "each call asks once");
        Ring::unlink(name);
    }

    /// **Another build's ring is refused, not drawn.** The pixel base is
    /// `HEADER_LEN`, so an older header would shift every scanline; the
    /// caller ends the host instead. The control is this build's own ring,
    /// which opens.
    #[test]
    fn open_refuses_a_ring_from_another_build() {
        let name = "/forge-cef-test-current";
        let ring = Ring::create(name, 4, 4).expect("a test ring");
        assert!(Ring::open(name).is_ok(), "control: this build's own ring opens");
        // SAFETY: the ring is mapped and holds a header.
        unsafe { (*ring.header()).version.store(1, Ordering::Release) };
        assert!(Ring::open(name).is_err(), "an older version is refused");
        // **The refused segment is left for its own host.** Unlinking another
        // host's ring would take the picture out from under a driver using
        // it; the replacement's own create is what replaces the name.
        let kept = Ring::open(name).err().expect("the refused ring's host still holds it");
        assert!(
            kept.contains("written by another build"),
            "a foreign ring is refused without being unlinked: {kept}"
        );

        // A segment shorter than this header, carrying a valid magic,
        // version and length field: the LENGTH clause must be what refuses
        // it, not the magic read first.
        let short_name = "/forge-cef-test-short";
        let short_len = HEADER_LEN - 8;
        unsafe {
            let cname = CString::new(short_name).expect("a c string");
            libc::shm_unlink(cname.as_ptr());
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            assert!(fd >= 0, "the short ring exists");
            assert_eq!(
                libc::ftruncate(fd, i64::try_from(short_len).unwrap_or(i64::MAX)),
                0,
                "one field short"
            );
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                short_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert!(ptr != libc::MAP_FAILED, "the short ring maps for the header write");
            // SAFETY: the mapping holds the three leading u32s the check
            // reads first, and they are written before the unmap.
            let words = ptr.cast::<u32>();
            words.write(MAGIC);
            words.add(1).write(VERSION);
            words.add(2).write(u32::try_from(HEADER_LEN).unwrap_or(u32::MAX));
            libc::munmap(ptr, short_len);
            libc::close(fd);
        }
        assert!(
            Ring::open(short_name).is_err(),
            "a ring shorter than this header is refused even with its magic and version"
        );
        Ring::unlink(short_name);

        // A full-size segment whose header names another length, with this
        // build's magic and version: the HEADER_LEN clause must be what
        // refuses it. The short segment above is already refused by length,
        // so without this case a header grown while VERSION stayed put would
        // map with every scanline shifted and all tests green.
        let header_name = "/forge-cef-test-header";
        unsafe {
            let cname = CString::new(header_name).expect("a c string");
            libc::shm_unlink(cname.as_ptr());
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            assert!(fd >= 0, "the wrong-header ring exists");
            assert_eq!(
                libc::ftruncate(fd, i64::try_from(RING_LEN).unwrap_or(i64::MAX)),
                0,
                "a full-size mapping"
            );
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                RING_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert!(ptr != libc::MAP_FAILED, "the wrong-header ring maps for the header write");
            // SAFETY: the mapping holds the three leading u32s the check
            // reads first, written before the unmap.
            let words = ptr.cast::<u32>();
            words.write(MAGIC);
            words.add(1).write(VERSION);
            words.add(2).write(u32::try_from(HEADER_LEN).unwrap_or(u32::MAX) + 8);
            libc::munmap(ptr, RING_LEN);
            libc::close(fd);
        }
        assert!(
            Ring::open(header_name).is_err(),
            "a ring whose header names another length is refused at this build's magic and \
             version"
        );
        Ring::unlink(header_name);

        // A full-size segment nobody wrote a header into.
        let blank_name = "/forge-cef-test-blank";
        unsafe {
            let cname = CString::new(blank_name).expect("a c string");
            libc::shm_unlink(cname.as_ptr());
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            assert!(fd >= 0, "the blank ring exists");
            assert_eq!(
                libc::ftruncate(fd, i64::try_from(RING_LEN).unwrap_or(i64::MAX)),
                0,
                "a full-size mapping"
            );
            libc::close(fd);
        }
        assert!(
            Ring::open(blank_name).is_err(),
            "a full-size ring nobody wrote a header into is refused"
        );
        Ring::unlink(blank_name);
        // The refused ring now keeps its name for its own host, so the test
        // takes its own segment with it.
        Ring::unlink(name);
    }

    /// **The offsets are the contract.** A field inserted anywhere before
    /// the end moves the ones after it and changes the pixel base; these
    /// numbers and the version are what `Ring::open` is protecting, so a
    /// silent reorder or a forgotten bump fails here first.
    #[test]
    fn the_header_offsets_are_pinned() {
        assert_eq!(std::mem::offset_of!(Header, magic), 0);
        assert_eq!(std::mem::offset_of!(Header, version), 4);
        assert_eq!(std::mem::offset_of!(Header, header_len), 8);
        assert_eq!(std::mem::offset_of!(Header, width), 12);
        assert_eq!(std::mem::offset_of!(Header, height), 16);
        assert_eq!(std::mem::offset_of!(Header, request_seq), 40);
        assert_eq!(std::mem::offset_of!(Header, state), 48);
        assert_eq!(std::mem::offset_of!(Header, focused), 56);
        assert_eq!(std::mem::offset_of!(Header, frame), 64);
        assert_eq!(std::mem::offset_of!(Header, paints), 72);
        assert_eq!(std::mem::offset_of!(Header, show_seq), 80);
        assert_eq!(std::mem::offset_of!(Header, show_applied), 88);
        assert_eq!(std::mem::offset_of!(Header, key_seq), 96);
        assert_eq!(std::mem::offset_of!(Header, paused), 104);
        assert_eq!(std::mem::offset_of!(Header, pid), 108);
        assert_eq!(HEADER_LEN, 112, "the header this version's pixel base is built on");
        assert_eq!(VERSION, 5, "the version identifies this layout; bump it with any move");
    }

    /// **A dirty-rect write lands exactly what a whole-frame write would.**
    /// CEF sends only the changed rectangle on most paints; a single stale
    /// pixel outside it is the kind of drift a full-frame test cannot see.
    #[test]
    fn a_dirty_rect_write_matches_the_whole_frame() {
        let name = "/forge-cef-test-dirty";
        let ring = Ring::create(name, 6, 4).expect("a test ring");
        let first: Vec<u8> =
            (0..6 * 4 * 4).map(|at| u8::try_from(at % 256).unwrap_or_default()).collect();
        // SAFETY: both buffers are full 6x4 BGRA frames.
        unsafe { ring.write_frame(None, first.as_ptr(), 6, 4) };
        // CEF hands the whole view buffer with the changed rectangle marked;
        // only the marked pixels differ.
        let mut second = first.clone();
        for row in 1..3 {
            for column in 2..5 {
                for channel in 0..4 {
                    let at = (row * 6 + column) * 4 + channel;
                    second[at] = second[at].wrapping_add(7);
                }
            }
        }
        let dirty = [Rect { x: 2, y: 1, width: 3, height: 2 }];
        // SAFETY: as above; the dirty rect covers the only changed pixels.
        unsafe { ring.write_frame(Some(&dirty), second.as_ptr(), 6, 4) };
        // SAFETY: the ring holds the 6x4 pixels the header names.
        let written = unsafe { std::slice::from_raw_parts(ring.pixels(), second.len()) };
        assert_eq!(written, &second[..], "the dirty rect is the whole difference");
        Ring::unlink(name);
    }

    /// **A popup is a separate paint, not a view paint.** CEF hands the
    /// popup as its own buffer over the view; a host that drops it leaves a
    /// select open with nothing drawn, and the clicks keep going to the
    /// invisible popup while the page reads as frozen. The composited frame
    /// must reach the server's stream, at the view's size.
    #[test]
    fn a_popup_paint_is_composited_over_the_view() {
        type Captured = (Vec<u8>, u32, u32, bool);
        let name = "/forge-cef-test-popup";
        let ring = Arc::new(Ring::create(name, 8, 8).expect("a test ring"));
        ring.request(8, 8, 1.0);
        let frames: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&frames);
        let sink: PaintSink = Arc::new(move |pixels, width, height, distinct| {
            captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((
                pixels.to_vec(),
                width,
                height,
                distinct,
            ));
        });
        let handler = OsrRenderHandler {
            ring: Arc::clone(&ring),
            paint: Some(sink),
            state: Arc::new(Mutex::new(PaintState::default())),
            size: None,
            cef_object: std::ptr::null_mut(),
        };
        let view = vec![0_u8; 8 * 8 * 4];
        handler.on_paint(None, PaintElementType::VIEW, None, view.as_ptr(), 8, 8);
        // CEF names the popup's DIP bounds, then paints it.
        let bounds = Rect { x: 2, y: 2, width: 4, height: 4 };
        handler.on_popup_size(None, Some(&bounds));
        let popup = [255_u8; 4 * 4 * 4];
        handler.on_paint(None, PaintElementType::POPUP, None, popup.as_ptr(), 4, 4);

        let frames = frames.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(frames.len(), 2, "the view and the popup are both frames");
        let (pixels, width, height, distinct) = &frames[1];
        assert_eq!((*width, *height), (8, 8), "the composite is the view's size");
        assert!(*distinct, "the popup's frame is never coalesced: it is the one carrying the menu");
        let at = |x: usize, y: usize| pixels[(y * 8 + x) * 4];
        assert_eq!(at(2, 2), 255, "the popup's top-left is drawn where the bounds say");
        assert_eq!(at(5, 5), 255, "and its bottom-right");
        assert_eq!(at(1, 1), 0, "outside the popup the view shows");
        assert_eq!(at(6, 6), 0);
        Ring::unlink(name);
    }

    /// **The bounds are DIP and the paint is device pixels.** The position
    /// follows the factor the paint's own width gives, so a retina popup lands
    /// on its box rather than at half its offset.
    #[test]
    fn a_popup_position_follows_the_paint_scale() {
        let mut state = PaintState {
            bounds: Some(Rect { x: 10, y: 20, width: 5, height: 5 }),
            ..PaintState::default()
        };
        state.take_popup(&vec![0_u8; 10 * 10 * 4], 10, 10);
        let popup = state.popup.as_ref().expect("a popup");
        assert_eq!((popup.x, popup.y), (20, 40), "the paint doubles its DIP bounds");
    }

    /// **A popup is clipped to the view, on every edge.** A menu near the
    /// bottom or left of the page paints past the view; copying it whole
    /// would run past the buffer.
    #[test]
    fn a_popup_is_clipped_to_the_view() {
        let view = vec![0_u8; 4 * 4 * 4];
        let mut over = view.clone();
        let corner = PopupPaint { pixels: vec![9_u8; 2 * 2 * 4], width: 2, height: 2, x: 3, y: 3 };
        blit_popup(&mut over, 4, 4, &corner);
        assert_eq!(over[(3 * 4 + 3) * 4], 9, "the one visible pixel is drawn");
        assert_eq!(over[(3 * 4 + 2) * 4], 0, "and the clipped rest is not");

        let mut outside = view.clone();
        let away = PopupPaint { pixels: vec![9_u8; 2 * 2 * 4], width: 2, height: 2, x: 10, y: 10 };
        blit_popup(&mut outside, 4, 4, &away);
        assert_eq!(outside, view, "a popup past the view draws nothing");

        let mut negative = view.clone();
        let left = PopupPaint { pixels: vec![7_u8; 2 * 2 * 4], width: 2, height: 2, x: -1, y: -1 };
        blit_popup(&mut negative, 4, 4, &left);
        assert_eq!(negative[0], 7, "a negative origin keeps its visible corner");
        assert_eq!(negative[4], 0, "and only that");
    }

    /// **A take lays out its own tab, not the profile's.** The recorded
    /// tab's handler reports the take's size while every other tab keeps
    /// what the controller's surface asked for.
    #[test]
    fn a_tabs_view_size_can_differ_from_the_profiles_request() {
        use super::ViewSize;
        use std::sync::Arc;

        let name = format!("/forge-cef-test-view-size-{}", std::process::id());
        let ring = Arc::new(Ring::create(&name, 4, 4).expect("a test ring"));
        ring.request(1440, 900, 1.0);
        let recorded = ViewSize::of_ring(&ring);
        let other = ViewSize::of_ring(&ring);
        recorded.set(1280, 720, 1.0);

        let size_of = |size: Arc<ViewSize>| {
            let handler = OsrRenderHandler {
                ring: Arc::clone(&ring),
                paint: None,
                state: Arc::new(Mutex::new(PaintState::default())),
                size: Some(size),
                cef_object: std::ptr::null_mut(),
            };
            let mut rect = Rect { x: 0, y: 0, width: 0, height: 0 };
            handler.view_rect(None, Some(&mut rect));
            (rect.width, rect.height)
        };
        assert_eq!(size_of(recorded), (1280, 720), "the recorded tab takes the take's size");
        assert_eq!(size_of(other), (1440, 900), "another tab keeps the profile's request");

        // **The paint gate follows the tab's own size.** The take's frames
        // are 1280x720 while the profile's request is 1440x900; a gate keyed
        // only on the profile request dropped every recorded frame as stale.
        let take = vec![0_u8; 1280 * 720 * 4];
        super::paint(
            &ring,
            None,
            take.as_ptr(),
            1280,
            720,
            Some(super::painted_size(1280, 720, 1_000)),
        );
        // SAFETY: the header is this test's mapped ring.
        let header = unsafe { &*ring.header() };
        assert_eq!(
            (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed)),
            (1280, 720),
            "the take's frame is the one the ring holds",
        );
        // The same frame is stale for a tab that still expects the profile's
        // request, and does not replace what the recorded tab wrote.
        super::paint(
            &ring,
            None,
            take.as_ptr(),
            1280,
            720,
            Some(super::painted_size(1440, 900, 1_000)),
        );
        assert_eq!(
            (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed)),
            (1280, 720),
            "another tab's gate does not adopt the take's size",
        );
        Ring::unlink(&name);
    }

    /// **`emit` carries the tab's own size through to the ring gate.** The
    /// handler's expected size is the only thing that lets a take's 1280x720
    /// frame into a ring whose profile request is 1440x900, and it is also
    /// what refuses that frame for a tab still laid out at the request.
    #[test]
    fn the_emitted_frame_is_gated_by_the_tabs_own_size() {
        use super::{PaintState, emit, painted_size};

        let name = format!("/forge-cef-test-emit-{}", std::process::id());
        let ring = Ring::create(&name, 4, 4).expect("a test ring");
        ring.request(1440, 900, 1.0);
        let pixels = vec![0_u8; 1280 * 720 * 4];

        let mut recorded = PaintState::default();
        recorded.take_view(&pixels, 1280, 720);
        emit(&ring, None, &mut recorded, None, false, Some(painted_size(1280, 720, 1_000)));
        // SAFETY: the header is this test's mapped ring.
        let header = unsafe { &*ring.header() };
        assert_eq!(
            (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed)),
            (1280, 720),
            "the take's frame is written",
        );

        let mut other = PaintState::default();
        other.take_view(&pixels, 1280, 720);
        emit(&ring, None, &mut other, None, false, Some(painted_size(1440, 900, 1_000)));
        assert_eq!(
            (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed)),
            (1280, 720),
            "a tab laid out at the profile request refuses it",
        );
        Ring::unlink(&name);
    }
}
