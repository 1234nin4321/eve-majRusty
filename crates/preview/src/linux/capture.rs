//! Getting a client window's pixels into a buffer we own, for drawing into its thumbnail.
//!
//! Every capture protocol follows the same shape: start a frame for a window, receive the buffer types the compositor
//! can fill (wl_shm and/or dmabuf), hand it a buffer, wait for "ready". The protocol-specific part is only
//! [`CaptureProtocol`]/[`CaptureFrame`] plus a `Dispatch` impl that forwards events to the shared handlers here
//! (`on_shm_offer`, `on_dmabuf_offer`, `on_buffer_done`, `on_frame_ready`, ...). The buffer side is shared: a GBM
//! dmabuf imported as a GL texture (zero CPU copies) or a wl_shm fallback, double-buffered so the compositor writes
//! one while the other is on screen.
//!
//! Implemented: hyprland-toplevel-export-v1. The standard one to add for other compositors is
//! ext-image-copy-capture-v1 with ext-foreign-toplevel-image-capture-source-v1.

use std::ffi::c_void;
use std::os::fd::{AsFd, AsRawFd};

use eve_maj_core::log::Scope;
use khronos_egl as egl;
use wayland_client::protocol::{wl_buffer::WlBuffer, wl_shm::Format as ShmFormat, wl_shm_pool::WlShmPool};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};

use super::backend::WindowId;
use super::gpu;
use super::protocols::hyprland_toplevel_export::{
    hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1},
    hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1,
};
use super::App;

const SLOG: Scope = Scope::new("capture");
/// Captures that keep failing back off to one try in this many capture ticks.
const FAIL_BACKOFF: u32 = 30;

/// The protocol used to capture windows.
pub enum CaptureProtocol {
    HyprlandExport(HyprlandToplevelExportManagerV1),
}

/// One in-flight capture of one window.
pub enum CaptureFrame {
    HyprlandExport(HyprlandToplevelExportFrameV1),
}

impl CaptureProtocol {
    pub fn name(&self) -> &'static str {
        match self {
            Self::HyprlandExport(_) => "hyprland-toplevel-export",
        }
    }

    fn start(&self, window: WindowId, thumb: u64, qh: &QueueHandle<App>) -> CaptureFrame {
        match self {
            // Hyprland identifies windows by the low 32 bits of their address.
            Self::HyprlandExport(mgr) => CaptureFrame::HyprlandExport(mgr.capture_toplevel(0, (window & 0xffff_ffff) as u32, qh, thumb)),
        }
    }
}

impl CaptureFrame {
    /// `wait_for_damage`: let the compositor hold the copy until the window changes (protocols that support it).
    fn copy(&self, buffer: &WlBuffer, wait_for_damage: bool) {
        match self {
            Self::HyprlandExport(f) => f.copy(buffer, (!wait_for_damage) as i32),
        }
    }

    fn destroy(self) {
        match self {
            Self::HyprlandExport(f) => f.destroy(),
        }
    }

    fn is(&self, proxy: &impl Proxy) -> bool {
        match self {
            Self::HyprlandExport(f) => f.id() == proxy.id(),
        }
    }
}

#[derive(Default)]
pub struct CapBuf {
    wl: Option<WlBuffer>,
    ready: bool,
    pub dmabuf: bool,
    pub width: u32,
    pub height: u32,
    format: u32,
    pub stride: u32,
    bo: Option<gbm::BufferObject<()>>,
    image: Option<egl::Image>,
    pub tex: Option<glow::Texture>,
    shm: Option<(*mut u8, usize)>,
}

#[derive(Default)]
pub struct Capture {
    frame: Option<CaptureFrame>,
    bufs: [CapBuf; 2],
    /// Index shown on screen; the other one is the capture target.
    front: usize,
    pub have_image: bool,
    first_done: bool,
    pub y_invert: bool,
    shm_offer: Option<(u32, u32, u32, u32)>,
    dma_offer: Option<(u32, u32, u32)>,
    fails: u32,
}

