//! Paints a thumbnail's text/border overlay into a premultiplied ARGB32 image with cairo + pango.
//!
//! Mirrors the Windows painter's `render_thumbnail_overlay` step for step (exclusion pattern, text backgrounds,
//! border, then text, notifications last), using core's shared pixel helpers for everything that isn't text.

use cairo::{Format, ImageSurface};
use eve_maj_core::config::Config;
use eve_maj_core::config::IskRateUnit;
use eve_maj_core::log::Scope;
use eve_maj_core::render::*;
use eve_maj_core::types::{FontWeight, TextPosition};

const SLOG: Scope = Scope::new("overlay");

static FONT_CASCADIA_CODE: &[u8] = include_bytes!("../../../../assets/fonts/CascadiaCode-Regular.ttf");
static FONT_CASCADIA_CODE_SEMIBOLD: &[u8] = include_bytes!("../../../../assets/fonts/CascadiaCode-SemiBold.ttf");
static FONT_CASCADIA_MONO: &[u8] = include_bytes!("../../../../assets/fonts/CascadiaMono-Regular.ttf");

#[link(name = "fontconfig")]
extern "C" {
    fn FcConfigAppFontAddFile(config: *mut std::ffi::c_void, file: *const u8) -> i32;
}

/// Makes the bundled Cascadia fonts available to pango (fontconfig) without installing them, like the Windows build's process-private AddFontMemResourceEx.
pub fn register_bundled_fonts(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    for (name, bytes) in [("CascadiaCode-Regular.ttf", FONT_CASCADIA_CODE), ("CascadiaCode-SemiBold.ttf", FONT_CASCADIA_CODE_SEMIBOLD), ("CascadiaMono-Regular.ttf", FONT_CASCADIA_MONO)] {
        let path = dir.join(name);
        if std::fs::metadata(&path).map(|m| m.len() as usize != bytes.len()).unwrap_or(true) {
            if let Err(e) = std::fs::write(&path, bytes) {
                SLOG.warn(format_args!("Can't write bundled font {}: {e}", path.display()));
                continue;
            }
        }
        let c_path = format!("{}\0", path.display());
        if unsafe { FcConfigAppFontAddFile(std::ptr::null_mut(), c_path.as_ptr()) } == 0 {
            SLOG.warn(format_args!("fontconfig refused {}", path.display()));
        }
    }
}

fn font_description(name: &str, size: i32, weight: FontWeight) -> pango::FontDescription {
    let mut d = pango::FontDescription::new();
    d.set_family(name);
    d.set_absolute_size((size.max(1) * pango::SCALE) as f64);
    d.set_weight(if matches!(weight, FontWeight::Bold | FontWeight::BoldItalic) { pango::Weight::Bold } else { pango::Weight::Normal });
    d.set_style(if weight.is_italic() { pango::Style::Italic } else { pango::Style::Normal });
    d
}

struct Text {
    layout: pango::Layout,
    dims: TextDimensions,
}

fn measure(cr: &cairo::Context, text: &str, font: &pango::FontDescription) -> Text {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(font));
    layout.set_text(text);
    let (w, h) = layout.pixel_size();
    Text { dims: TextDimensions::padded(w.max(0) as usize, h.max(0) as usize), layout }
}

struct DrawLine {
    layout: pango::Layout,
    render_pos: TextPos,
    bg_pos: TextPos,
    bg_dims: TextDimensions,
    color: u32,
    bg_color: u32,
}

fn rate_unit(unit: IskRateUnit) -> (f32, &'static str) {
    match unit {
        IskRateUnit::Hour => (3600.0, "hr"),
        IskRateUnit::Minute => (60.0, "min"),
    }
}

fn set_rgb(cr: &cairo::Context, color: u32) {
    cr.set_source_rgb(((color >> 16) & 0xFF) as f64 / 255.0, ((color >> 8) & 0xFF) as f64 / 255.0, (color & 0xFF) as f64 / 255.0);
}

