//! What one thumbnail should look like right now, and the pixel-level drawing that doesn't depend on a text engine.
//!
//! Platform-neutral twin of the Windows painter's `render.rs`: `create_render_settings` makes the same decisions from a
//! plain [`ThumbnailView`] instead of a Win32 `ThumbnailWindow`, and the border/exclusion/background helpers work on
//! any premultiplied `0xAARRGGBB` pixel buffer (a Windows DIB section or a cairo ARGB32 image surface alike).

use crate::color::with_alpha;
use crate::config::{CharacterBorderColors, Config, StateVisualConfig};
use crate::layout::scale_pixels;
use crate::state::ThumbnailState;
use crate::types::{BorderStyle, ExclusionOverlayStyle, FontWeight, NotificationType, TextPosition};

pub const TEXT_PADDING_X: usize = 5;
pub const TEXT_PADDING_Y: usize = 2;
pub const OVERLAY_ALPHA: u8 = 255;
/// Cap on simultaneously stacked notifications per thumbnail.
pub const MAX_STACKED_NOTIFICATIONS: usize = 3;

const NOTIFICATION_FLASH_PHASE_MS: u64 = 150;
const NOTIFICATION_FLASH_CYCLES: u64 = 4;
const NOTIFICATION_FLASH_TOTAL_MS: u64 = NOTIFICATION_FLASH_PHASE_MS * NOTIFICATION_FLASH_CYCLES * 2;

/// Monotonic milliseconds since the first call; for elapsed-time comparisons only.
pub fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    // +1 so a real timestamp is never 0, which callers treat as "never".
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveNotification {
    pub text: String,
    pub notification_type: NotificationType,
    pub start_ms: u64,
    pub duration_ms: u32,
    pub suppress_when_focused: bool,
    pub suppress_when_clicked: bool,
    pub border_color_override: Option<u32>,
    pub text_color_override: Option<u32>,
    pub show_border: bool,
    pub flash_border: bool,
}

/// Whether a flashing notification's border should currently be hidden; false once the flash sequence has finished.
pub fn is_notification_flash_off(notif: &ActiveNotification, now: u64) -> bool {
    if !notif.show_border || !notif.flash_border {
        return false;
    }
    let elapsed = now.saturating_sub(notif.start_ms);
    if elapsed >= NOTIFICATION_FLASH_TOTAL_MS {
        return false;
    }
    (elapsed / NOTIFICATION_FLASH_PHASE_MS) % 2 == 1
}

/// Comma-joined label of the badge-enabled hotkey groups `character_name` is in ("1, 3"); "" = none.
pub fn group_badge_label(config: &Config, character_name: &str) -> String {
    config
        .hotkey_groups
        .iter()
        .enumerate()
        .filter(|(_, g)| g.show_badge && g.characters.iter().any(|c| c == character_name))
        .map(|(i, g)| if g.name.is_empty() { (i + 1).to_string() } else { g.name.clone() })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Live tracker readings shown on the overlay (combat/mining/bounty/resources); all absent until a tracker reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Metrics {
    pub incoming_dps: Option<f32>,
    pub outgoing_dps: Option<f32>,
    pub mining_rate: Option<f32>,
    pub mining_isk_rate: Option<f32>,
    pub bounty_isk_rate: Option<f32>,
    pub cpu_percent: f32,
    pub ram_mb: f32,
    pub vram_mb: f32,
    pub has_dps_data: bool,
    pub has_mining_data: bool,
    pub has_bounty_data: bool,
    pub has_resource_data: bool,
    pub has_vram_data: bool,
}

/// Everything about one thumbnail that decides its look, as plain data.
pub struct ThumbnailView<'a> {
    pub state: ThumbnailState,
    pub is_visible: bool,
    pub is_focused: bool,
    pub character_name: &'a str,
    pub system_name: &'a str,
    /// Newest first.
    pub notifications: &'a [ActiveNotification],
    pub hide_thumbnail: bool,
    pub border_colors: Option<CharacterBorderColors>,
    pub character_color: Option<u32>,
    pub system_color: u32,
    pub opacity: u8,
    pub group_badge_label: &'a str,
    pub is_excluded_from_cycle: bool,
    pub overlay_width: i32,
    pub overlay_height: i32,
    pub dpi_scale: f32,
    pub metrics: Metrics,
}

/// One resolved (text, color) line of the stacked notification block.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationLine {
    pub text: String,
    pub color: u32,
}

