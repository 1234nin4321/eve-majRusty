//! Shared GDI leaf helpers: a top-down 32bpp DIB section for layered-window overlays, pixel fills, text
//! measure/draw, window-class registration and the draggable-panel trio used by the list and history panels.
//!
//! Text goes through the wide (`...W`) GDI calls, so every string here is UTF-8 in and UTF-16 to GDI; the Zig
//! build used the ANSI calls for ASCII-only text and the wide ones for translated labels.

use std::cell::Cell;

use eve_maj_core::log::Scope;
use eve_maj_core::types::FontWeight;
use eve_maj_win::geometry::{rect_height, rect_width};
use eve_maj_win::wide;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const SLOG: Scope = Scope::new("gdi_overlay");

/// Top-down 32bpp DIB section selected into its own memory DC, for GDI text/shape rendering
/// into a pixel buffer that later becomes a layered window's alpha-blended source.
pub struct OverlayBitmap {
    pub mem_dc: HDC,
    bitmap: HBITMAP,
    pixels: *mut u32,
    pub width: usize,
    pub height: usize,
    old_bitmap: HGDIOBJ,
}

impl OverlayBitmap {
    pub fn create(screen_dc: HDC, width: i32, height: i32) -> Option<OverlayBitmap> {
        unsafe {
            let mem_dc = CreateCompatibleDC(screen_dc);
            if mem_dc.is_null() {
                return None;
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = width;
            // Negative height selects top-down row order.
            bmi.bmiHeader.biHeight = -height;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;

            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(mem_dc);
                return None;
            }
            let old_bitmap = SelectObject(mem_dc, bitmap);
            if old_bitmap.is_null() {
                DeleteObject(bitmap);
                DeleteDC(mem_dc);
                return None;
            }
            Some(OverlayBitmap {
                mem_dc,
                bitmap,
                pixels: bits.cast(),
                width: width.max(0) as usize,
                height: height.max(0) as usize,
                old_bitmap,
            })
        }
    }

    pub fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the DIB section holds exactly width*height 32-bit pixels for as long as self lives.
        unsafe { std::slice::from_raw_parts_mut(self.pixels, self.width * self.height) }
    }

    pub fn needs_resize(existing: &Option<OverlayBitmap>, width: i32, height: i32) -> bool {
        match existing {
            None => true,
            Some(b) => b.width != width.max(0) as usize || b.height != height.max(0) as usize,
        }
    }

    /// Leaves `slot` None, rather than a stale bitmap, if creation fails.
    pub fn recreate(slot: &mut Option<OverlayBitmap>, screen_dc: HDC, width: i32, height: i32) -> bool {
        *slot = None;
        *slot = OverlayBitmap::create(screen_dc, width, height);
        slot.is_some()
    }
}

impl Drop for OverlayBitmap {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.old_bitmap);
            DeleteObject(self.bitmap);
            DeleteDC(self.mem_dc);
        }
    }
}

/// Converts this app's 0xAARRGGBB color into a Win32 COLORREF (0x00BBGGRR) for GDI APIs; without this, SetTextColor swaps red and blue.
pub fn to_color_ref(color: u32) -> u32 {
    let r = (color >> 16) & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = color & 0xFF;
    (b << 16) | (g << 8) | r
}

/// GDI text rendering leaves the alpha byte at 0; sets alpha=255 on every pixel with non-zero RGB still at alpha 0 within the given rect, without touching alpha other drawing code already set.
pub fn fix_text_alpha_rect(pixels: &mut [u32], width: usize, height: usize, x: i32, y: i32, w: usize, h: usize) {
    let start_x = x.max(0) as usize;
    let start_y = y.max(0) as usize;
    let end_x = (start_x + w).min(width);
    let end_y = (start_y + h).min(height);
    if end_x <= start_x || end_y <= start_y {
        return;
    }
    for py in start_y..end_y {
        for p in &mut pixels[py * width + start_x..py * width + end_x] {
            let v = *p;
            if (v >> 24) == 0 && (v & 0x00FF_FFFF) != 0 {
                *p = v | 0xFF00_0000;
            }
        }
    }
}

/// Same as fix_text_alpha_rect but over the whole buffer.
pub fn fix_text_alpha(pixels: &mut [u32], width: usize, height: usize) {
    fix_text_alpha_rect(pixels, width, height, 0, 0, width, height);
}