/// Paints the overlay for `s`; None if cairo can't allocate the surface.
pub fn paint(config: &Config, s: &RenderSettings) -> Option<ImageSurface> {
    let (ow, oh) = (s.overlay_width.max(1) as usize, s.overlay_height.max(1) as usize);
    let mut surface = ImageSurface::create(Format::ARgb32, ow as i32, oh as i32).ok()?;
    let cr = cairo::Context::new(&surface).ok()?;

    let pos = |position: TextPosition, dims: TextDimensions, ox: i32, oy: i32| calculate_text_position(position, dims, ow, oh, ox, oy);
    let single = |t: Text, p: TextPos, color: u32, bg: u32| DrawLine { layout: t.layout, render_pos: p, bg_pos: p, bg_dims: t.dims, color, bg_color: bg };
    let mut lines: Vec<DrawLine> = Vec::with_capacity(11);

    if s.show_character_name {
        let t = measure(&cr, &s.display_name, &font_description(&s.character_name_font_name, s.character_name_font_size, s.character_name_font_weight));
        let p = pos(s.character_name_position, t.dims, s.character_name_offset_x, s.character_name_offset_y);
        lines.push(single(t, p, s.character_name_color, s.character_name_bg_color));
    }
    if s.show_system_name {
        let t = measure(&cr, &s.system_name, &font_description(&s.system_name_font_name, s.system_name_font_size, s.system_name_font_weight));
        let p = pos(s.system_name_position, t.dims, s.system_name_offset_x, s.system_name_offset_y);
        lines.push(single(t, p, s.system_name_color, s.system_name_bg_color));
    }
    if s.show_group_badge {
        let t = measure(&cr, &s.group_badge_text, &font_description(&s.group_badge_font_name, s.group_badge_font_size, s.group_badge_font_weight));
        let p = pos(s.group_badge_position, t.dims, s.group_badge_offset_x, s.group_badge_offset_y);
        lines.push(single(t, p, s.group_badge_color, s.group_badge_bg_color));
    }

    // Combat / mining / bounty / resources, gated on show_text like the Windows build.
    let m = &s.metrics;
    let scaled = |size: i32| eve_maj_core::layout::scale_pixels(size, s.dpi_scale);
    let show_text = config.thumbnail.show_text && s.show_text;
    let combat = &config.combat;
    if combat.enabled && show_text && combat.show_incoming && m.has_dps_data && m.incoming_dps.is_none_or(|d| d > 0.0) {
        let text = match m.incoming_dps {
            Some(d) if combat.incoming_show_prefix => format!("IN: {d:.0}"),
            Some(d) => format!("{d:.0}"),
            None if combat.incoming_show_prefix => "IN: ??".to_owned(),
            None => "??".to_owned(),
        };
        let t = measure(&cr, &text, &font_description(&combat.incoming_font_name, scaled(combat.incoming_font_size), combat.incoming_font_weight));
        let p = pos(combat.incoming_position, t.dims, combat.incoming_offset_x, combat.incoming_offset_y);
        lines.push(single(t, p, combat.incoming_color, s.combat_incoming_bg_color));
    }
    if combat.enabled && show_text && combat.show_outgoing && m.has_dps_data && m.outgoing_dps.is_none_or(|d| d > 0.0) {
        let text = match m.outgoing_dps {
            Some(d) if combat.outgoing_show_prefix => format!("OUT: {d:.0}"),
            Some(d) => format!("{d:.0}"),
            None if combat.outgoing_show_prefix => "OUT: ??".to_owned(),
            None => "??".to_owned(),
        };
        let t = measure(&cr, &text, &font_description(&combat.outgoing_font_name, scaled(combat.outgoing_font_size), combat.outgoing_font_weight));
        let p = pos(combat.outgoing_position, t.dims, combat.outgoing_offset_x, combat.outgoing_offset_y);
        lines.push(single(t, p, combat.outgoing_color, s.combat_outgoing_bg_color));
    }
    let mining = &config.mining;
    if mining.enabled && show_text && m.has_mining_data && m.mining_rate.is_none_or(|r| r > 0.0) {
        let font = font_description(&mining.font_name, scaled(mining.font_size), mining.font_weight);
        let mining_text = match m.mining_rate {
            Some(rate) => {
                let per_min = rate * 60.0;
                let raw = if per_min < 10.0 { format!("{per_min:.1}") } else { format!("{per_min:.0}") };
                let formatted = insert_thousands_separators(&raw);
                if mining.show_prefix { format!("M: {formatted} m3/min") } else { format!("{formatted} m3/min") }
            }
            None if mining.show_prefix => "M: ?? m3/min".to_owned(),
            None => "?? m3/min".to_owned(),
        };
        let first = measure(&cr, &mining_text, &font);
        let second = mining.show_isk_rate.then(|| {
            let (period, suffix) = rate_unit(mining.isk_rate_unit);
            let text = match m.mining_isk_rate {
                Some(r) => format!("{} ISK/{suffix}", format_isk_abbrev(r * period)),
                None => format!("?? ISK/{suffix}"),
            };
            measure(&cr, &text, &font)
        });
        let block_width = first.dims.width.max(second.as_ref().map_or(0, |t| t.dims.width));
        let block_height = first.dims.height + second.as_ref().map_or(0, |t| t.dims.height);
        let anchor = pos(mining.position, TextDimensions { width: block_width, height: block_height }, mining.offset_x, mining.offset_y);
        let h_align = horizontal_align_of(mining.position);
        let mut y = anchor.y;
        for t in std::iter::once(first).chain(second) {
            let x = aligned_line_x(anchor.x, block_width, t.dims.width, h_align);
            let h = t.dims.height;
            lines.push(DrawLine { layout: t.layout, render_pos: TextPos { x, y }, bg_pos: TextPos { x: anchor.x, y }, bg_dims: TextDimensions { width: block_width, height: h }, color: mining.color, bg_color: s.mining_bg_color });
            y += h as i32;
        }
    }
    let bounty = &config.bounty;
    if bounty.enabled && show_text && m.has_bounty_data && m.bounty_isk_rate.is_none_or(|r| r > 0.0) {
        let (period, suffix) = rate_unit(bounty.isk_rate_unit);
        let text = match m.bounty_isk_rate {
            Some(r) if bounty.show_prefix => format!("ISK: {} ISK/{suffix}", format_isk_abbrev(r * period)),
            Some(r) => format!("{} ISK/{suffix}", format_isk_abbrev(r * period)),
            None if bounty.show_prefix => format!("ISK: ?? ISK/{suffix}"),
            None => format!("?? ISK/{suffix}"),
        };
        let t = measure(&cr, &text, &font_description(&bounty.font_name, scaled(bounty.font_size), bounty.font_weight));
        let p = pos(bounty.position, t.dims, bounty.offset_x, bounty.offset_y);
        lines.push(single(t, p, bounty.color, s.bounty_bg_color));
    }
    let res = &config.resources;
    if res.enabled && show_text && m.has_resource_data {
        let font = font_description(&res.font_name, scaled(res.font_size), res.font_weight);
        let mut texts = Vec::new();
        if res.show_cpu {
            texts.push(format!("CPU: {:.0}%", m.cpu_percent));
        }
        if res.show_ram {
            texts.push(format!("RAM: {:.0}MB", m.ram_mb));
        }
        if res.show_vram && m.has_vram_data {
            texts.push(format!("VRAM: {:.0}MB", m.vram_mb));
        }
        let measured: Vec<Text> = texts.iter().map(|t| measure(&cr, t, &font)).collect();
        if !measured.is_empty() {
            let block_width = measured.iter().map(|t| t.dims.width).max().unwrap_or(0);
            let combined: usize = measured.iter().map(|t| t.dims.height).sum();
            let anchor = pos(res.position, TextDimensions { width: block_width, height: combined }, res.offset_x, res.offset_y);
            let h_align = horizontal_align_of(res.position);
            let mut y = anchor.y;
            for t in measured {
                let x = aligned_line_x(anchor.x, block_width, t.dims.width, h_align);
                let h = t.dims.height;
                lines.push(DrawLine { layout: t.layout, render_pos: TextPos { x, y }, bg_pos: TextPos { x: anchor.x, y }, bg_dims: TextDimensions { width: block_width, height: h }, color: res.color, bg_color: s.resources_bg_color });
                y += h as i32;
            }
        }
    }

    // Stacked notifications: one shared background block, each line in its own color.
    let mut notif_lines: Vec<(Text, u32)> = Vec::new();
    let mut notif_dims = TextDimensions::default();
    if s.show_notifications && !s.notification_lines.is_empty() {
        let font = font_description(&s.notifications_font_name, s.notifications_font_size, s.notifications_font_weight);
        for line in &s.notification_lines {
            let t = measure(&cr, &line.text, &font);
            notif_dims.width = notif_dims.width.max(t.dims.width);
            notif_dims.height += t.dims.height;
            notif_lines.push((t, line.color));
        }
    }
    let notif_pos = pos(s.notifications_position, notif_dims, s.notifications_offset_x, s.notifications_offset_y);

    // Pixel passes: exclusion pattern, text backgrounds, notification background, border.
    drop(cr);
    surface.flush();
    {
        let stride = surface.stride() as usize;
        let Ok(mut data) = surface.data() else {
            SLOG.err(format_args!("cairo surface data unavailable"));
            return None;
        };
        debug_assert_eq!(stride, ow * 4);
        let pixels: &mut [u32] = unsafe { std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u32, ow * oh) };
        pixels.fill(0);
        if s.show_exclusion_overlay {
            draw_exclusion_overlay(pixels, ow, oh, s.exclusion_overlay_color, s.exclusion_overlay_style);
        }
        for l in &lines {
            fill_text_background(pixels, ow, oh, l.bg_pos.x, l.bg_pos.y, l.bg_dims.width, l.bg_dims.height, l.bg_color);
        }
        if !notif_lines.is_empty() {
            fill_text_background(pixels, ow, oh, notif_pos.x, notif_pos.y, notif_dims.width, notif_dims.height, s.notifications_bg_color);
        }
        if s.show_border {
            draw_border(pixels, ow, oh, s.border_width as usize, s.border_color, s.border_style);
        }
    }
    surface.mark_dirty();

    // Text passes.
    let cr = cairo::Context::new(&surface).ok()?;
    for l in &lines {
        set_rgb(&cr, l.color);
        cr.move_to((l.render_pos.x + TEXT_PADDING_X as i32) as f64, (l.render_pos.y + TEXT_PADDING_Y as i32) as f64);
        pangocairo::functions::update_layout(&cr, &l.layout);
        pangocairo::functions::show_layout(&cr, &l.layout);
    }
    let mut y = notif_pos.y;
    for (t, color) in &notif_lines {
        set_rgb(&cr, *color);
        cr.move_to((notif_pos.x + TEXT_PADDING_X as i32) as f64, (y + TEXT_PADDING_Y as i32) as f64);
        pangocairo::functions::update_layout(&cr, &t.layout);
        pangocairo::functions::show_layout(&cr, &t.layout);
        y += t.dims.height as i32;
    }
    drop(cr);
    surface.flush();
    Some(surface)
}