/// Settings for rendering a thumbnail's overlay (text and border); compared field-for-field to skip redundant redraws.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSettings {
    pub show_text: bool,
    pub show_character_name: bool,
    pub character_name: String,
    pub display_name: String,
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
    /// Opacity of the live window image (the "thumbnail opacity" / per-character opacity).
    pub thumbnail_alpha: u8,
    /// Opacity of the text/border overlay: the thumbnail opacity when applyOpacityToOverlayTexts, else opaque.
    pub overlay_alpha: u8,

    pub overlay_width: i32,
    pub overlay_height: i32,
    pub dpi_scale: f32,
    pub metrics: Metrics,

    pub dps_incoming_color: u32,
    pub dps_outgoing_color: u32,
    pub mining_color: u32,
    pub bounty_color: u32,
    pub resources_color: u32,
}

impl RenderSettings {
    /// Every visual field equal, ignoring show_thumbnail.
    pub fn visual_eq(&self, other: &RenderSettings) -> bool {
        if self.show_thumbnail == other.show_thumbnail {
            return self == other;
        }
        let mut a = self.clone();
        a.show_thumbnail = other.show_thumbnail;
        &a == other
    }

    pub fn only_visibility_changed(&self, other: &RenderSettings) -> bool {
        self.show_thumbnail != other.show_thumbnail && self.visual_eq(other)
    }
}

/// Per-state override (if any) wins, then opacity is forced fully opaque when the window's own Opacity setting applies instead.
fn resolve_text_bg_color(state_cfg: &StateVisualConfig, base_color: u32, force_opaque: bool) -> u32 {
    let resolved = state_cfg.text_bg_color_or(base_color);
    if force_opaque { with_alpha(resolved, 255) } else { resolved }
}

