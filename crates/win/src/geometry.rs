//! RECT/POINT arithmetic and monitor/DPI lookups.

use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

pub fn rect_width(r: RECT) -> i32 {
    r.right - r.left
}

pub fn rect_height(r: RECT) -> i32 {
    r.bottom - r.top
}

pub fn rect_center(r: RECT) -> POINT {
    POINT { x: r.left + rect_width(r) / 2, y: r.top + rect_height(r) / 2 }
}

/// Half-open like GDI: the left/top edges are inside, right/bottom aren't.
pub fn rect_contains(r: RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

/// Pulls each edge of `rect` into `bounds`.
pub fn clamp_rect(rect: RECT, bounds: RECT) -> RECT {
    RECT {
        left: rect.left.clamp(bounds.left, bounds.right),
        top: rect.top.clamp(bounds.top, bounds.bottom),
        right: rect.right.clamp(bounds.left, bounds.right),
        bottom: rect.bottom.clamp(bounds.top, bounds.bottom),
    }
}

pub fn dpi_to_scale(dpi: u32) -> f32 {
    dpi as f32 / 96.0
}

/// Scales a logical (96-DPI) pixel value to the given monitor scale factor, rounding to nearest.
pub fn scale_pixels(value: i32, scale: f32) -> i32 {
    if scale == 1.0 {
        return value;
    }
    (value as f32 * scale).round() as i32
}

/// The monitor containing `pt`, or the closest one if it's off every screen.
pub fn nearest_monitor(pt: POINT) -> Option<HMONITOR> {
    let m = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    (!m.is_null()).then_some(m)
}

/// Effective DPI of `monitor`, defaulting to 96 if the query fails.
pub fn monitor_dpi(monitor: HMONITOR) -> u32 {
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    dpi_x
}

/// Full bounds of `monitor` (taskbar included), or None if the lookup fails.
pub fn monitor_rect(monitor: HMONITOR) -> Option<RECT> {
    monitor_info(monitor).map(|i| i.rcMonitor)
}

/// Work area of `monitor` (taskbar excluded), or None if the lookup fails.
pub fn monitor_work_rect(monitor: HMONITOR) -> Option<RECT> {
    monitor_info(monitor).map(|i| i.rcWork)
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then_some(info)
}
