//! The windowless browser's picture: the render handler and the shared ring
//! the client maps.
//!
//! CEF delivers each paint as BGRA pixels for the view's rect; the handler
//! copies the dirty rects into a POSIX shared-memory object named after the
//! host's port, so the client's UI process can draw the same bytes without a
//! socket hop per frame. A monotonic frame counter makes the ring a seqlock:
//! odd while a paint is being written, even when the whole view is readable.

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use cef::rc::Rc as _;
use cef::{
    Browser, ImplRenderHandler, PaintElementType, Rect, RenderHandler, ScreenInfo,
    WrapRenderHandler,
};

/// The largest surface a host allocates room for: a 4K display's worth of
/// device pixels. A bigger region is clamped rather than refused - the
/// takeover never fills more than a 4K screen.
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2160;

const MAGIC: u32 = 0x4345_4652;
/// **Bumped whenever the header grows or its fields move.** The pixel base is
/// `HEADER_LEN`, so a client drawing an older host's ring would shift every
/// scanline; `Ring::open` refuses a version or length it does not own and the
/// host is replaced instead.
const VERSION: u32 = 2;

/// What the host is doing, for the client's state line.
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
    /// The live paint size, in device pixels: what `on_paint` writes. The
    /// view's own rect is this divided by the device scale.
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// The display's device pixel ratio, times 1000.
    pub scale_milli: AtomicU32,
    /// The paint size and scale the client last asked for, and the sequence
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
    /// The client's cue that a call is about to drive the page: the host
    /// re-asserts the page visible when it changes.
    pub show_seq: AtomicU64,
    /// The cue the host has applied, so the client can wait (bounded) before
    /// it drives the call - a pump already in flight must not serve the call
    /// against the page the cue is about to show.
    pub show_applied: AtomicU64,
}

pub const HEADER_LEN: usize = std::mem::size_of::<Header>();

/// A ring this build created or can safely cue: the header and its pixels.
pub const RING_LEN: usize = HEADER_LEN + MAX_WIDTH as usize * MAX_HEIGHT as usize * 4;

