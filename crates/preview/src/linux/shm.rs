//! One-shot wl_shm buffers for the CPU-drawn surfaces (the region selection overlay and the panels): each repaint
//! gets a fresh ARGB8888 buffer, unmapped and destroyed once the compositor releases it.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_shm::Format as ShmFormat;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::App;

/// A wl_shm buffer's mapping, unmapped once the compositor releases the buffer.
pub(super) struct ShmMapping {
    ptr: usize,
    len: usize,
}

impl Dispatch<WlBuffer, ShmMapping> for App {
    fn event(_: &mut Self, buffer: &WlBuffer, event: wl_buffer::Event, mapping: &ShmMapping, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
            unsafe { libc::munmap(mapping.ptr as *mut libc::c_void, mapping.len) };
        }
    }
}

/// A fresh ARGB8888 (premultiplied, cairo's ARgb32) wl_shm buffer of `w`x`h`, and where its pixels are mapped.
pub(super) fn alloc_buffer(app: &App, w: i32, h: i32) -> Option<(WlBuffer, *mut u8)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let stride = w.checked_mul(4)?;
    let len = (stride as usize).checked_mul(h as usize)?;
    unsafe {
        let fd = libc::memfd_create(c"eve-maj-shm".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return None;
        }
        let owned = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd, len as i64) < 0 {
            return None;
        }
        let ptr = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
        if ptr == libc::MAP_FAILED {
            return None;
        }
        let pool: WlShmPool = app.shm.create_pool(owned.as_fd(), len as i32, &app.qh, ());
        let buffer = pool.create_buffer(0, w, h, stride, ShmFormat::Argb8888, &app.qh, ShmMapping { ptr: ptr as usize, len });
        pool.destroy();
        Some((buffer, ptr as *mut u8))
    }
}
