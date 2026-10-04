//! Transient drag-time overlays: ghost outlines of other characters' saved positions, and the centered hint box
//! (also reused above the region-select overlay).

use eve_maj_core::color::with_alpha;
use eve_maj_win::geometry::scale_pixels;
use eve_maj_win::wide;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::layout::{cursor_monitor_bounds, default_dpi, dpi_to_scale, is_region_fit_active, region_rect_from_config, calculate_region_fit_grid, window_dpi};
use super::render::{measure_text, render_text, TEXT_BUFFER_SIZE};
use super::*;
use crate::gdi_overlay::{self, OverlayBitmap};

/// One saved-position outline for the drag-time ghost overlay; `names` is the comma-joined list of every character sharing that exact rect.
#[derive(Clone)]
pub struct GhostGroup {
    pub rect: RECT,
    pub names: String,
}

fn rects_equal(a: RECT, b: RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

fn create_overlay_window(class: &str, x: i32, y: i32, width: i32, height: i32, instance: windows_sys::Win32::Foundation::HINSTANCE) -> HWND {
    let class_w = wide(class);
    let empty = [0u16];
    unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            class_w.as_ptr(), empty.as_ptr(), WS_POPUP, x, y, width, height,
            std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null(),
        )
    }
}

impl Painter {
    /// Saved positions for every other character in the profile, grouped by exact rect match (identical x/y/w/h counts as "stacked").
    pub fn collect_ghost_groups(&self, exclude_character: &str) -> Vec<GhostGroup> {
        let config = globals::config();
        let cfg = &config.display;
        let region_fit_grid = is_region_fit_active(cfg).then(|| {
            let region = region_rect_from_config(cfg).expect("checked by is_region_fit_active");
            calculate_region_fit_grid(region, self.region_fit_grid_count(), cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region))
        });

        let raw: Vec<(&str, RECT)> = config
            .characters
            .iter()
            .filter(|c| c.name != exclude_character)
            .filter_map(|c| {
                let pos = c.position?;
                let (w, h) = self.thumbnail_size(&c.name, self.thumbnails.len(), region_fit_grid);
                Some((c.name.as_str(), RECT { left: pos.x, top: pos.y, right: pos.x + w, bottom: pos.y + h }))
            })
            .collect();