/// The single point where a thumbnail's effective render state determines all of its visual properties.
pub fn create_render_settings(cfg: &Config, v: &ThumbnailView<'_>) -> RenderSettings {
    let state = v.state;
    let is_visible = v.is_visible;
    let dpi_scale = v.dpi_scale;
    let t = &cfg.thumbnail;
    let state_cfg = t.state_config(state);

    // Alert is treated like Active as a base; StateVisualConfig for Alert, per-type overrides, and per-character overrides layer on top.
    let is_alert_like = matches!(state, ThumbnailState::Active | ThumbnailState::Alert);
    let base_border_width = if is_alert_like { t.border_width } else { t.inactive_border_width };
    let base_border_color = if is_alert_like { t.border_color } else { t.inactive_border_color };
    let base_border_style = if is_alert_like { t.border_style } else { t.inactive_border_style };
    let base_show_border = if is_alert_like { t.show_border_when_focused } else { t.show_border_when_inactive };

    let char_hidden = v.hide_thumbnail;
    let base_show_thumbnail = if !is_visible { false } else if state == ThumbnailState::Active { !t.active_thumbnail_hidden } else { true };
    let should_hide_all = !is_visible || char_hidden || (state == ThumbnailState::Active && t.active_thumbnail_hidden);

    let notif_on_focused_char = v.is_focused;
    let newest = v.notifications.first();
    let notif_hides_border = state == ThumbnailState::Alert && !notif_on_focused_char && newest.is_some_and(|n| !n.show_border);
    let notif_flash_hides_border = state == ThumbnailState::Alert && !notif_on_focused_char && newest.is_some_and(|n| is_notification_flash_off(n, now_ms()));

    let effective_show_border = if should_hide_all || notif_hides_border || notif_flash_hides_border { false } else { state_cfg.show_border_or(base_show_border) };
    let effective_show_text = !should_hide_all && t.show_text;
    let effective_show_character_name = !should_hide_all && t.show_text && t.show_character_name;
    let effective_show_system_name = !should_hide_all && t.show_text && t.show_system_name && !v.system_name.is_empty();
    let effective_show_notifications = !should_hide_all && t.show_text && t.notifications.enabled;
    let effective_show_group_badge = !should_hide_all && t.show_text && t.show_quick_group_badge;

    let mut final_border_color = state_cfg.border_color_or(base_border_color);
    let is_suppressed_alert = state == ThumbnailState::Alert && newest.is_some_and(|n| n.suppress_when_focused && v.is_focused);
    if state == ThumbnailState::Alert && !is_suppressed_alert {
        if let Some(color) = newest.and_then(|n| n.border_color_override) {
            final_border_color = color;
        }
    }
    let notification_base_text_color = state_cfg.text_color_or(t.character_name_color);

    // Per-character border color has the highest precedence; a suppressed Alert is treated as Active for border purposes.
    if let Some(char_colors) = v.border_colors {
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

    let final_text_color = v.character_color.unwrap_or_else(|| state_cfg.text_color_or(t.character_name_color));

    let mut notification_lines = Vec::new();
    if effective_show_notifications {
        for notif in v.notifications {
            if notif.suppress_when_focused && v.is_focused {
                continue;
            }
            notification_lines.push(NotificationLine { text: notif.text.clone(), color: notif.text_color_override.unwrap_or(notification_base_text_color) });
        }
    }

    let opaque = t.apply_opacity_to_overlay_texts;
    let m = &v.metrics;
    RenderSettings {
        show_text: effective_show_text,
        show_character_name: effective_show_character_name,
        character_name: v.character_name.to_owned(),
        display_name: cfg.display_name(v.character_name).to_owned(),
        show_system_name: effective_show_system_name,
        system_name: v.system_name.to_owned(),
        character_name_color: final_text_color,
        system_name_color: v.system_color,
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
        show_exclusion_overlay: v.is_excluded_from_cycle && is_visible,
        exclusion_overlay_style: t.exclusion_overlay_style,
        exclusion_overlay_color: t.exclusion_overlay_color,
        show_group_badge: effective_show_group_badge && !v.group_badge_label.is_empty() && is_visible,
        group_badge_text: v.group_badge_label.to_owned(),
        group_badge_color: t.quick_group_badge_color,
        group_badge_position: t.quick_group_badge_position,
        group_badge_offset_x: t.quick_group_badge_offset_x,
        group_badge_offset_y: t.quick_group_badge_offset_y,
        group_badge_font_name: t.quick_group_badge_font_name.clone(),
        group_badge_font_size: scale_pixels(t.quick_group_badge_font_size, dpi_scale),
        group_badge_font_weight: t.quick_group_badge_font_weight,
        // visibility_state and per-character hide_thumbnail take absolute priority over per-state show_thumbnail config.
        show_thumbnail: if !is_visible || char_hidden { false } else { state_cfg.show_thumbnail_or(base_show_thumbnail) },
        thumbnail_alpha: v.opacity,
        overlay_alpha: if opaque { v.opacity } else { OVERLAY_ALPHA },
        overlay_width: v.overlay_width,
        overlay_height: v.overlay_height,
        dpi_scale,
        metrics: Metrics {
            incoming_dps: if cfg.combat.enabled { m.incoming_dps } else { None },
            outgoing_dps: if cfg.combat.enabled { m.outgoing_dps } else { None },
            mining_rate: if cfg.mining.enabled { m.mining_rate } else { None },
            mining_isk_rate: if cfg.mining.enabled && cfg.mining.show_isk_rate { m.mining_isk_rate } else { None },
            bounty_isk_rate: if cfg.bounty.enabled { m.bounty_isk_rate } else { None },
            has_resource_data: cfg.resources.enabled && m.has_resource_data,
            ..*m
        },
        dps_incoming_color: cfg.combat.incoming_color,
        dps_outgoing_color: cfg.combat.outgoing_color,
        mining_color: cfg.mining.color,
        bounty_color: cfg.bounty.color,
        resources_color: cfg.resources.color,
    }
}

// ------------------------------------------------------------------------------------------------ text layout --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextDimensions {
    pub width: usize,
    pub height: usize,
}