impl Capture {
    /// The buffer currently on screen.
    pub fn front(&self) -> &CapBuf {
        &self.bufs[self.front]
    }
}

impl App {
    pub(super) fn request_capture(&mut self, i: usize) {
        let t = &mut self.thumbs[i];
        if t.cap.frame.is_some() || !t.surf.as_ref().is_some_and(|s| s.configured) {
            return;
        }
        if t.cap.fails >= 5 {
            t.cap.fails += 1;
            if t.cap.fails % FAIL_BACKOFF != 0 {
                return;
            }
        }
        t.cap.shm_offer = None;
        t.cap.dma_offer = None;
        t.cap.frame = Some(self.capture.start(t.window, t.id, &self.qh));
    }

    /// Releases everything a thumbnail's capture holds.
    pub(super) fn free_capture(&self, cap: &mut Capture) {
        if let Some(f) = cap.frame.take() {
            f.destroy();
        }
        for b in &mut cap.bufs {
            self.free_capbuf(b);
        }
    }

    fn free_capbuf(&self, b: &mut CapBuf) {
        self.gpu.make_current_surfaceless();
        if let Some(wl) = b.wl.take() {
            wl.destroy();
        }
        if let Some(img) = b.image.take() {
            self.gpu.destroy_image(img);
        }
        if let Some(tex) = b.tex.take() {
            self.gpu.delete_texture(tex);
        }
        if let Some((ptr, len)) = b.shm.take() {
            unsafe { libc::munmap(ptr as *mut c_void, len) };
        }
        *b = CapBuf::default();
    }

    // Shared event handlers that each protocol's Dispatch forwards to.

    fn on_shm_offer(&mut self, i: usize, format: u32, width: u32, height: u32, stride: u32) {
        self.thumbs[i].cap.shm_offer = Some((format, width, height, stride));
    }

    fn on_dmabuf_offer(&mut self, i: usize, fourcc: u32, width: u32, height: u32) {
        self.thumbs[i].cap.dma_offer = Some((fourcc, width, height));
    }

    fn on_frame_failed(&mut self, i: usize) {
        let c = &mut self.thumbs[i].cap;
        if let Some(f) = c.frame.take() {
            f.destroy();
        }
        c.fails += 1;
        if c.fails == 5 {
            SLOG.warn(format_args!("Capture keeps failing for {}", self.thumbs[i].name));
        }
    }

    /// The compositor has listed every buffer type it accepts: reuse or allocate the back buffer, then copy.
    fn on_buffer_done(&mut self, i: usize) {
        let back = 1 - self.thumbs[i].cap.front;
        let want_dma = self.use_dmabuf && self.thumbs[i].cap.dma_offer.is_some();
        let reusable = {
            let c = &self.thumbs[i].cap;
            let b = &c.bufs[back];
            b.ready
                && b.dmabuf == want_dma
                && match (want_dma, c.dma_offer, c.shm_offer) {
                    (true, Some((f, w, h)), _) => (b.format, b.width, b.height) == (f, w, h),
                    (false, _, Some((f, w, h, s))) => (b.format, b.width, b.height, b.stride) == (f, w, h, s),
                    _ => false,
                }
        };
        if reusable {
            self.start_copy(i);
            return;
        }
        let mut b = std::mem::take(&mut self.thumbs[i].cap.bufs[back]);
        self.free_capbuf(&mut b);
        if want_dma {
            if let Some(nb) = self.alloc_dmabuf(i, back) {
                self.thumbs[i].cap.bufs[back] = nb;
                return; // the copy is issued once the compositor accepts the buffer
            }
            SLOG.warn(format_args!("dmabuf allocation failed, falling back to wl_shm"));
            self.use_dmabuf = false;
        }
        match self.thumbs[i].cap.shm_offer.and_then(|offer| self.alloc_shm(offer)) {
            Some(nb) => {
                self.thumbs[i].cap.bufs[back] = nb;
                self.start_copy(i);
            }
            None => self.on_frame_failed(i),
        }
    }

