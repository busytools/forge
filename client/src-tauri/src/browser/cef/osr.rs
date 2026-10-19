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
const VERSION: u32 = 1;

/// What the host is doing, for the client's state line.
pub const STATE_STARTING: u32 = 0;
pub const STATE_LIVE: u32 = 1;
pub const STATE_FAILED: u32 = 2;

#[repr(C)]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
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
}

pub const HEADER_LEN: usize = std::mem::size_of::<Header>();

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
        let len = HEADER_LEN + MAX_WIDTH as usize * MAX_HEIGHT as usize * 4;
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
                tauri_plugin_log::log::info!(
                    "replaced a stale frame ring (event_name cef_ring_replaced, ring {name})"
                );
            }
            let ring = Ring { name: name.to_owned(), ptr: ptr.cast(), len };
            let header = ring.header();
            std::ptr::write(
                header,
                Header {
                    magic: AtomicU32::new(MAGIC),
                    version: AtomicU32::new(VERSION),
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
            Ok(Ring { name: name.to_owned(), ptr: ptr.cast(), len })
        }
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
            // SAFETY: CEF hands a BGRA frame of exactly this size.
            unsafe {
                self.ring.write_frame(
                    dirty_rects,
                    buffer,
                    width.max(0) as u32,
                    height.max(0) as u32,
                );
            }
        }
    }
}

/// The render handler: the view's size and the paint destination.
pub fn handler(ring: Arc<Ring>) -> RenderHandler {
    OsrRenderHandler::new(ring)
}