impl TextDimensions {
    /// Padded box for a run of text whose ink measures `text_width` x `text_height`.
    pub fn padded(text_width: usize, text_height: usize) -> Self {
        Self { width: text_width + TEXT_PADDING_X * 2, height: text_height + TEXT_PADDING_Y * 2 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextPos {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy)]
pub enum HorizontalAlign {
    Left,
    Center,
    Right,
}

pub fn horizontal_align_of(position: TextPosition) -> HorizontalAlign {
    use TextPosition::*;
    match position {
        TopLeft | LeftCenter | BottomLeft => HorizontalAlign::Left,
        TopCenter | Center | BottomCenter => HorizontalAlign::Center,
        TopRight | RightCenter | BottomRight => HorizontalAlign::Right,
    }
}

pub fn aligned_line_x(block_x: i32, block_width: usize, line_width: usize, alignment: HorizontalAlign) -> i32 {
    match alignment {
        HorizontalAlign::Left => block_x,
        HorizontalAlign::Center => block_x + (block_width.saturating_sub(line_width) / 2) as i32,
        HorizontalAlign::Right => block_x + block_width.saturating_sub(line_width) as i32,
    }
}

#[derive(Clone, Copy)]
pub enum VerticalAlign {
    Top,
    Middle,
    Bottom,
}

pub fn vertical_align_of(position: TextPosition) -> VerticalAlign {
    use TextPosition::*;
    match position {
        TopLeft | TopCenter | TopRight => VerticalAlign::Top,
        LeftCenter | Center | RightCenter => VerticalAlign::Middle,
        BottomLeft | BottomCenter | BottomRight => VerticalAlign::Bottom,
    }
}

pub fn aligned_line_y(block_y: i32, block_height: usize, line_height: usize, alignment: VerticalAlign) -> i32 {
    match alignment {
        VerticalAlign::Top => block_y,
        VerticalAlign::Middle => block_y + (block_height.saturating_sub(line_height) / 2) as i32,
        VerticalAlign::Bottom => block_y + block_height.saturating_sub(line_height) as i32,
    }
}

pub fn calculate_text_position(position: TextPosition, dims: TextDimensions, overlay_width: usize, overlay_height: usize, offset_x: i32, offset_y: i32) -> TextPos {
    let mut x = aligned_line_x(0, overlay_width, dims.width, horizontal_align_of(position)) + offset_x;
    let mut y = aligned_line_y(0, overlay_height, dims.height, vertical_align_of(position)) + offset_y;
    x = x.min(overlay_width as i32 - dims.width as i32).max(0);
    y = y.min(overlay_height as i32 - dims.height as i32).max(0);
    TextPos { x, y }
}

// --------------------------------------------------------------------------------------------- pixel drawing --

/// Pre-multiplies color by its alpha, for writing straight into a premultiplied buffer.
pub fn premultiply_alpha(color: u32) -> u32 {
    let a = (color >> 24) & 0xFF;
    if a == 255 {
        return color;
    }
    let r = ((color >> 16) & 0xFF) * a / 255;
    let g = ((color >> 8) & 0xFF) * a / 255;
    let b = (color & 0xFF) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Clipped solid rectangle fill.
#[allow(clippy::too_many_arguments)]
pub fn fill_rect(pixels: &mut [u32], width: usize, height: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    let x_end = (x + w).min(width);
    let y_end = (y + h).min(height);
    if x >= x_end {
        return;
    }
    for row in y.min(height)..y_end {
        pixels[row * width + x..row * width + x_end].fill(color);
    }
}

/// Fills a text run's background rectangle (premultiplied).
#[allow(clippy::too_many_arguments)]
pub fn fill_text_background(pixels: &mut [u32], width: usize, height: usize, x: i32, y: i32, text_width: usize, bar_height: usize, color: u32) {
    let start_x = x.max(0) as usize;
    let start_y = y.max(0) as usize;
    let end_y = (start_y + bar_height).min(height);
    let end_x = (start_x + text_width).min(width);
    if end_x <= start_x || end_y <= start_y {
        return;
    }
    let blended = premultiply_alpha(color);
    for py in start_y..end_y {
        pixels[py * width + start_x..py * width + end_x].fill(blended);
    }
}

fn draw_diagonal_band(pixels: &mut [u32], width: usize, height: usize, color: u32, is_diag2: bool) {
    let iw = width as i32;
    let ih = height as i32;
    if ih == 0 {
        return;
    }
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
pub fn draw_exclusion_overlay(pixels: &mut [u32], width: usize, height: usize, color: u32, style: ExclusionOverlayStyle) {
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
            let slash_reach = radius * 1.15;
            let sqrt2 = 2f32.sqrt();
            for y in 0..height {
                let dy = y as f32 - cy;
                for x in 0..width {
                    let dx = x as f32 - cx;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let on_ring = (dist - radius).abs() <= thickness / 2.0;
                    let on_slash = (dx + dy).abs() / sqrt2 <= thickness / 2.0 && dist <= slash_reach;
                    if on_ring || on_slash {
                        pixels[y * width + x] = blended;
                    }
                }
            }
        }
        ExclusionOverlayStyle::DiagonalHatch => {
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

fn border_regions(width: usize, height: usize, bw: usize) -> [BorderRegion; 4] {
    [
        BorderRegion { x_start: 0, y_start: 0, x_end: width, y_end: bw },
        BorderRegion { x_start: 0, y_start: height - bw, x_end: width, y_end: height },
        BorderRegion { x_start: 0, y_start: 0, x_end: bw, y_end: height },
        BorderRegion { x_start: width - bw, y_start: 0, x_end: width, y_end: height },
    ]
}

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

/// Draws the thumbnail border in `style`; `color` is written as-is (borders are normally opaque).
pub fn draw_border(pixels: &mut [u32], width: usize, height: usize, bw: usize, color: u32, style: BorderStyle) {
    if bw == 0 || bw > width || bw > height {
        return;
    }
    let color = premultiply_alpha(color);
    match style {
        BorderStyle::Solid => {
            pixels[..bw * width].fill(color);
            pixels[(height - bw) * width..height * width].fill(color);
            for y in bw..height - bw {
                let row = y * width;
                pixels[row..row + bw].fill(color);
                pixels[row + width - bw..row + width].fill(color);
            }
        }
        BorderStyle::Dashed => draw_lengthwise_pattern(pixels, width, height, bw, color, 8, 4),
        BorderStyle::Dotted => {
            let dot = bw.max(1);
            draw_lengthwise_pattern(pixels, width, height, bw, color, dot, dot * 2);
        }
        BorderStyle::Double => {
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
            let arm = (bw * 4).min(width.min(height) / 3);
            let f = |p: &mut [u32], x, y, w, h| fill_rect(p, width, height, x, y, w, h, color);
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

// --------------------------------------------------------------------------------------------- number formats --

/// Inserts comma thousands-separators into the leading run of ASCII digits ("12405.3 m3/min" -> "12,405.3 m3/min").
pub fn insert_thousands_separators(text: &str) -> String {
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

/// Abbreviates an ISK value with k/m suffixes (2_450_000 -> "2.5m", 200_000 -> "200k", 850 -> "850").
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

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(state: ThumbnailState) -> ThumbnailView<'a> {
        ThumbnailView {
            state,
            is_visible: true,
            is_focused: state == ThumbnailState::Active,
            character_name: "Pilot",
            system_name: "",
            notifications: &[],
            hide_thumbnail: false,
            border_colors: None,
            character_color: None,
            system_color: 0xFFFFFFFF,
            opacity: 200,
            group_badge_label: "",
            is_excluded_from_cycle: false,
            overlay_width: 320,
            overlay_height: 180,
            dpi_scale: 1.0,
            metrics: Metrics::default(),
        }
    }

    #[test]
    fn active_and_inactive_use_their_own_border_settings() {
        let cfg = Config::defaults_with_profile("default.json").unwrap();
        let active = create_render_settings(&cfg, &view(ThumbnailState::Active));
        let inactive = create_render_settings(&cfg, &view(ThumbnailState::Inactive));
        assert_eq!(active.border_color, cfg.thumbnail.state_config(ThumbnailState::Active).border_color_or(cfg.thumbnail.border_color));
        assert_eq!(inactive.border_width, cfg.thumbnail.state_config(ThumbnailState::Inactive).border_width_or(cfg.thumbnail.inactive_border_width));
        assert!(active.show_thumbnail);
    }

    #[test]
    fn hidden_character_hides_everything() {
        let cfg = Config::defaults_with_profile("default.json").unwrap();
        let mut v = view(ThumbnailState::Inactive);
        v.hide_thumbnail = true;
        let s = create_render_settings(&cfg, &v);
        assert!(!s.show_thumbnail && !s.show_border && !s.show_character_name);
    }

    #[test]
    fn border_styles_stay_in_bounds() {
        for style in [BorderStyle::Solid, BorderStyle::Dashed, BorderStyle::Dotted, BorderStyle::Double, BorderStyle::DiagonalHatch, BorderStyle::DashDot, BorderStyle::CornerBrackets] {
            let mut px = vec![0u32; 40 * 30];
            draw_border(&mut px, 40, 30, 3, 0xFFFF0000, style);
            assert!(px.iter().any(|&p| p == 0xFFFF0000));
            assert_eq!(px[15 * 40 + 20], 0, "{style:?} drew into the middle");
        }
    }

    #[test]
    fn text_position_clamps_inside_overlay() {
        let p = calculate_text_position(TextPosition::BottomRight, TextDimensions { width: 50, height: 20 }, 100, 60, 30, 30);
        assert_eq!(p, TextPos { x: 50, y: 40 });
    }

    #[test]
    fn thousands_and_isk() {
        assert_eq!(insert_thousands_separators("12405.3 m3/min"), "12,405.3 m3/min");
        assert_eq!(insert_thousands_separators("1234567"), "1,234,567");
        assert_eq!(format_isk_abbrev(2_450_000.0), "2.5m");
        assert_eq!(format_isk_abbrev(200_000.0), "200k");
    }
}