    fn alloc_dmabuf(&mut self, i: usize, slot: usize) -> Option<CapBuf> {
        let (fourcc, width, height) = self.thumbs[i].cap.dma_offer?;
        let gbm = self.gpu.gbm.as_ref()?;
        let format = drm_fourcc::DrmFourcc::try_from(fourcc).ok()?;
        let mut flags = gbm::BufferObjectFlags::RENDERING;
        if self.linear {
            flags |= gbm::BufferObjectFlags::LINEAR;
        }
        let bo = match gbm.create_buffer_object::<()>(width, height, format, flags) {
            Ok(bo) => bo,
            Err(e) => {
                SLOG.warn(format_args!("gbm_bo_create {width}x{height} {format:?}: {e}"));
                return None;
            }
        };
        let modifier: u64 = bo.modifier().into();
        let mut fds = Vec::new();
        let mut planes = Vec::new();
        for p in 0..bo.plane_count() as i32 {
            let fd = bo.fd_for_plane(p).ok()?;
            planes.push(gpu::DmabufPlane { fd: fd.as_raw_fd(), offset: bo.offset(p), stride: bo.stride_for_plane(p) });
            fds.push(fd);
        }
        let (image, tex) = self.gpu.import_dmabuf(width, height, fourcc, modifier, &planes)?;
        let params = self.dmabuf.as_ref()?.create_params(&self.qh, (self.thumbs[i].id, slot));
        for (p, fd) in fds.iter().enumerate() {
            params.add(fd.as_fd(), p as u32, planes[p].offset, planes[p].stride, (modifier >> 32) as u32, (modifier & 0xffff_ffff) as u32);
        }
        params.create(width as i32, height as i32, fourcc, zwp_linux_buffer_params_v1::Flags::empty());
        SLOG.debug(format_args!("dmabuf {width}x{height} {format:?} mod 0x{modifier:x} for {}", self.thumbs[i].name));
        Some(CapBuf { wl: None, ready: false, dmabuf: true, width, height, format: fourcc, stride: planes[0].stride, bo: Some(bo), image: Some(image), tex: Some(tex), shm: None })
    }

    fn alloc_shm(&self, (format, width, height, stride): (u32, u32, u32, u32)) -> Option<CapBuf> {
        use std::os::fd::FromRawFd;
        let size = (stride * height) as usize;
        unsafe {
            let fd = libc::memfd_create(c"eve-maj-capture".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let owned = std::os::fd::OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(fd, size as i64) < 0 {
                return None;
            }
            let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            if ptr == libc::MAP_FAILED {
                return None;
            }
            let Ok(shm_format) = ShmFormat::try_from(format) else {
                libc::munmap(ptr, size);
                return None;
            };
            let pool: WlShmPool = self.shm.create_pool(owned.as_fd(), size as i32, &self.qh, ());
            let wl = pool.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, &self.qh, ());
            pool.destroy();
            self.gpu.make_current_surfaceless();
            let tex = self.gpu.new_texture();
            Some(CapBuf { wl: Some(wl), ready: true, dmabuf: false, width, height, format, stride, bo: None, image: None, tex, shm: Some((ptr as *mut u8, size)) })
        }
    }

    fn start_copy(&mut self, i: usize) {
        let t = &self.thumbs[i];
        let back = &t.cap.bufs[1 - t.cap.front];
        let (Some(frame), Some(buffer)) = (&t.cap.frame, &back.wl) else { return };
        if !back.ready {
            return;
        }
        // The first copy must not wait for damage, or a static window would never show up.
        frame.copy(buffer, t.cap.first_done);
    }