/// No-ops if the rect would overrun the buffer's row width or its height.
#[allow(clippy::too_many_arguments)]
pub fn fill_rect(pixels: &mut [u32], stride: usize, height: usize, x: usize, y: usize, w: usize, h: usize, argb: u32) {
    let end_y = y + h;
    let end_x = x + w;
    if end_x > stride || end_y > height {
        return;
    }
    for py in y..end_y {
        pixels[py * stride + x..py * stride + end_x].fill(argb);
    }
}

/// Translucent black backing shared by the small hint boxes and labels drawn over the desktop.
pub const HINT_BG_COLOR: u32 = 0xC800_0000;

/// UTF-16 for GDI, truncated to `max_units` code units like the Zig build's fixed text buffers.
fn to_wide_truncated(text: &str, max_units: usize) -> Vec<u16> {
    let mut w: Vec<u16> = text.encode_utf16().take(max_units).collect();
    // Don't leave half a surrogate pair at the cut.
    if w.last().is_some_and(|&u| (0xD800..0xDC00).contains(&u)) {
        w.pop();
    }
    w
}

/// Measures `text` (truncated to `buf_size - 1` UTF-16 units) using the currently selected font.
pub fn measure_text_size(buf_size: usize, dc: HDC, text: &str) -> SIZE {
    let w = to_wide_truncated(text, buf_size.saturating_sub(1));
    let mut sz = SIZE { cx: 0, cy: 0 };
    unsafe { GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut sz) };
    sz
}

/// Measures the pixel width of `text` using the currently selected font.
pub fn measure_text_width(buf_size: usize, dc: HDC, text: &str) -> usize {
    measure_text_size(buf_size, dc, text).cx.max(0) as usize
}

/// Draws `text` with a transparent background in the currently selected font; `color` is 0xAARRGGBB.
pub fn draw_text(buf_size: usize, dc: HDC, x: i32, y: i32, text: &str, color: u32) {
    let w = to_wide_truncated(text, buf_size.saturating_sub(1));
    unsafe {
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, to_color_ref(color));
        TextOutW(dc, x, y, w.as_ptr(), w.len() as i32);
    }
}

/// Corners are cut with per-row insets (no anti-aliasing), which is fine at the small radii UI buttons use.
#[allow(clippy::too_many_arguments)]
pub fn fill_rounded_rect(pixels: &mut [u32], stride: usize, height: usize, x: usize, y: usize, w: usize, h: usize, radius: usize, argb: u32) {
    let r = radius.min(w.min(h) / 2);
    let radius_f = r as f32;
    for row in 0..h {
        let from_edge = row.min(h - 1 - row);
        let mut inset = 0usize;
        if from_edge < r {
            let dy = radius_f - from_edge as f32 - 0.5;
            inset = (radius_f - (radius_f * radius_f - dy * dy).sqrt()).round() as usize;
        }
        fill_rect(pixels, stride, height, x + inset, y + row, w.saturating_sub(2 * inset), 1, argb);
    }
}

/// None if `name` isn't installed, since CreateFont silently substitutes another face. Matches by prefix because GDI can report a weight variant of a variable font (e.g. "Cascadia Code SemiBold") as the face.
pub fn create_installed_font(dc: HDC, name: &str, height_px: i32, weight: i32) -> Option<HFONT> {
    let name_w = wide(name);
    unsafe {
        let font = CreateFontW(
            -height_px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, name_w.as_ptr(),
        );
        if font.is_null() {
            return None;
        }
        let old_font = SelectObject(dc, font);
        let mut face = [0u16; 64];
        let face_len = GetTextFaceW(dc, face.len() as i32, face.as_mut_ptr());
        if !old_font.is_null() {
            SelectObject(dc, old_font);
        }
        if face_len > 1 {
            let face = String::from_utf16_lossy(&face[..(face_len - 1) as usize]);
            if face.len() >= name.len() && face.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes()) {
                return Some(font);
            }
        }
        DeleteObject(font);
        None
    }
}

pub struct ButtonFace {
    pub fill: u32,
    pub border: u32,
    pub text_color: u32,
    pub radius: usize,
    pub border_px: usize,
}

