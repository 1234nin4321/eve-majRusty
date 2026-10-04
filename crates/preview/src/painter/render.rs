//! Per-thumbnail overlay rendering: RenderSettings (the resolved look of one thumbnail right now), the GDI text
//! overlay painter, and the pixel-level border/exclusion/background drawing helpers.

use eve_maj_core::color::with_alpha;
use eve_maj_core::config::{Config, StateVisualConfig};
use eve_maj_core::types::{BorderStyle, ExclusionOverlayStyle, FontWeight, TextPosition};
use eve_maj_core::config::IskRateUnit;
use eve_maj_win::geometry::scale_pixels;
use eve_maj_win::time::Ticks;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

use super::*;
use crate::gdi_overlay::{self, OverlayBitmap};

pub(super) const TEXT_BUFFER_SIZE: usize = 256;
const TEXT_PADDING_X: usize = 5;
const TEXT_PADDING_Y: usize = 2;
pub(super) const OVERLAY_ALPHA: u8 = 255;

/// One resolved (text, color) line of the stacked notification block; built by create_render_settings, drawn by render_thumbnail_overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationLine {
    pub text: String,
    pub color: u32,
}

/// Settings for rendering thumbnail overlays (text and border); compared field-for-field to skip redundant redraws.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSettings {
    pub show_text: bool,
    pub show_character_name: bool,
    pub character_name: String,
    pub show_system_name: bool,
    pub system_name: String,
    pub character_name_color: u32,
    pub system_name_color: u32,
    pub character_name_bg_color: u32,
    pub system_name_bg_color: u32,
    pub character_name_font_name: String,
    pub character_name_font_size: i32,
    pub character_name_font_weight: FontWeight,
    pub character_name_position: TextPosition,
    pub character_name_offset_x: i32,
    pub character_name_offset_y: i32,
    pub system_name_position: TextPosition,
    pub system_name_offset_x: i32,
    pub system_name_offset_y: i32,
    pub system_name_font_name: String,
    pub system_name_font_size: i32,
    pub system_name_font_weight: FontWeight,
    pub show_notifications: bool,
    pub notification_lines: Vec<NotificationLine>,
    pub notifications_position: TextPosition,
    pub notifications_offset_x: i32,
    pub notifications_offset_y: i32,
    pub notifications_font_name: String,
    pub notifications_font_size: i32,
    pub notifications_font_weight: FontWeight,
    pub notifications_bg_color: u32,

    pub show_border: bool,
    pub border_width: u8,
    pub border_color: u32,
    pub border_style: BorderStyle,

    pub show_exclusion_overlay: bool,
    pub exclusion_overlay_style: ExclusionOverlayStyle,
    pub exclusion_overlay_color: u32,

    pub show_group_badge: bool,
    pub group_badge_text: String,
    pub group_badge_color: u32,
    pub group_badge_position: TextPosition,
    pub group_badge_offset_x: i32,
    pub group_badge_offset_y: i32,
    pub group_badge_font_name: String,
    pub group_badge_font_size: i32,
    pub group_badge_font_weight: FontWeight,
    pub group_badge_bg_color: u32,
    pub combat_incoming_bg_color: u32,
    pub combat_outgoing_bg_color: u32,
    pub mining_bg_color: u32,
    pub bounty_bg_color: u32,
    pub resources_bg_color: u32,

    pub show_thumbnail: bool,
    pub overlay_alpha: u8,

    pub overlay_width: i32,
    pub overlay_height: i32,

    pub dps_incoming: f32,
    pub dps_outgoing: f32,
    pub mining_rate: f32,
    pub mining_isk_rate: f32,
    pub bounty_isk_rate: f32,
    pub resource_cpu_percent: f32,
    pub resource_ram_mb: f32,
    pub resource_vram_mb: f32,
    // Included so the false->true transition on first tracker push invalidates the cache even when the (sentineled) rate value itself didn't change.
    pub has_dps_data: bool,
    pub has_mining_data: bool,
    pub has_bounty_data: bool,
    pub has_resource_data: bool,
    pub has_vram_data: bool,

    // Included so a config-only color change still invalidates render_thumbnail's cache even when the DPS/rate value itself hasn't moved.
    pub dps_incoming_color: u32,
    pub dps_outgoing_color: u32,
    pub mining_color: u32,
    pub bounty_color: u32,
    pub resources_color: u32,
}

impl RenderSettings {
    /// Every visual field equal; show_thumbnail is deliberately ignored (see only_visibility_changed).
    pub(super) fn visual_eq(&self, other: &RenderSettings) -> bool {
        self.show_thumbnail == other.show_thumbnail && self == other
            || self.show_thumbnail != other.show_thumbnail && {
                let mut a = self.clone();
                a.show_thumbnail = other.show_thumbnail;
                &a == other
            }
    }