        let mut used = vec![false; raw.len()];
        let mut groups = Vec::new();
        for (i, &(name, rect)) in raw.iter().enumerate() {
            if used[i] {
                continue;
            }
            used[i] = true;
            let mut names = name.to_owned();
            for (j, &(other_name, other_rect)) in raw.iter().enumerate().skip(i + 1) {
                if used[j] || !rects_equal(rect, other_rect) {
                    continue;
                }
                used[j] = true;
                names.push_str(", ");
                names.push_str(other_name);
            }
            groups.push(GhostGroup { rect, names });
        }
        groups
    }

    /// Shows (creating on first use) a topmost, click-through overlay outlining every other saved position in the profile; called once when a drag starts. Ghosts are static for the duration of the drag, so the computed groups are cached for input's apply_ghost_snapping to reuse (see current_drag_ghost_groups).
    pub fn show_ghost_overlay(&mut self, exclude_character: &str) {
        self.current_drag_ghost_groups = None;
        let groups = self.collect_ghost_groups(exclude_character);
        if groups.is_empty() {
            self.current_drag_ghost_groups = Some(groups);
            self.hide_ghost_overlay_window();
            return;
        }

        let mut bounds = groups[0].rect;
        for g in &groups[1..] {
            bounds.left = bounds.left.min(g.rect.left);
            bounds.top = bounds.top.min(g.rect.top);
            bounds.right = bounds.right.max(g.rect.right);
            bounds.bottom = bounds.bottom.max(g.rect.bottom);
        }
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        if width <= 0 || height <= 0 {
            self.current_drag_ghost_groups = Some(groups);
            self.hide_ghost_overlay_window();
            return;
        }

        let hwnd = match self.ghost_overlay_hwnd {
            Some(hwnd) => {
                unsafe { SetWindowPos(hwnd, HWND_TOPMOST, bounds.left, bounds.top, width, height, SWP_NOACTIVATE) };
                hwnd
            }
            None => {
                let hwnd = create_overlay_window(GHOST_WINDOW_CLASS_NAME, bounds.left, bounds.top, width, height, self.instance);
                if hwnd.is_null() {
                    SLOG.err(format_args!("Failed to create ghost overlay window"));
                    self.current_drag_ghost_groups = Some(groups);
                    return;
                }
                self.ghost_overlay_hwnd = Some(hwnd);
                hwnd
            }
        };

        if OverlayBitmap::needs_resize(&self.ghost_overlay_bitmap, width, height) {
            let init_dc = unsafe { GetDC(std::ptr::null_mut()) };
            if init_dc.is_null() {
                self.current_drag_ghost_groups = Some(groups);
                return;
            }
            let ok = OverlayBitmap::recreate(&mut self.ghost_overlay_bitmap, init_dc, width, height);
            unsafe { ReleaseDC(std::ptr::null_mut(), init_dc) };
            if !ok {
                SLOG.err(format_args!("Failed to allocate ghost overlay bitmap"));
                self.current_drag_ghost_groups = Some(groups);
                return;
            }
        }

        let ghost_dpi = window_dpi(hwnd);
        let t = &globals::config().thumbnail;
        let font = match self.cached_font(FontSlot::Main, ghost_dpi, &t.character_name_font_name, scale_pixels(t.character_name_font_size, dpi_to_scale(ghost_dpi)), t.character_name_font_weight) {
            Ok(f) => f,
            Err(err) => {
                SLOG.err(format_args!("Failed to get font for ghost overlay: {err}"));
                self.current_drag_ghost_groups = Some(groups);
                return;
            }
        };

        // Same hue as the focused/active thumbnail border, at reduced alpha so it still reads as a ghost rather than a real thumbnail.
        let outline_color = with_alpha(t.border_color, 0xB0);
        let text_color = 0xE0FF_FFFF;

        let overlay = self.ghost_overlay_bitmap.as_mut().expect("allocated above");
        let (ow, oh, dc) = (overlay.width, overlay.height, overlay.mem_dc);
        overlay.pixels().fill(0);
        let old_font = unsafe { SelectObject(dc, font) };
        for g in &groups {
            let local_x = g.rect.left - bounds.left;
            let local_y = g.rect.top - bounds.top;
            let rect_w = (g.rect.right - g.rect.left).max(0) as usize;
            let rect_h = (g.rect.bottom - g.rect.top).max(0) as usize;
            gdi_overlay::draw_rect_outline(overlay.pixels(), ow, oh, local_x, local_y, rect_w, rect_h, 2, outline_color);
            let dims = measure_text(dc, &g.names);
            render_text(dc, &g.names, local_x, local_y, text_color);
            gdi_overlay::fix_text_alpha_rect(overlay.pixels(), ow, oh, local_x, local_y, dims.width, dims.height);
        }
        if !old_font.is_null() {
            unsafe { SelectObject(dc, old_font) };
        }

        gdi_overlay::present_layered(hwnd, overlay, 255);
        unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        self.current_drag_ghost_groups = Some(groups);
    }

    fn hide_ghost_overlay_window(&mut self) {
        if let Some(hwnd) = self.ghost_overlay_hwnd {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    }

    pub fn hide_ghost_overlay(&mut self) {
        self.current_drag_ghost_groups = None;
        self.hide_ghost_overlay_window();
    }

    /// Shows (creating on first use) a topmost, click-through hint box centered on the monitor nearest `dragging_hwnd`; called once when a drag starts. Static for the duration of the drag.
    pub fn show_drag_hint_overlay(&mut self, dragging_hwnd: HWND) {
        let nearest = Painter::nearest_monitor_bounds(dragging_hwnd);
        self.show_hint_box("Hold Ctrl to move all thumbnails together", "Turn off dragging from the tray icon or settings", nearest.bounds, nearest.monitor);
    }

    /// Hint box centered on the monitor under the cursor, shown above the region-select overlay.
    #[allow(dead_code)]
    pub fn show_region_select_hint(&mut self, line1: &str, line2: &str) {
        let nearest = cursor_monitor_bounds();
        self.show_hint_box(line1, line2, nearest.bounds, nearest.monitor);
    }

    pub(super) fn character_name_font(&mut self, monitor: Option<HMONITOR>) -> Result<HFONT, RenderError> {
        let dpi = monitor.map_or_else(default_dpi, eve_maj_win::geometry::monitor_dpi);
        let t = &globals::config().thumbnail;
        self.cached_font(FontSlot::Main, dpi, &t.character_name_font_name, scale_pixels(t.character_name_font_size, dpi_to_scale(dpi)), t.character_name_font_weight)
    }

    fn show_hint_box(&mut self, line1: &str, line2: &str, bounds: RECT, monitor: Option<HMONITOR>) {
        let font = match self.character_name_font(monitor) {
            Ok(f) => f,
            Err(err) => {
                SLOG.err(format_args!("Failed to get font for hint overlay: {err}"));
                return;
            }
        };

        let init_dc = unsafe { GetDC(std::ptr::null_mut()) };
        if init_dc.is_null() {
            return;
        }
        let old_measure_font = unsafe { SelectObject(init_dc, font) };
        let dims1 = measure_text(init_dc, line1);
        let dims2 = measure_text(init_dc, line2);
        if !old_measure_font.is_null() {
            unsafe { SelectObject(init_dc, old_measure_font) };
        }

        const LINE_GAP: usize = 4;
        const BOX_PADDING: usize = 10;
        let width = (dims1.width.max(dims2.width) + BOX_PADDING * 2) as i32;
        let height = (dims1.height + dims2.height + LINE_GAP + BOX_PADDING * 2) as i32;
        let x = bounds.left + ((bounds.right - bounds.left) - width) / 2;
        let y = bounds.top + ((bounds.bottom - bounds.top) - height) / 2;

        let hwnd = match self.drag_hint_hwnd {
            Some(hwnd) => {
                unsafe { SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE) };
                hwnd
            }
            None => {
                let hwnd = create_overlay_window(GHOST_WINDOW_CLASS_NAME, x, y, width, height, self.instance);
                if hwnd.is_null() {
                    SLOG.err(format_args!("Failed to create drag hint overlay window"));
                    unsafe { ReleaseDC(std::ptr::null_mut(), init_dc) };
                    return;
                }
                self.drag_hint_hwnd = Some(hwnd);
                hwnd
            }
        };

        if OverlayBitmap::needs_resize(&self.drag_hint_bitmap, width, height) && !OverlayBitmap::recreate(&mut self.drag_hint_bitmap, init_dc, width, height) {
            SLOG.err(format_args!("Failed to allocate drag hint overlay bitmap"));
            unsafe { ReleaseDC(std::ptr::null_mut(), init_dc) };
            return;
        }
        unsafe { ReleaseDC(std::ptr::null_mut(), init_dc) };

        let text_color = globals::config().thumbnail.character_name_color | 0xFF00_0000;
        let overlay = self.drag_hint_bitmap.as_mut().expect("allocated above");
        let (ow, oh, dc) = (overlay.width, overlay.height, overlay.mem_dc);
        overlay.pixels().fill(0);
        gdi_overlay::fill_rect(overlay.pixels(), ow, oh, 0, 0, ow, oh, gdi_overlay::HINT_BG_COLOR);

        let old_font = unsafe { SelectObject(dc, font) };
        render_text(dc, line1, BOX_PADDING as i32, BOX_PADDING as i32, text_color);
        gdi_overlay::fix_text_alpha_rect(overlay.pixels(), ow, oh, BOX_PADDING as i32, BOX_PADDING as i32, dims1.width, dims1.height);
        let line2_y = (BOX_PADDING + dims1.height + LINE_GAP) as i32;
        render_text(dc, line2, BOX_PADDING as i32, line2_y, text_color);
        gdi_overlay::fix_text_alpha_rect(overlay.pixels(), ow, oh, BOX_PADDING as i32, line2_y, dims2.width, dims2.height);
        if !old_font.is_null() {
            unsafe { SelectObject(dc, old_font) };
        }

        gdi_overlay::present_layered(hwnd, overlay, 255);
        unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        let _ = TEXT_BUFFER_SIZE;
    }

    pub fn hide_drag_hint_overlay(&mut self) {
        if let Some(hwnd) = self.drag_hint_hwnd {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    }
}