/// A filled, bordered, rounded button with `label` centered in `font`; `rect` is in bitmap coordinates.
pub fn draw_button_face(bmp: &mut OverlayBitmap, rect: RECT, label: &str, font: HFONT, face: &ButtonFace) {
    let x = rect.left.max(0) as usize;
    let y = rect.top.max(0) as usize;
    let w = rect_width(rect).max(0) as usize;
    let h = rect_height(rect).max(0) as usize;
    let (bw, bh) = (bmp.width, bmp.height);
    fill_rounded_rect(bmp.pixels(), bw, bh, x, y, w, h, face.radius, face.border);
    fill_rounded_rect(
        bmp.pixels(), bw, bh, x + face.border_px, y + face.border_px,
        w.saturating_sub(2 * face.border_px), h.saturating_sub(2 * face.border_px),
        face.radius.saturating_sub(face.border_px), face.fill,
    );

    const LABEL_BUF_SIZE: usize = 32;
    let old_font = unsafe { SelectObject(bmp.mem_dc, font) };
    let text_size = measure_text_size(LABEL_BUF_SIZE, bmp.mem_dc, label);
    let text_w = text_size.cx.max(0) as usize;
    let text_h = text_size.cy.max(0) as usize;
    let text_x = (x + w.saturating_sub(text_w) / 2) as i32;
    let text_y = (y + h.saturating_sub(text_h) / 2) as i32;
    draw_text(LABEL_BUF_SIZE, bmp.mem_dc, text_x, text_y, label, face.text_color);
    fix_text_alpha_rect(bmp.pixels(), bw, bh, text_x, text_y, text_w, text_h);
    if !old_font.is_null() {
        unsafe { SelectObject(bmp.mem_dc, old_font) };
    }
}

/// Draws a `thickness`-px outline of a rect placed anywhere inside a `buf_width`x`buf_height` pixel buffer, clamped to the buffer bounds.
#[allow(clippy::too_many_arguments)]
pub fn draw_rect_outline(pixels: &mut [u32], buf_width: usize, buf_height: usize, x: i32, y: i32, w: usize, h: usize, thickness: usize, color: u32) {
    let left = x.clamp(0, buf_width as i32) as usize;
    let top = y.clamp(0, buf_height as i32) as usize;
    let right = buf_width.min(left + w);
    let bottom = buf_height.min(top + h);
    if right <= left || bottom <= top {
        return;
    }
    let t = thickness.min((right - left).min(bottom - top));
    fill_rect(pixels, buf_width, buf_height, left, top, right - left, t, color);
    fill_rect(pixels, buf_width, buf_height, left, bottom - t, right - left, t, color);
    fill_rect(pixels, buf_width, buf_height, left, top, t, bottom - top, color);
    fill_rect(pixels, buf_width, buf_height, right - t, top, t, bottom - top, color);
}

/// Pushes the bitmap to a layered window at its origin using per-pixel alpha, scaled by `opacity`.
pub fn present_layered(hwnd: HWND, bmp: &OverlayBitmap, opacity: u8) {
    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            SLOG.err(format_args!("Failed to get screen DC to present layered overlay"));
            return;
        }
        let window_size = SIZE { cx: bmp.width as i32, cy: bmp.height as i32 };
        let source_pos = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: opacity, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(hwnd, screen_dc, std::ptr::null(), &window_size, bmp.mem_dc, &source_pos, 0, &blend, ULW_ALPHA);
        ReleaseDC(std::ptr::null_mut(), screen_dc);
    }
}

/// Longest prefix of `text` (plus "...") that fits within `max_w` pixels measured on `dc`; the text as-is (no ellipsis) if it already fits.
pub fn truncate_text_to_fit(buf_size: usize, dc: HDC, text: &str, max_w: usize) -> String {
    // -4 leaves room for the "..." suffix.
    let units = to_wide_truncated(text, buf_size.saturating_sub(4));
    let extent = |s: &[u16]| {
        let mut sz = SIZE { cx: 0, cy: 0 };
        unsafe { GetTextExtentPoint32W(dc, s.as_ptr(), s.len() as i32, &mut sz) };
        sz.cx
    };
    if extent(&units).max(0) as usize <= max_w {
        return String::from_utf16_lossy(&units);
    }

    let ellipsis: Vec<u16> = "...".encode_utf16().collect();
    let budget = max_w as i32 - extent(&ellipsis);
    if budget <= 0 {
        return String::new();
    }

    let (mut lo, mut hi) = (0usize, units.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if extent(&units[..mid]) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out = String::from_utf16_lossy(&units[..lo]);
    out.push_str("...");
    out
}

/// `background` may be null for a layered/owner-drawn window that paints its own background.
pub fn register_window_class(
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    wnd_proc: WNDPROC,
    class_name: &str,
    background: HBRUSH,
) -> bool {
    let class_w = wide(class_name);
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: wnd_proc,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: background,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_w.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc) != 0
    }
}