    pub(super) fn only_visibility_changed(&self, other: &RenderSettings) -> bool {
        self.show_thumbnail != other.show_thumbnail && self.visual_eq(other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextDimensions {
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct TextPos {
    x: i32,
    y: i32,
}

/// One paintable text run inside the thumbnail overlay. render_pos is where its glyphs are drawn; bg_pos/bg_dims bound
/// its background-fill and alpha-fixup rect. These differ for stacked mining/resources lines, which share one
/// block-wide background but render at their own individually-aligned x.
struct DrawLine {
    font: HFONT,
    text: String,
    render_pos: TextPos,
    bg_pos: TextPos,
    bg_dims: TextDimensions,
    color: u32,
    bg_color: u32,
}

#[derive(Clone, Copy)]
enum HorizontalAlign {
    Left,
    Center,
    Right,
}

fn horizontal_align_of(position: TextPosition) -> HorizontalAlign {
    use TextPosition::*;
    match position {
        TopLeft | LeftCenter | BottomLeft => HorizontalAlign::Left,
        TopCenter | Center | BottomCenter => HorizontalAlign::Center,
        TopRight | RightCenter | BottomRight => HorizontalAlign::Right,
    }
}

/// x for a line of `line_width` so it sits flush against whichever edge `alignment` anchors to, within a block of `block_width` starting at `block_x`.
fn aligned_line_x(block_x: i32, block_width: usize, line_width: usize, alignment: HorizontalAlign) -> i32 {
    match alignment {
        HorizontalAlign::Left => block_x,
        HorizontalAlign::Center => block_x + (block_width.saturating_sub(line_width) / 2) as i32,
        HorizontalAlign::Right => block_x + block_width.saturating_sub(line_width) as i32,
    }
}

#[derive(Clone, Copy)]
enum VerticalAlign {
    Top,
    Middle,
    Bottom,
}

fn vertical_align_of(position: TextPosition) -> VerticalAlign {
    use TextPosition::*;
    match position {
        TopLeft | TopCenter | TopRight => VerticalAlign::Top,
        LeftCenter | Center | RightCenter => VerticalAlign::Middle,
        BottomLeft | BottomCenter | BottomRight => VerticalAlign::Bottom,
    }
}

/// y for a line of `line_height` so it sits flush against whichever edge `alignment` anchors to; same shape as aligned_line_x for the vertical axis.
fn aligned_line_y(block_y: i32, block_height: usize, line_height: usize, alignment: VerticalAlign) -> i32 {
    match alignment {
        VerticalAlign::Top => block_y,
        VerticalAlign::Middle => block_y + (block_height.saturating_sub(line_height) / 2) as i32,
        VerticalAlign::Bottom => block_y + block_height.saturating_sub(line_height) as i32,
    }
}

fn calculate_text_position(position: TextPosition, dims: TextDimensions, overlay_width: usize, overlay_height: usize, offset_x: i32, offset_y: i32) -> TextPos {
    let mut x = aligned_line_x(0, overlay_width, dims.width, horizontal_align_of(position)) + offset_x;
    let mut y = aligned_line_y(0, overlay_height, dims.height, vertical_align_of(position)) + offset_y;
    x = x.min(overlay_width as i32 - dims.width as i32).max(0);
    y = y.min(overlay_height as i32 - dims.height as i32).max(0);
    TextPos { x, y }
}

/// Fills a rectangular region of pixels with a single colour.
#[allow(clippy::too_many_arguments)]
fn fill_text_background(pixels: &mut [u32], width: usize, height: usize, x: i32, y: i32, text_width: usize, bar_height: usize, color: u32) {
    let start_x = x.max(0) as usize;
    let start_y = y.max(0) as usize;
    let end_y = (start_y + bar_height).min(height);
    let end_x = (start_x + text_width).min(width);
    if end_x <= start_x || end_y <= start_y {
        return;
    }
    // Must be premultiplied, or fix_text_alpha_rect's "alpha==0 but rgb!=0" heuristic mistakes a
    // transparent non-black background for unfixed GDI text and forces it fully opaque.
    let blended = premultiply_alpha(color);
    for py in start_y..end_y {
        pixels[py * width + start_x..py * width + end_x].fill(blended);
    }
}

/// Pre-multiplies color by alpha, valid only when blending onto an already-transparent buffer.
fn premultiply_alpha(color: u32) -> u32 {
    let a = (color >> 24) & 0xFF;
    if a == 255 {
        return color;
    }
    let r = ((color >> 16) & 0xFF) * a / 255;
    let g = ((color >> 8) & 0xFF) * a / 255;
    let b = (color & 0xFF) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Draws one diagonal band; is_diag2 selects top-right→bottom-left over top-left→bottom-right. Shared by X (both bands) and DiagonalSlash (diag2 only).
fn draw_diagonal_band(pixels: &mut [u32], width: usize, height: usize, color: u32, is_diag2: bool) {
    let iw = width as i32;
    let ih = height as i32;
    if ih == 0 {
        return;
    }
    // Line half-width in pixels, scaled proportionally with the aspect ratio.
    let half = (5 * iw / ih).max(1);
    for y in 0..height {
        let iy = y as i32;
        let cx = if is_diag2 { (ih - iy) * iw / ih } else { iy * iw / ih };
        let lo = (cx - half).max(0) as usize;
        let hi = (cx + half + 1).min(iw).max(0) as usize;
        if lo < hi {
            pixels[y * width + lo..y * width + hi].fill(color);
        }
    }
}

/// Draws the exclusion overlay onto an already-cleared buffer.
pub(super) fn draw_exclusion_overlay(pixels: &mut [u32], width: usize, height: usize, color: u32, style: ExclusionOverlayStyle) {
    if (color >> 24) & 0xFF == 0 {
        return;
    }
    let blended = premultiply_alpha(color);
    match style {
        ExclusionOverlayStyle::None => {}
        ExclusionOverlayStyle::SolidTint => pixels[..width * height].fill(blended),
        ExclusionOverlayStyle::CircleSlash => {
            let cx = width as f32 / 2.0;
            let cy = height as f32 / 2.0;
            let radius = cx.min(cy) * 0.7;
            let thickness = (radius * 0.18).max(2.0);
            // Slash extends a bit past the ring, matching the standard "no entry" glyph.
            let slash_reach = radius * 1.15;
            let sqrt2 = 2f32.sqrt();
            for y in 0..height {
                let dy = y as f32 - cy;
                for x in 0..width {
                    let dx = x as f32 - cx;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let on_ring = (dist - radius).abs() <= thickness / 2.0;
                    // Slash direction runs lower-left to upper-right (dx + dy = 0 through centre).
                    let on_slash = (dx + dy).abs() / sqrt2 <= thickness / 2.0 && dist <= slash_reach;
                    if on_ring || on_slash {
                        pixels[y * width + x] = blended;
                    }
                }
            }
        }
        ExclusionOverlayStyle::DiagonalHatch => {
            // Same ratio as BorderStyle::DiagonalHatch, but filling the whole area.
            for y in 0..height {
                for x in 0..width {
                    if (x + y) % 6 < 3 {
                        pixels[y * width + x] = blended;
                    }
                }
            }
        }
        ExclusionOverlayStyle::Checkerboard => {
            const SQUARE_SIZE: usize = 8;
            for y in 0..height {
                for x in 0..width {
                    if ((x / SQUARE_SIZE) + (y / SQUARE_SIZE)).is_multiple_of(2) {
                        pixels[y * width + x] = blended;
                    }
                }
            }
        }
        ExclusionOverlayStyle::X => {
            draw_diagonal_band(pixels, width, height, blended, false);
            draw_diagonal_band(pixels, width, height, blended, true);
        }
        ExclusionOverlayStyle::DiagonalSlash => draw_diagonal_band(pixels, width, height, blended, true),
    }
}

struct BorderRegion {
    x_start: usize,
    y_start: usize,
    x_end: usize,
    y_end: usize,
}

/// The four border bands (top/bottom/left/right), each `border_width` thick and running the full length of its edge.
fn border_regions(width: usize, height: usize, bw: usize) -> [BorderRegion; 4] {
    [
        BorderRegion { x_start: 0, y_start: 0, x_end: width, y_end: bw },
        BorderRegion { x_start: 0, y_start: height - bw, x_end: width, y_end: height },
        BorderRegion { x_start: 0, y_start: 0, x_end: bw, y_end: height },
        BorderRegion { x_start: width - bw, y_start: 0, x_end: width, y_end: height },
    ]
}

/// Marks pixels along the border's length using a repeating mark/gap pattern, where `pos` runs along the edge; shared by Dashed and Dotted.
#[allow(clippy::too_many_arguments)]
fn draw_lengthwise_pattern(pixels: &mut [u32], width: usize, height: usize, bw: usize, color: u32, mark_length: usize, gap_length: usize) {
    let pattern_length = mark_length + gap_length;
    if pattern_length == 0 {
        return;
    }
    for region in border_regions(width, height, bw) {
        let is_horizontal = region.x_end - region.x_start == width;
        for y in region.y_start..region.y_end {
            for x in region.x_start..region.x_end {
                let pos = if is_horizontal { x } else { y };
                if pos % pattern_length < mark_length {
                    pixels[y * width + x] = color;
                }
            }
        }
    }
}

pub(super) fn draw_border(pixels: &mut [u32], width: usize, height: usize, bw: usize, color: u32, style: BorderStyle) {
    // A border thicker than the overlay itself would index past the buffer below.
    if bw == 0 || bw > width || bw > height {
        return;
    }
    match style {
        BorderStyle::Solid => {
            // Top and bottom bands — each is a contiguous run of (bw * width) pixels.
            pixels[..bw * width].fill(color);
            pixels[(height - bw) * width..height * width].fill(color);
            // Left and right strips for the middle rows (corners already covered above).
            for y in bw..height - bw {
                let row = y * width;
                pixels[row..row + bw].fill(color);
                pixels[row + width - bw..row + width].fill(color);
            }
        }
        BorderStyle::Dashed => draw_lengthwise_pattern(pixels, width, height, bw, color, 8, 4),
        BorderStyle::Dotted => {
            // Square-ish dots roughly one border-width wide, spaced two border-widths apart, distinct from Dashed's fixed 8px marks.
            let dot = bw.max(1);
            draw_lengthwise_pattern(pixels, width, height, bw, color, dot, dot * 2);
        }
        BorderStyle::Double => {
            // Mirrors the CSS "double" border look; at very thin widths the lines abut with no visible gap and just render as solid.
            let line = (bw / 3).max(1);
            let inner_start = bw - line;
            pixels[..line * width].fill(color);
            pixels[inner_start * width..bw * width].fill(color);
            pixels[(height - line) * width..height * width].fill(color);
            pixels[(height - bw) * width..(height - bw + line) * width].fill(color);
            for y in bw..height - bw {
                let row = y * width;
                pixels[row..row + line].fill(color);
                pixels[row + inner_start..row + bw].fill(color);
                pixels[row + width - bw..row + width - bw + line].fill(color);
                pixels[row + width - line..row + width].fill(color);
            }
        }
        BorderStyle::DiagonalHatch => {
            // Fixed mark/gap ratio regardless of border width so the 45-degree hatch angle stays consistent.
            for region in border_regions(width, height, bw) {
                for y in region.y_start..region.y_end {
                    for x in region.x_start..region.x_end {
                        if (x + y) % 6 < 3 {
                            pixels[y * width + x] = color;
                        }
                    }
                }
            }
        }
        BorderStyle::DashDot => {
            // Dash, gap, dot, gap: a four-phase pattern, so it needs its own test rather than draw_lengthwise_pattern's single mark/gap pair.
            let (dash, gap) = (8usize, 4usize);
            let dot = bw.max(1);
            let pattern_length = dash + gap + dot + gap;
            let dot_start = dash + gap;
            for region in border_regions(width, height, bw) {
                let is_horizontal = region.x_end - region.x_start == width;
                for y in region.y_start..region.y_end {
                    for x in region.x_start..region.x_end {
                        let phase = (if is_horizontal { x } else { y }) % pattern_length;
                        if phase < dash || (phase >= dot_start && phase < dot_start + dot) {
                            pixels[y * width + x] = color;
                        }
                    }
                }
            }
        }
        BorderStyle::CornerBrackets => {
            // Arm length scales with border width but is capped at a third of the shorter dimension so brackets from adjacent corners never meet.
            let arm = (bw * 4).min(width.min(height) / 3);
            let f = |p: &mut [u32], x, y, w, h| gdi_overlay::fill_rect(p, width, height, x, y, w, h, color);
            f(pixels, 0, 0, arm, bw);
            f(pixels, 0, 0, bw, arm);
            f(pixels, width - arm, 0, arm, bw);
            f(pixels, width - bw, 0, bw, arm);
            f(pixels, 0, height - bw, arm, bw);
            f(pixels, 0, height - arm, bw, arm);
            f(pixels, width - arm, height - bw, arm, bw);
            f(pixels, width - bw, height - arm, bw, arm);
        }
    }
}

/// Inserts comma thousands-separators into the leading run of ASCII digits in `text` (e.g. "12405.3 m3/min" -> "12,405.3 m3/min").
pub(super) fn insert_thousands_separators(text: &str) -> String {
    let digit_end = text.bytes().take_while(u8::is_ascii_digit).count();
    if digit_end <= 3 {
        return text.to_owned();
    }
    let first_group = if digit_end.is_multiple_of(3) { 3 } else { digit_end % 3 };
    let mut out = String::with_capacity(text.len() + digit_end / 3);
    out.push_str(&text[..first_group]);
    let mut i = first_group;
    while i < digit_end {
        out.push(',');
        out.push_str(&text[i..i + 3]);
        i += 3;
    }
    out.push_str(&text[digit_end..]);
    out
}

/// Abbreviates an ISK value with k/m suffixes (e.g. 2_450_000.0 -> "2.5m", 200_000.0 -> "200k", 850.0 -> "850").
pub fn format_isk_abbrev(value: f32) -> String {
    let abs_value = value.abs();
    if abs_value >= 1_000_000.0 {
        format!("{:.1}m", value / 1_000_000.0)
    } else if abs_value >= 1_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

/// Measures text dimensions without rendering; the correct font must already be selected into `dc` by the caller.
pub(super) fn measure_text(dc: HDC, text: &str) -> TextDimensions {
    let size: SIZE = gdi_overlay::measure_text_size(TEXT_BUFFER_SIZE, dc, text);
    TextDimensions { width: size.cx.max(0) as usize + TEXT_PADDING_X * 2, height: size.cy.max(0) as usize + TEXT_PADDING_Y * 2 }
}

/// Renders text onto the device context at the specified position; the correct font must already be selected into `dc` by the caller.
pub(super) fn render_text(dc: HDC, text: &str, x: i32, y: i32, color: u32) {
    gdi_overlay::draw_text(TEXT_BUFFER_SIZE, dc, x + TEXT_PADDING_X as i32, y + TEXT_PADDING_Y as i32, text, color);
}

/// Whether a cached font's name/size/weight differ from the settings that would be used to render now.
fn font_settings_changed(cached: &(String, i32, FontWeight), name: &str, size: i32, weight: FontWeight) -> bool {
    cached.0 != name || cached.1 != size || cached.2 != weight
}

/// Per-state override (if any) wins, then opacity is forced fully opaque when the window's own Opacity setting should
/// apply instead, so it isn't compounded with this color's own alpha.
fn resolve_text_bg_color(state_cfg: &StateVisualConfig, base_color: u32, force_opaque: bool) -> u32 {
    let resolved = state_cfg.text_bg_color_or(base_color);
    if force_opaque { with_alpha(resolved, 255) } else { resolved }
}

fn rate_unit(unit: IskRateUnit) -> (f32, &'static str) {
    match unit {
        IskRateUnit::Hour => (3600.0, "hr"),
        IskRateUnit::Minute => (60.0, "min"),
    }
}

/// Builds RenderSettings from config; the single point where a thumbnail's effective render state determines all visual properties.
pub(super) fn create_render_settings(cfg: &Config, thumbnail: &ThumbnailWindow, active_source_hwnd: Option<HWND>) -> RenderSettings {
    let state = thumbnail.effective_render_state(active_source_hwnd);
    let is_visible = thumbnail.visibility_state.is_visible();
    // Read live rather than cached, so fonts/geometry track whichever monitor this window is on right now.
    let dpi_scale = dpi_to_scale(window_dpi(thumbnail.hwnd));
    let t = &cfg.thumbnail;
    let state_cfg = t.state_config(state);

    // Alert is treated like Active as a base (it's an attention event); StateVisualConfig for Alert, per-type overrides, and per-character overrides all layer on top of this.
    let is_alert_like = matches!(state, ThumbnailState::Active | ThumbnailState::Alert);
    let base_border_width = if is_alert_like { t.border_width } else { t.inactive_border_width };
    let base_border_color = if is_alert_like { t.border_color } else { t.inactive_border_color };
    let base_border_style = if is_alert_like { t.border_style } else { t.inactive_border_style };
    let base_show_border = if is_alert_like { t.show_border_when_focused } else { t.show_border_when_inactive };

    // Per-character override: hides this thumbnail unconditionally, regardless of state.
    let char_hidden = thumbnail.cached_hide_thumbnail;

    // char_hidden is handled separately as an absolute override on the final show_thumbnail field below.
    let base_show_thumbnail = if !is_visible { false } else if state == ThumbnailState::Active { !t.active_thumbnail_hidden } else { true };

    // If the thumbnail, active thumbnail, or this character specifically is hidden, don't show border or text either.
    let should_hide_all = !is_visible || char_hidden || (state == ThumbnailState::Active && t.active_thumbnail_hidden);

    // Whether this thumbnail belongs to the character focused when the notification fired; notification border effects must not fight with that character's always-on active border.
    let notif_on_focused_char = thumbnail.is_focused(active_source_hwnd);
    let newest = thumbnail.active_notifications.first();

    // Border color/flash effects are governed solely by the newest stacked notification; older entries only add text lines.
    // Per-type "show_border: false" forces the border off during Alert, skipped for the focused character so it can't also hide that character's active border.
    let notif_hides_border = state == ThumbnailState::Alert && !notif_on_focused_char && newest.is_some_and(|n| !n.show_border);
    // Blinks the border off for alternating phases at Alert start (see is_notification_flash_off), skipped for the focused character for the same reason.
    let notif_flash_hides_border =
        state == ThumbnailState::Alert && !notif_on_focused_char && newest.is_some_and(|n| is_notification_flash_off(n, Ticks::now()));

    let effective_show_border =
        if should_hide_all || notif_hides_border || notif_flash_hides_border { false } else { state_cfg.show_border_or(base_show_border) };
    let effective_show_text = !should_hide_all && t.show_text;
    let effective_show_character_name = !should_hide_all && t.show_text && t.show_character_name;
    let effective_show_system_name = !should_hide_all && t.show_text && t.show_system_name && !thumbnail.system_name.is_empty();
    let effective_show_notifications = !should_hide_all && t.show_text && t.notifications.enabled;
    // Combat/Mining/Bounty are also gated by show_text, but checked directly in the render function, since they already bypass RenderSettings for their enabled-checks.
    let effective_show_group_badge = !should_hide_all && t.show_text && t.show_quick_group_badge;

    let mut final_border_color = state_cfg.border_color_or(base_border_color);

    // When suppress_when_focused is true and the character is focused, the border falls back to normal Active appearance instead of the Alert override color.
    let is_suppressed_alert =
        state == ThumbnailState::Alert && newest.is_some_and(|n| n.suppress_when_focused && thumbnail.is_focused(active_source_hwnd));

    // Per-type border color override sits above the Alert StateVisualConfig but below per-character overrides; skipped when the alert is suppressed.
    if state == ThumbnailState::Alert && !is_suppressed_alert {
        if let Some(color) = newest.and_then(|n| n.border_color_override) {
            final_border_color = color;
        }
    }

    // Fallback color for stacked notification lines that don't carry their own text_color_override.
    let notification_base_text_color = state_cfg.text_color_or(t.character_name_color);

    // Per-character border color has the highest precedence; a suppressed Alert is treated as Active for border purposes.
    if let Some(char_colors) = thumbnail.cached_border_colors {
        if state == ThumbnailState::Active || (state == ThumbnailState::Alert && is_suppressed_alert) {
            if let Some(color) = char_colors.active_border_color {
                final_border_color = color;
            }
        } else if matches!(state, ThumbnailState::Inactive | ThumbnailState::Minimized) {
            if let Some(color) = char_colors.inactive_border_color {
                final_border_color = color;
            }
        }
    }

    // Unique Character Name Colors takes precedence over the per-state color, same as border color above.
    let final_text_color = thumbnail.cached_character_color.unwrap_or_else(|| state_cfg.text_color_or(t.character_name_color));

    // Read the already-sized window back rather than duplicating the region-fit grid math here.
    let mut live_size = None;
    if cfg.display.layout_mode == eve_maj_core::types::LayoutMode::RegionFit {
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { GetClientRect(thumbnail.hwnd, &mut client) } != 0 && client.right > 0 && client.bottom > 0 {
            live_size = Some((client.right, client.bottom));
        }
    }
    let (overlay_width, overlay_height) = live_size.unwrap_or_else(|| {
        let char_size = thumbnail.cached_thumbnail_size;
        let logical_width = char_size.and_then(|cs| cs.width).unwrap_or(t.width);
        let logical_height = char_size.and_then(|cs| cs.height).unwrap_or(t.height);
        (scale_pixels(logical_width, dpi_scale), scale_pixels(logical_height, dpi_scale))
    });

    // Builds the visible stack, newest first: each entry keeps its own suppress_when_focused/text_color_override,
    // so different notification types can be filtered and colored independently within the same stack.
    let mut notification_lines = Vec::new();
    if effective_show_notifications {
        let notif_is_focused = thumbnail.is_focused(active_source_hwnd);
        for notif in &thumbnail.active_notifications {
            if notif.suppress_when_focused && notif_is_focused {
                continue;
            }
            notification_lines.push(NotificationLine { text: notif.text.clone(), color: notif.text_color_override.unwrap_or(notification_base_text_color) });
        }
    }

    let opaque = t.apply_opacity_to_overlay_texts;
    let show_exclusion_overlay = thumbnail.is_excluded_from_cycle && is_visible;
    if thumbnail.is_excluded_from_cycle {
        SLOG.debug(format_args!(
            "Render settings for {}: is_excluded={}, is_visible={is_visible}, show_overlay={show_exclusion_overlay}",
            thumbnail.character_name, thumbnail.is_excluded_from_cycle
        ));
    }

    RenderSettings {
        show_text: effective_show_text,
        show_character_name: effective_show_character_name,
        character_name: thumbnail.character_name.clone(),
        show_system_name: effective_show_system_name,
        system_name: thumbnail.system_name.clone(),
        character_name_color: final_text_color,
        // Already resolved when system name was set.
        system_name_color: thumbnail.cached_system_color,
        character_name_bg_color: resolve_text_bg_color(&state_cfg, t.character_name_bg_color, opaque),
        system_name_bg_color: resolve_text_bg_color(&state_cfg, t.system_name_bg_color, opaque),
        group_badge_bg_color: resolve_text_bg_color(&state_cfg, t.quick_group_badge_bg_color, opaque),
        notifications_bg_color: resolve_text_bg_color(&state_cfg, t.notifications.bg_color, opaque),
        combat_incoming_bg_color: resolve_text_bg_color(&state_cfg, cfg.combat.incoming_bg_color, opaque),
        combat_outgoing_bg_color: resolve_text_bg_color(&state_cfg, cfg.combat.outgoing_bg_color, opaque),
        mining_bg_color: resolve_text_bg_color(&state_cfg, cfg.mining.bg_color, opaque),
        bounty_bg_color: resolve_text_bg_color(&state_cfg, cfg.bounty.bg_color, opaque),
        resources_bg_color: resolve_text_bg_color(&state_cfg, cfg.resources.bg_color, opaque),
        character_name_font_name: t.character_name_font_name.clone(),
        character_name_font_size: scale_pixels(t.character_name_font_size, dpi_scale),
        character_name_font_weight: t.character_name_font_weight,
        character_name_position: t.character_name_position,
        character_name_offset_x: t.character_name_offset_x,
        character_name_offset_y: t.character_name_offset_y,
        system_name_position: t.system_name_position,
        system_name_offset_x: t.system_name_offset_x,
        system_name_offset_y: t.system_name_offset_y,
        system_name_font_name: t.system_name_font_name.clone(),
        system_name_font_size: scale_pixels(t.system_name_font_size, dpi_scale),
        system_name_font_weight: t.system_name_font_weight,
        show_notifications: effective_show_notifications,
        notification_lines,
        notifications_position: t.notifications.position,
        notifications_offset_x: t.notifications.offset_x,
        notifications_offset_y: t.notifications.offset_y,
        notifications_font_name: t.notifications.font_name.clone(),
        notifications_font_size: scale_pixels(t.notifications.font_size, dpi_scale),
        notifications_font_weight: t.notifications.font_weight,
        show_border: effective_show_border,
        border_width: state_cfg.border_width_or(base_border_width),
        border_color: final_border_color,
        border_style: state_cfg.border_style_or(base_border_style),
        show_exclusion_overlay,
        exclusion_overlay_style: t.exclusion_overlay_style,
        exclusion_overlay_color: t.exclusion_overlay_color,
        show_group_badge: effective_show_group_badge && !thumbnail.cached_group_badge_label.is_empty() && is_visible,
        group_badge_text: thumbnail.cached_group_badge_label.clone(),
        group_badge_color: t.quick_group_badge_color,
        group_badge_position: t.quick_group_badge_position,
        group_badge_offset_x: t.quick_group_badge_offset_x,
        group_badge_offset_y: t.quick_group_badge_offset_y,
        group_badge_font_name: t.quick_group_badge_font_name.clone(),
        group_badge_font_size: scale_pixels(t.quick_group_badge_font_size, dpi_scale),
        group_badge_font_weight: t.quick_group_badge_font_weight,
        // visibility_state and per-character hide_thumbnail take absolute priority over per-state show_thumbnail config.
        show_thumbnail: if !is_visible || char_hidden { false } else { state_cfg.show_thumbnail_or(base_show_thumbnail) },
        overlay_alpha: if opaque { thumbnail.cached_opacity } else { OVERLAY_ALPHA },
        overlay_width,
        overlay_height,
        // -1.0 stands in for "calculating" (None) here — no real rate is negative, and this struct only needs
        // equality for cache invalidation, not the calculating/zero distinction the render code cares about.
        dps_incoming: if cfg.combat.enabled { thumbnail.last_incoming_dps.unwrap_or(-1.0) } else { 0.0 },
        dps_outgoing: if cfg.combat.enabled { thumbnail.last_outgoing_dps.unwrap_or(-1.0) } else { 0.0 },
        mining_rate: if cfg.mining.enabled { thumbnail.last_mining_rate.unwrap_or(-1.0) } else { 0.0 },
        mining_isk_rate: if cfg.mining.enabled && cfg.mining.show_isk_rate { thumbnail.last_mining_isk_rate.unwrap_or(-1.0) } else { 0.0 },
        bounty_isk_rate: if cfg.bounty.enabled { thumbnail.last_bounty_isk_rate.unwrap_or(-1.0) } else { 0.0 },
        resource_cpu_percent: if cfg.resources.enabled { thumbnail.last_cpu_percent } else { 0.0 },
        resource_ram_mb: if cfg.resources.enabled { thumbnail.last_ram_mb } else { 0.0 },
        resource_vram_mb: if cfg.resources.enabled { thumbnail.last_vram_mb } else { 0.0 },
        has_dps_data: thumbnail.has_dps_data,
        has_mining_data: thumbnail.has_mining_data,
        has_bounty_data: thumbnail.has_bounty_data,
        has_resource_data: cfg.resources.enabled && thumbnail.has_resource_data,
        has_vram_data: thumbnail.has_vram_data,
        dps_incoming_color: cfg.combat.incoming_color,
        dps_outgoing_color: cfg.combat.outgoing_color,
        mining_color: cfg.mining.color,
        bounty_color: cfg.bounty.color,
        resources_color: cfg.resources.color,
    }
}

impl Painter {
    /// Paints the thumbnail's text/border overlay into its cached bitmap and pushes it to the layered text window.
    pub(super) fn render_thumbnail_overlay(&mut self, index: usize, settings: &RenderSettings) -> Result<(), RenderError> {
        let config = globals::config();
        let (width, height) = (settings.overlay_width, settings.overlay_height);

        let thumbnail = &mut self.thumbnails[index];
        // Reuse the cached overlay bitmap unless dimensions changed (first render or resize).
        if OverlayBitmap::needs_resize(&thumbnail.cached_overlay, width, height) {
            let init_dc = unsafe { GetDC(std::ptr::null_mut()) };
            if init_dc.is_null() {
                return Err(RenderError::GetDcFailed);
            }
            let ok = OverlayBitmap::recreate(&mut thumbnail.cached_overlay, init_dc, width, height);
            unsafe { ReleaseDC(std::ptr::null_mut(), init_dc) };
            if !ok {
                return Err(RenderError::CreateBitmapFailed);
            }
            SLOG.debug(format_args!("Allocated overlay bitmap {width}x{height} for {}", thumbnail.character_name));
        }

        let hwnd = thumbnail.hwnd;
        let text_hwnd = thumbnail.text_hwnd;
        let dpi = window_dpi(hwnd);
        // Combat/mining/bounty font sizes bypass RenderSettings (see create_render_settings), so they're scaled here instead.
        let dpi_scale = dpi_to_scale(dpi);

        let font = self.cached_font(FontSlot::Main, dpi, &settings.character_name_font_name, settings.character_name_font_size, settings.character_name_font_weight)?;
        let sys_font = if settings.show_system_name {
            Some(self.cached_font(FontSlot::SystemName, dpi, &settings.system_name_font_name, settings.system_name_font_size, settings.system_name_font_weight)?)
        } else {
            None
        };
        let has_notification_text = !settings.notification_lines.is_empty();
        let notif_font = if settings.show_notifications && has_notification_text {
            Some(self.cached_font(FontSlot::Notification, dpi, &settings.notifications_font_name, settings.notifications_font_size, settings.notifications_font_weight)?)
        } else {
            None
        };
        let badge_font = if settings.show_group_badge {
            Some(self.cached_font(FontSlot::GroupBadge, dpi, &settings.group_badge_font_name, settings.group_badge_font_size, settings.group_badge_font_weight)?)
        } else {
            None
        };

        let combat_cfg = &config.combat;
        let show_text = config.thumbnail.show_text;
        let thumbnail = &self.thumbnails[index];
        let want_dps_in = combat_cfg.enabled && show_text && combat_cfg.show_incoming && thumbnail.has_dps_data && thumbnail.last_incoming_dps.is_none_or(|d| d > 0.0);
        let want_dps_out = combat_cfg.enabled && show_text && combat_cfg.show_outgoing && thumbnail.has_dps_data && thumbnail.last_outgoing_dps.is_none_or(|d| d > 0.0);
        let want_mining = config.mining.enabled && show_text && thumbnail.has_mining_data && thumbnail.last_mining_rate.is_none_or(|r| r > 0.0);
        let want_bounty = config.bounty.enabled && show_text && thumbnail.has_bounty_data && thumbnail.last_bounty_isk_rate.is_none_or(|r| r > 0.0);
        let want_resources = config.resources.enabled && show_text && thumbnail.has_resource_data;

        let dps_in_font = if want_dps_in {
            Some(self.cached_font(FontSlot::Combat, dpi, &combat_cfg.incoming_font_name, scale_pixels(combat_cfg.incoming_font_size, dpi_scale), combat_cfg.incoming_font_weight)?)
        } else {
            None
        };
        let dps_out_font = if want_dps_out {
            Some(self.cached_font(FontSlot::CombatOutgoing, dpi, &combat_cfg.outgoing_font_name, scale_pixels(combat_cfg.outgoing_font_size, dpi_scale), combat_cfg.outgoing_font_weight)?)
        } else {
            None
        };
        let mining_font = if want_mining {
            Some(self.cached_font(FontSlot::Mining, dpi, &config.mining.font_name, scale_pixels(config.mining.font_size, dpi_scale), config.mining.font_weight)?)
        } else {
            None
        };
        let bounty_font = if want_bounty {
            Some(self.cached_font(FontSlot::Bounty, dpi, &config.bounty.font_name, scale_pixels(config.bounty.font_size, dpi_scale), config.bounty.font_weight)?)
        } else {
            None
        };
        let resources_font = if want_resources {
            Some(self.cached_font(FontSlot::Resources, dpi, &config.resources.font_name, scale_pixels(config.resources.font_size, dpi_scale), config.resources.font_weight)?)
        } else {
            None
        };

        let thumbnail = &mut self.thumbnails[index];
        let display_name = config.display_name(&thumbnail.character_name).to_owned();
        let system_name = thumbnail.system_name.clone();
        let overlay = thumbnail.cached_overlay.as_mut().expect("overlay bitmap allocated above");
        let (ow, oh) = (overlay.width, overlay.height);
        let dc = overlay.mem_dc;
        overlay.pixels().fill(0);

        if settings.show_exclusion_overlay {
            draw_exclusion_overlay(overlay.pixels(), ow, oh, settings.exclusion_overlay_color, settings.exclusion_overlay_style);
        }

        // Select the main font once for all char/system/notification measure+render calls; restore the original object on exit.
        let old_main_font = unsafe { SelectObject(dc, font) };
        let select = |f: HFONT| unsafe {
            SelectObject(dc, f);
        };

        let char_font_key = (settings.character_name_font_name.clone(), settings.character_name_font_size, settings.character_name_font_weight);
        let mut char_dims = TextDimensions::default();
        if settings.show_character_name {
            match thumbnail.cached_char_dims {
                Some(d) if !font_settings_changed(&thumbnail.cached_font, &char_font_key.0, char_font_key.1, char_font_key.2) => char_dims = d,
                _ => {
                    char_dims = measure_text(dc, &display_name);
                    thumbnail.cached_char_dims = Some(char_dims);
                    thumbnail.cached_font = char_font_key;
                }
            }
        }

        let mut system_dims = TextDimensions::default();
        if let Some(sf) = sys_font {
            let key = (settings.system_name_font_name.clone(), settings.system_name_font_size, settings.system_name_font_weight);
            match thumbnail.cached_sys_dims {
                Some(d) if !font_settings_changed(&thumbnail.cached_sys_font, &key.0, key.1, key.2) => system_dims = d,
                _ => {
                    select(sf);
                    system_dims = measure_text(dc, &system_name);
                    select(font);
                    thumbnail.cached_sys_dims = Some(system_dims);
                    thumbnail.cached_sys_font = key;
                }
            }
        }

        // Stacked notification lines aren't dims-cached (unlike char/system name above): the stack's contents change
        // far more often than those, so a cache would invalidate almost every render anyway.
        let mut notif_line_dims = Vec::new();
        let mut notifications_dims = TextDimensions::default();
        if let Some(nf) = notif_font {
            select(nf);
            for line in &settings.notification_lines {
                let d = measure_text(dc, &line.text);
                notifications_dims.width = notifications_dims.width.max(d.width);
                notifications_dims.height += d.height;
                notif_line_dims.push(d);
            }
            select(font);
        }

        let mut badge_dims = TextDimensions::default();
        if let Some(bf) = badge_font {
            let key = (settings.group_badge_font_name.clone(), settings.group_badge_font_size, settings.group_badge_font_weight);
            match thumbnail.cached_badge_dims {
                Some(d) if !font_settings_changed(&thumbnail.cached_badge_font, &key.0, key.1, key.2) => badge_dims = d,
                _ => {
                    select(bf);
                    badge_dims = measure_text(dc, &settings.group_badge_text);
                    select(font);
                    thumbnail.cached_badge_dims = Some(badge_dims);
                    thumbnail.cached_badge_font = key;
                }
            }
        }

        let pos = |position, dims, offset_x, offset_y| calculate_text_position(position, dims, ow, oh, offset_x, offset_y);

        // Collects every single-rect text run (character/system/badge/DPS/mining/bounty/resources) so their
        // background-fill, glyph-render, and alpha-fixup passes can share one loop each below. Notifications
        // are excluded: they fill/fixup as one combined multi-line rect but render each line separately.
        let mut lines: Vec<DrawLine> = Vec::with_capacity(11);
        let single = |font, text: String, p: TextPos, dims, color, bg_color| DrawLine { font, text, render_pos: p, bg_pos: p, bg_dims: dims, color, bg_color };

        if settings.show_character_name {
            let p = pos(settings.character_name_position, char_dims, settings.character_name_offset_x, settings.character_name_offset_y);
            lines.push(single(font, display_name, p, char_dims, settings.character_name_color, settings.character_name_bg_color));
        }
        if let Some(sf) = sys_font {
            let p = pos(settings.system_name_position, system_dims, settings.system_name_offset_x, settings.system_name_offset_y);
            lines.push(single(sf, system_name, p, system_dims, settings.system_name_color, settings.system_name_bg_color));
        }
        let notifications_pos =
            if notif_font.is_some() { pos(settings.notifications_position, notifications_dims, settings.notifications_offset_x, settings.notifications_offset_y) } else { TextPos::default() };
        if let Some(bf) = badge_font {
            let p = pos(settings.group_badge_position, badge_dims, settings.group_badge_offset_x, settings.group_badge_offset_y);
            lines.push(single(bf, settings.group_badge_text.clone(), p, badge_dims, settings.group_badge_color, settings.group_badge_bg_color));
        }

        let thumbnail = &self.thumbnails[index];

        if let Some(f) = dps_in_font {
            select(f);
            let text = match thumbnail.last_incoming_dps {
                Some(dps) if combat_cfg.incoming_show_prefix => format!("IN: {dps:.0}"),
                Some(dps) => format!("{dps:.0}"),
                None if combat_cfg.incoming_show_prefix => "IN: ??".to_owned(),
                None => "??".to_owned(),
            };
            let d = measure_text(dc, &text);
            let p = pos(combat_cfg.incoming_position, d, combat_cfg.incoming_offset_x, combat_cfg.incoming_offset_y);
            lines.push(single(f, text, p, d, combat_cfg.incoming_color, settings.combat_incoming_bg_color));
            select(font);
        }
        if let Some(f) = dps_out_font {
            select(f);
            let text = match thumbnail.last_outgoing_dps {
                Some(dps) if combat_cfg.outgoing_show_prefix => format!("OUT: {dps:.0}"),
                Some(dps) => format!("{dps:.0}"),
                None if combat_cfg.outgoing_show_prefix => "OUT: ??".to_owned(),
                None => "??".to_owned(),
            };
            let d = measure_text(dc, &text);
            let p = pos(combat_cfg.outgoing_position, d, combat_cfg.outgoing_offset_x, combat_cfg.outgoing_offset_y);
            lines.push(single(f, text, p, d, combat_cfg.outgoing_color, settings.combat_outgoing_bg_color));
            select(font);
        }

        // ISK rate (if shown) stacks as a second GDI-single-line text; both lines share one background width so text can align to the position's edge without a gap.
        if let Some(mf) = mining_font {
            let mining_cfg = &config.mining;
            select(mf);
            let mining_text = match thumbnail.last_mining_rate {
                Some(rate) => {
                    // Displayed per-minute rather than per-second so low-yield ore doesn't round to "0".
                    let rate_per_min = rate * 60.0;
                    let raw = if rate_per_min < 10.0 { format!("{rate_per_min:.1}") } else { format!("{rate_per_min:.0}") };
                    let formatted = insert_thousands_separators(&raw);
                    if mining_cfg.show_prefix { format!("M: {formatted} m3/min") } else { format!("{formatted} m3/min") }
                }
                None if mining_cfg.show_prefix => "M: ?? m3/min".to_owned(),
                None => "?? m3/min".to_owned(),
            };
            let mining_dims = measure_text(dc, &mining_text);

            let mut isk_text = String::new();
            let mut isk_dims = TextDimensions::default();
            if mining_cfg.show_isk_rate {
                let (period_secs, unit_suffix) = rate_unit(mining_cfg.isk_rate_unit);
                isk_text = match thumbnail.last_mining_isk_rate {
                    Some(isk_rate) => format!("{} ISK/{unit_suffix}", format_isk_abbrev(isk_rate * period_secs)),
                    None => format!("?? ISK/{unit_suffix}"),
                };
                isk_dims = measure_text(dc, &isk_text);
            }

            // Anchored as one combined block so Bottom*/Center* positions account for both lines' height, not just the first; each line then just stacks top-down from there.
            let block_width = mining_dims.width.max(isk_dims.width);
            let anchor = pos(mining_cfg.position, TextDimensions { width: block_width, height: mining_dims.height + isk_dims.height }, mining_cfg.offset_x, mining_cfg.offset_y);
            let h_align = horizontal_align_of(mining_cfg.position);
            let mining_pos = TextPos { x: aligned_line_x(anchor.x, block_width, mining_dims.width, h_align), y: anchor.y };
            lines.push(DrawLine {
                font: mf,
                text: mining_text,
                render_pos: mining_pos,
                bg_pos: TextPos { x: anchor.x, y: mining_pos.y },
                bg_dims: TextDimensions { width: block_width, height: mining_dims.height },
                color: mining_cfg.color,
                bg_color: settings.mining_bg_color,
            });
            if !isk_text.is_empty() {
                let isk_pos = TextPos { x: aligned_line_x(anchor.x, block_width, isk_dims.width, h_align), y: anchor.y + mining_dims.height as i32 };
                lines.push(DrawLine {
                    font: mf,
                    text: isk_text,
                    render_pos: isk_pos,
                    bg_pos: TextPos { x: anchor.x, y: isk_pos.y },
                    bg_dims: TextDimensions { width: block_width, height: isk_dims.height },
                    color: mining_cfg.color,
                    bg_color: settings.mining_bg_color,
                });
            }
            select(font);
        }

        if let Some(bf) = bounty_font {
            let bounty_cfg = &config.bounty;
            select(bf);
            let (period_secs, unit_suffix) = rate_unit(bounty_cfg.isk_rate_unit);
            let text = match thumbnail.last_bounty_isk_rate {
                Some(isk_rate) => {
                    let abbrev = format_isk_abbrev(isk_rate * period_secs);
                    if bounty_cfg.show_prefix { format!("ISK: {abbrev} ISK/{unit_suffix}") } else { format!("{abbrev} ISK/{unit_suffix}") }
                }
                None if bounty_cfg.show_prefix => format!("ISK: ?? ISK/{unit_suffix}"),
                None => format!("?? ISK/{unit_suffix}"),
            };
            let d = measure_text(dc, &text);
            let p = pos(bounty_cfg.position, d, bounty_cfg.offset_x, bounty_cfg.offset_y);
            lines.push(single(bf, text, p, d, bounty_cfg.color, settings.bounty_bg_color));
            select(font);
        }

        // One stacked line per enabled metric, same as the mining block above.
        if let Some(rf) = resources_font {
            let resources_cfg = &config.resources;
            select(rf);
            let mut texts = Vec::new();
            if resources_cfg.show_cpu {
                texts.push(format!("CPU: {:.0}%", thumbnail.last_cpu_percent));
            }
            if resources_cfg.show_ram {
                texts.push(format!("RAM: {:.0}MB", thumbnail.last_ram_mb));
            }
            if resources_cfg.show_vram && thumbnail.has_vram_data {
                texts.push(format!("VRAM: {:.0}MB", thumbnail.last_vram_mb));
            }
            if !texts.is_empty() {
                let dims: Vec<TextDimensions> = texts.iter().map(|t| measure_text(dc, t)).collect();
                let block_width = dims.iter().map(|d| d.width).max().unwrap_or(0);
                let combined_height = dims.iter().map(|d| d.height).sum();
                let anchor = pos(resources_cfg.position, TextDimensions { width: block_width, height: combined_height }, resources_cfg.offset_x, resources_cfg.offset_y);
                let h_align = horizontal_align_of(resources_cfg.position);
                let mut y = anchor.y;
                for (text, d) in texts.into_iter().zip(dims) {
                    lines.push(DrawLine {
                        font: rf,
                        text,
                        render_pos: TextPos { x: aligned_line_x(anchor.x, block_width, d.width, h_align), y },
                        bg_pos: TextPos { x: anchor.x, y },
                        bg_dims: TextDimensions { width: block_width, height: d.height },
                        color: resources_cfg.color,
                        bg_color: settings.resources_bg_color,
                    });
                    y += d.height as i32;
                }
            }
            select(font);
        }

        let overlay = self.thumbnails[index].cached_overlay.as_mut().expect("overlay bitmap allocated above");

        // Background-fill for every collected single-rect run, before the border so the border renders on top.
        for line in &lines {
            fill_text_background(overlay.pixels(), ow, oh, line.bg_pos.x, line.bg_pos.y, line.bg_dims.width, line.bg_dims.height, line.bg_color);
        }
        // Notifications' background is filled last so it isn't covered by an overlapping element's background.
        if notif_font.is_some() {
            fill_text_background(overlay.pixels(), ow, oh, notifications_pos.x, notifications_pos.y, notifications_dims.width, notifications_dims.height, settings.notifications_bg_color);
        }

        if settings.show_border {
            draw_border(overlay.pixels(), ow, oh, settings.border_width as usize, settings.border_color, settings.border_style);
        }

        for line in &lines {
            select(line.font);
            render_text(dc, &line.text, line.render_pos.x, line.render_pos.y, line.color);
        }
        select(font);

        // Notifications render as several separately-colored lines under one shared fixup rect, so they stay outside the
        // loop above. Rendered last so notification text is never hidden behind an overlapping element.
        if let Some(nf) = notif_font {
            select(nf);
            let mut y = notifications_pos.y;
            for (line, d) in settings.notification_lines.iter().zip(&notif_line_dims) {
                render_text(dc, &line.text, notifications_pos.x, y, line.color);
                y += d.height as i32;
            }
            select(font);
        }

        // Bounded to the rects text/glyphs were actually drawn into instead of scanning the whole overlay.
        for line in &lines {
            gdi_overlay::fix_text_alpha_rect(overlay.pixels(), ow, oh, line.bg_pos.x, line.bg_pos.y, line.bg_dims.width, line.bg_dims.height);
        }
        if notif_font.is_some() {
            gdi_overlay::fix_text_alpha_rect(overlay.pixels(), ow, oh, notifications_pos.x, notifications_pos.y, notifications_dims.width, notifications_dims.height);
        }

        if !old_main_font.is_null() {
            unsafe { SelectObject(dc, old_main_font) };
        }

        let window_size = SIZE { cx: width, cy: height };
        let source_pos = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: settings.overlay_alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        // hdcDst=null is valid here: UpdateLayeredWindow uses the screen DC internally when hdcSrc is supplied, sparing a GetDC/ReleaseDC pair every repaint.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow(
                text_hwnd, std::ptr::null_mut(), std::ptr::null(), &window_size, dc, &source_pos, 0, &blend,
                windows_sys::Win32::UI::WindowsAndMessaging::ULW_ALPHA,
            )
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_and_isk() {
        assert_eq!(insert_thousands_separators("12405.3 m3/min"), "12,405.3 m3/min");
        assert_eq!(insert_thousands_separators("999"), "999");
        assert_eq!(insert_thousands_separators("1234567"), "1,234,567");
        assert_eq!(format_isk_abbrev(2_450_000.0), "2.5m");
        assert_eq!(format_isk_abbrev(200_000.0), "200k");
        assert_eq!(format_isk_abbrev(850.0), "850");
    }
}