    fn on_frame_ready(&mut self, i: usize) {
        let back_index = 1 - self.thumbs[i].cap.front;
        if let Some(f) = self.thumbs[i].cap.frame.take() {
            f.destroy();
        }
        {
            let b = &self.thumbs[i].cap.bufs[back_index];
            if let (false, Some((ptr, len)), Some(tex)) = (b.dmabuf, b.shm, b.tex) {
                let data = unsafe { std::slice::from_raw_parts(ptr, len) };
                self.gpu.make_current_surfaceless();
                self.gpu.upload_rgba(tex, b.width, b.height, b.stride, data);
            }
        }
        let c = &mut self.thumbs[i].cap;
        c.front = back_index;
        c.have_image = true;
        c.first_done = true;
        c.fails = 0;
        self.thumbs[i].needs_draw = true;
        self.draw(i);
    }

    /// The thumbnail this frame belongs to, if it's still that thumbnail's current frame.
    fn frame_owner(&self, id: u64, frame: &impl Proxy) -> Option<usize> {
        let i = self.thumb_index(id)?;
        self.thumbs[i].cap.frame.as_ref().is_some_and(|f| f.is(frame)).then_some(i)
    }
}

// --------------------------------------------------------------------------------------- hyprland-toplevel-export --

impl Dispatch<HyprlandToplevelExportFrameV1, u64> for App {
    fn event(state: &mut Self, frame: &HyprlandToplevelExportFrameV1, event: hyprland_toplevel_export_frame_v1::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        use hyprland_toplevel_export_frame_v1::Event;
        let Some(i) = state.frame_owner(*id, frame) else {
            // A frame for a thumbnail that's gone; let it go once the compositor is done with it.
            if matches!(event, Event::Ready { .. } | Event::Failed) {
                frame.destroy();
            }
            return;
        };
        match event {
            Event::Buffer { format, width, height, stride } => {
                let format = match format {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(u) => u,
                };
                state.on_shm_offer(i, format, width, height, stride);
            }
            Event::LinuxDmabuf { format, width, height } => state.on_dmabuf_offer(i, format, width, height),
            Event::BufferDone => state.on_buffer_done(i),
            Event::Flags { flags } => {
                let bits = match flags {
                    WEnum::Value(f) => f.bits(),
                    WEnum::Unknown(u) => u,
                };
                state.thumbs[i].cap.y_invert = bits & 1 != 0;
            }
            Event::Ready { .. } => state.on_frame_ready(i),
            Event::Failed => state.on_frame_failed(i),
            _ => {}
        }
    }
}

// ------------------------------------------------------------------------------------- dmabuf buffer creation --

impl Dispatch<ZwpLinuxBufferParamsV1, (u64, usize)> for App {
    fn event(state: &mut Self, params: &ZwpLinuxBufferParamsV1, event: zwp_linux_buffer_params_v1::Event, data: &(u64, usize), _: &Connection, _: &QueueHandle<Self>) {
        let (id, slot) = *data;
        params.destroy();
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                let Some(i) = state.thumb_index(id) else {
                    buffer.destroy();
                    return;
                };
                let b = &mut state.thumbs[i].cap.bufs[slot];
                if !b.dmabuf || b.wl.is_some() {
                    buffer.destroy();
                    return;
                }
                b.wl = Some(buffer);
                b.ready = true;
                if 1 - state.thumbs[i].cap.front == slot {
                    state.start_copy(i);
                }
            }
            zwp_linux_buffer_params_v1::Event::Failed => {
                SLOG.warn(format_args!("Compositor rejected our dmabuf; falling back to wl_shm (CPU copies)"));
                state.use_dmabuf = false;
                let Some(i) = state.thumb_index(id) else { return };
                let mut b = std::mem::take(&mut state.thumbs[i].cap.bufs[slot]);
                state.free_capbuf(&mut b);
                if state.thumbs[i].cap.frame.is_some() {
                    state.on_buffer_done(i);
                }
            }
            _ => {}
        }
    }

    event_created_child!(App, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ()),
    ]);
}