const HTCAPTION_RESULT: LRESULT = HTCAPTION as LRESULT;
const HTCLIENT_RESULT: LRESULT = HTCLIENT as LRESULT;

thread_local! {
    // Anchors a drag to the cursor position at WM_ENTERSIZEMOVE, since WM_MOVING's rect reflects prior snap overrides; a single shared pair is safe since only one window can be mid-drag at a time.
    static PANEL_DRAG_ANCHOR: Cell<(POINT, RECT)> = const {
        Cell::new((POINT { x: 0, y: 0 }, RECT { left: 0, top: 0, right: 0, bottom: 0 }))
    };
}

/// WM_NCHITTEST for a panel whose header (the top `header_height` px) is its only drag handle.
pub fn panel_header_hit_test(hwnd: HWND, lparam: LPARAM, header_height: i32, dragging_enabled: bool) -> LRESULT {
    let sy = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
    let mut wr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetWindowRect(hwnd, &mut wr) };
    if dragging_enabled && sy - wr.top < header_height { HTCAPTION_RESULT } else { HTCLIENT_RESULT }
}

/// Call from WM_ENTERSIZEMOVE before any other drag-start handling.
pub fn begin_panel_drag(hwnd: HWND) {
    let mut cursor = POINT { x: 0, y: 0 };
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe {
        GetCursorPos(&mut cursor);
        GetWindowRect(hwnd, &mut rect);
    }
    PANEL_DRAG_ANCHOR.with(|a| a.set((cursor, rect)));
}

/// Call from WM_MOVING to recompute the truly-intended position from the absolute cursor delta since drag start (ignoring Windows' possibly already-snapped `rect`) and snap it via input::apply_snapping.
pub fn update_panel_drag_rect(hwnd: HWND, rect: &mut RECT) {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let (anchor_cursor, anchor_rect) = PANEL_DRAG_ANCHOR.with(|a| a.get());
    let mut cursor = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut cursor) };
    let intended_x = anchor_rect.left + (cursor.x - anchor_cursor.x);
    let intended_y = anchor_rect.top + (cursor.y - anchor_cursor.y);

    let snapped = crate::input::apply_snapping(intended_x, intended_y, width, height, hwnd);
    *rect = RECT { left: snapped.x, top: snapped.y, right: snapped.x + width, bottom: snapped.y + height };
}

/// Creates a GDI font for `name` at `size` px; null on failure.
pub fn create_font(name: &str, size: i32, weight: FontWeight) -> HFONT {
    let name_w = wide(name);
    unsafe {
        CreateFontW(
            -size, 0, 0, 0, weight.to_win32_weight(), weight.is_italic() as u32, 0, 0, DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32,
            name_w.as_ptr(),
        )
    }
}

/// A single cached font for callers that only ever need one at a time (the painter keeps its own per-slot/DPI cache).
#[derive(Default)]
pub struct CachedFont {
    font: Option<HFONT>,
    name: String,
    size: i32,
    weight: Option<FontWeight>,
}

impl CachedFont {
    /// The font for these settings, recreating it only when they changed; None if GDI couldn't create it.
    pub fn ensure(&mut self, context: &str, name: &str, size: i32, weight: FontWeight) -> Option<HFONT> {
        let unchanged = self.font.is_some() && self.name == name && self.size == size && self.weight == Some(weight);
        if !unchanged {
            self.release();
            let font = create_font(name, size, weight);
            if font.is_null() {
                SLOG.err(format_args!("Failed to create {context} font '{name}'"));
            } else {
                self.font = Some(font);
            }
            self.name = name.to_owned();
            self.size = size;
            self.weight = Some(weight);
        }
        self.font
    }

    fn release(&mut self) {
        if let Some(old) = self.font.take() {
            unsafe { DeleteObject(old) };
        }
    }
}

impl Drop for CachedFont {
    fn drop(&mut self) {
        self.release();
    }
}