/// The ring, as the host owns it and the client maps it.
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
            if libc::ftruncate(fd, len as i64) != 0 {
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
                    header_len: AtomicU32::new(HEADER_LEN as u32),
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
            if libc::fstat(fd, &mut stat) != 0 {
                libc::close(fd);
                return Err(format!("the frame ring {name} cannot be read"));
            }
            let len = stat.st_size as usize;
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
            && header.header_len.load(Ordering::Acquire) as usize == HEADER_LEN
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
    pub fn request(&self, width: u32, height: u32, scale: f64) {
        // SAFETY: the header is mapped; the sequence is published last so the
        // host never reads a half-written request.
        unsafe {
            let header = &*self.header();
            header.request_width.store(width.clamp(1, MAX_WIDTH), Ordering::Relaxed);
            header.request_height.store(height.clamp(1, MAX_HEIGHT), Ordering::Relaxed);
            header
                .request_scale_milli
                .store((scale * 1000.0).round().clamp(100.0, 8_000.0) as u32, Ordering::Relaxed);
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
        // SAFETY: the ring holds MAX_WIDTH*MAX_HEIGHT pixels after the
        // header; every rect is clipped to the live view.
        unsafe {
            let pixels = self.ptr.add(HEADER_LEN);
            frame.frame.fetch_add(1, Ordering::AcqRel);
            match rects {
                Some(rects) if !rects.is_empty() => {
                    for rect in rects {
                        let x = rect.x.max(0) as usize;
                        let y = rect.y.max(0) as usize;
                        let w = rect.width.max(0) as usize;
                        let h = rect.height.max(0) as usize;
                        let x1 = (x + w).min(width as usize);
                        let y1 = (y + h).min(height as usize);
                        if x1 <= x {
                            continue;
                        }
                        for row in y..y1 {
                            let offset = (row * width as usize + x) * 4;
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
                        width as usize * height as usize * 4,
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

cef::wrap_render_handler! {
    struct OsrRenderHandler {
        ring: Arc<Ring>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            // SAFETY: the header is mapped for this process's whole life.
            let header = unsafe { &*self.ring.header() };
            let scale = header.scale_milli.load(Ordering::Relaxed) as f64 / 1000.0;
            rect.x = 0;
            rect.y = 0;
            // **The view rect is DIP; the paint buffer is device pixels.**
            // CEF lays the page out at the DIP size and paints it at the
            // scale `screen_info` reports, so the layer's contents land 1:1
            // on the display.
            rect.width = (header.width.load(Ordering::Relaxed) as f64 / scale).round() as i32;
            rect.height = (header.height.load(Ordering::Relaxed) as f64 / scale).round() as i32;
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            info: Option<&mut ScreenInfo>,
        ) -> i32 {
            let Some(info) = info else { return 0 };
            // SAFETY: the header is mapped for this process's whole life.
            let header = unsafe { &*self.ring.header() };
            let scale = header.scale_milli.load(Ordering::Relaxed) as f32 / 1000.0;
            info.device_scale_factor = scale.max(1.0);
            info.depth = 24;
            info.depth_per_component = 8;
            info.is_monochrome = 0;
            info.rect = Rect {
                x: 0,
                y: 0,
                width: (header.width.load(Ordering::Relaxed) as f32 / scale).round() as i32,
                height: (header.height.load(Ordering::Relaxed) as f32 / scale).round() as i32,
            };
            info.available_rect = info.rect.clone();
            1
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
            if type_ != PaintElementType::VIEW {
                return;
            }
            let width = width.max(0) as u32;
            let height = height.max(0) as u32;
            // **The delivered size is the view's size.** The DIP round trip
            // through `view_rect` and `screen_info` can land a pixel off the
            // request, and a frame painted at a different size than the
            // header names used to be dropped for ever. The host adopts what
            // CEF actually painted, so the ring always describes its frame.
            let header = unsafe { &*self.ring.header() };
            header.width.store(width, Ordering::Relaxed);
            header.height.store(height, Ordering::Relaxed);
            // SAFETY: CEF hands a BGRA frame of exactly this size.
            unsafe {
                self.ring.write_frame(dirty_rects, buffer, width, height);
            }
        }
    }
}

/// The render handler: the view's size and the paint destination.
pub fn handler(ring: Arc<Ring>) -> RenderHandler {
    OsrRenderHandler::new(ring)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The frame is the bytes and the counter is a seqlock.** A reader
    /// takes an even counter and the pixels behind it; a frame of another
    /// size must not be written into a rectangle the header does not name,
    /// which is the dropped-paint defect the adoption in `on_paint` fixes.
    #[test]
    fn a_frame_is_written_whole_and_never_at_another_size() {
        let name = "/forge-cef-test-frame";
        let ring = Ring::create(name, 4, 4).expect("a test ring");
        let pixels: Vec<u8> = (0..4 * 4 * 4).map(|at| at as u8).collect();
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
        // one acknowledged, which is what a client waiting on its own
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
        // host's ring would take the picture out from under a client driving
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
            assert_eq!(libc::ftruncate(fd, short_len as i64), 0, "one field short");
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
            words.add(2).write(HEADER_LEN as u32);
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
            assert_eq!(libc::ftruncate(fd, RING_LEN as i64), 0, "a full-size mapping");
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
            // reads, written before the unmap.
            let words = ptr.cast::<u32>();
            words.write(MAGIC);
            words.add(1).write(VERSION);
            words.add(2).write(HEADER_LEN as u32 + 8);
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
            assert_eq!(libc::ftruncate(fd, RING_LEN as i64), 0, "a full-size mapping");
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
        assert_eq!(HEADER_LEN, 96, "the header this version's pixel base is built on");
        assert_eq!(VERSION, 2, "the version identifies this layout; bump it with any move");
    }

    /// **A dirty-rect write lands exactly what a whole-frame write would.**
    /// CEF sends only the changed rectangle on most paints; a single stale
    /// pixel outside it is the kind of drift a full-frame test cannot see.
    #[test]
    fn a_dirty_rect_write_matches_the_whole_frame() {
        let name = "/forge-cef-test-dirty";
        let ring = Ring::create(name, 6, 4).expect("a test ring");
        let first: Vec<u8> = (0..6 * 4 * 4).map(|at| at as u8).collect();
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
}
