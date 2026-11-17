//! The settings dialog's "drag to select a region" overlay on Linux: one dimmed full-screen layer surface per monitor
//! where a rectangle is drawn, then moved or resized by its edges, and confirmed with Save/Enter or dropped with
//! Cancel/Esc/right-click. The result goes back to the dialog through a small file
//! ([`protocol::region_select_result_path`]), which its `pollRegionSelectResult` picks up, in place of the Windows
//! build's shared-memory mapping.
//!
//! Drawing is plain cairo into wl_shm buffers; a surface is repainted only when what it shows changed.

use eve_maj_core::log::Scope;
use eve_maj_core::protocol::{self, label_text, Rect, RegionSelectRequest, RegionSelectResult};
use eve_maj_core::render::now_ms;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Proxy, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1}};

use super::shm::alloc_buffer;
use super::App;

const SLOG: Scope = Scope::new("region-select");
const NAMESPACE: &str = "eve-maj-region-select";
/// Surface ids from here up are region-select surfaces (thumbnail ids count up from 1).
pub(super) const SURFACE_ID_BASE: u64 = 1 << 62;
/// The dialog gives up polling after two minutes; the overlay goes away then too.
const TIMEOUT_MS: u64 = 120_000;
/// How far from an edge (logical pixels) a press still grabs it.
const EDGE_GRAB: i32 = 8;
/// A drag shorter than this is a click, not a new rectangle.
const MIN_DRAG: i32 = 4;
const HANDLE: f64 = 8.0;
const BUTTON_PAD_X: f64 = 16.0;
const BUTTON_H: i32 = 32;
const BUTTON_GAP: i32 = 8;
const FONT: &str = "Sans";
const FONT_PX: i32 = 15;
const ACCENT: (f64, f64, f64) = (0.24, 0.68, 0.91);

// Standard evdev key codes, for when the keymap couldn't be read.
const KEY_ESC: u32 = 1;
const KEY_ENTER: u32 = 28;
const KEY_KPENTER: u32 = 96;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Save,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Edges {
    left: bool,
    top: bool,
    right: bool,
    bottom: bool,
}

/// What a press at a point would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Button(Button),
    Resize(Edges),
    Move,
    Draw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Drawing { anchor: (i32, i32), previous: Option<Rect> },
    Moving { grab: (i32, i32), orig: Rect },
    Resizing { edges: Edges, grab: (i32, i32), orig: Rect },
    Pressing(Button),
}

/// What one surface shows; it's repainted only when this changes.
#[derive(Debug, Clone, PartialEq)]
struct Scene {
    rect: Option<Rect>,
    hover: Option<Button>,
    buttons_here: bool,
    has_rect: bool,
}

struct OverlaySurface {
    id: u64,
    wl: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// The monitor's bounds in layout space.
    bounds: Rect,
    size: (i32, i32),
    configured: bool,
    frame_pending: bool,
    shown: Option<Scene>,
}

pub(super) struct RegionSelect {
    request: RegionSelectRequest,
    surfaces: Vec<OverlaySurface>,
    rect: Option<Rect>,
    action: Action,
    /// Pointer position in layout space, and the overlay surface it was last reported on.
    pointer: (i32, i32),
    pointer_surface: Option<usize>,
    hover: Option<Button>,
    enter_serial: u32,
    started_ms: u64,
    save_label: String,
    cancel_label: String,
    hint_new: String,
    hint_edit: String,
    hint_confirm: String,
    /// Button widths, measured once (labels don't change).
    save_w: i32,
    cancel_w: i32,
}

impl RegionSelect {
    pub(super) fn hides_thumbnails(&self) -> bool {
        self.request.hide_thumbnails
    }

    /// Save and Cancel, right-aligned under the rectangle, or inside its bottom edge when the monitor ends there.
    fn button_rects(&self, monitor: Option<Rect>) -> Option<(Rect, Rect)> {
        let r = self.rect?;
        let width = self.save_w + BUTTON_GAP + self.cancel_w;
        let below = r.bottom + BUTTON_GAP;
        let top = match monitor {
            Some(m) if below + BUTTON_H > m.bottom => (r.bottom - BUTTON_GAP - BUTTON_H).max(r.top),
            _ => below,
        };
        let mut left = r.right - width;
        if let Some(m) = monitor {
            left = left.clamp(m.left, (m.right - width).max(m.left));
        }
        let save = Rect { left, top, right: left + self.save_w, bottom: top + BUTTON_H };
        let cancel = Rect { left: save.right + BUTTON_GAP, top, right: save.right + BUTTON_GAP + self.cancel_w, bottom: top + BUTTON_H };
        Some((save, cancel))
    }

    fn buttons_monitor(&self) -> Option<Rect> {
        let r = self.rect?;
        let (x, y) = (r.right - 1, r.bottom - 1);
        let inside = |b: &Rect| x >= b.left && x < b.right && y >= b.top && y < b.bottom;
        let bounds: Vec<Rect> = self.surfaces.iter().map(|s| s.bounds).collect();
        bounds.iter().copied().find(inside).or_else(|| {
            bounds.iter().copied().min_by_key(|b| {
                let dx = (b.left - x).max(0).max(x - b.right) as i64;
                let dy = (b.top - y).max(0).max(y - b.bottom) as i64;
                dx * dx + dy * dy
            })
        })
    }

    fn hit(&self, (x, y): (i32, i32)) -> Hit {
        if let Some((save, cancel)) = self.button_rects(self.buttons_monitor()) {
            if contains(save, x, y) {
                return Hit::Button(Button::Save);
            }
            if contains(cancel, x, y) {
                return Hit::Button(Button::Cancel);
            }
        }
        let Some(r) = self.rect else { return Hit::Draw };
        hit_rect(r, x, y)
    }

    fn scene_for(&self, s: &OverlaySurface, buttons: Option<(Rect, Rect)>) -> Scene {
        // Text and handles reach a little past the rectangle, so nearby monitors count it too.
        let reach = |r: Rect| Rect { left: r.left - 64, top: r.top - 64, right: r.right + 64, bottom: r.bottom + 64 };
        let rect = self.rect.filter(|r| intersects(reach(*r), s.bounds));
        let buttons_here = buttons.is_some_and(|(a, b)| intersects(a, s.bounds) || intersects(b, s.bounds));
        Scene { rect, hover: if buttons_here { self.hover } else { None }, buttons_here, has_rect: self.rect.is_some() }
    }
}

fn contains(r: Rect, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn normalized((x0, y0): (i32, i32), (x1, y1): (i32, i32)) -> Rect {
    Rect { left: x0.min(x1), top: y0.min(y1), right: x0.max(x1), bottom: y0.max(y1) }
}

fn hit_rect(r: Rect, x: i32, y: i32) -> Hit {
    let in_x = x >= r.left - EDGE_GRAB && x <= r.right + EDGE_GRAB;
    let in_y = y >= r.top - EDGE_GRAB && y <= r.bottom + EDGE_GRAB;
    let edges = Edges {
        left: in_y && (x - r.left).abs() <= EDGE_GRAB,
        right: in_y && (x - r.right).abs() <= EDGE_GRAB,
        top: in_x && (y - r.top).abs() <= EDGE_GRAB,
        bottom: in_x && (y - r.bottom).abs() <= EDGE_GRAB,
    };
    // A rectangle thinner than the grab zone: prefer the far edges so it can still grow.
    let edges = Edges { left: edges.left && !edges.right, top: edges.top && !edges.bottom, ..edges };
    if edges != Edges::default() {
        Hit::Resize(edges)
    } else if contains(r, x, y) {
        Hit::Move
    } else {
        Hit::Draw
    }
}

/// `orig` with the grabbed edges moved by the pointer's travel; it flips over rather than inverting.
fn resized(orig: Rect, edges: Edges, (dx, dy): (i32, i32)) -> Rect {
    let mut r = orig;
    if edges.left {
        r.left += dx;
    }
    if edges.right {
        r.right += dx;
    }
    if edges.top {
        r.top += dy;
    }
    if edges.bottom {
        r.bottom += dy;
    }
    normalized((r.left, r.top), (r.right, r.bottom))
}

fn cursor_for(hit: Hit) -> Shape {
    match hit {
        Hit::Button(_) => Shape::Pointer,
        Hit::Move => Shape::Move,
        Hit::Draw => Shape::Crosshair,
        Hit::Resize(e) => match (e.left, e.top, e.right, e.bottom) {
            (true, true, _, _) | (_, _, true, true) => Shape::NwseResize,
            (true, _, _, true) | (_, true, true, _) => Shape::NeswResize,
            (true, _, _, _) | (_, _, true, _) => Shape::EwResize,
            _ => Shape::NsResize,
        },
    }
}

fn text_layout(cr: &cairo::Context, text: &str, bold: bool) -> pango::Layout {
    let mut font = pango::FontDescription::new();
    font.set_family(FONT);
    font.set_absolute_size((FONT_PX * pango::SCALE) as f64);
    if bold {
        font.set_weight(pango::Weight::Bold);
    }
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&font));
    layout.set_text(text);
    layout
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
}

impl App {
    /// The dialog's "Drag Select Region": covers every monitor with the selection overlay.
    pub(super) fn start_region_select(&mut self, request: RegionSelectRequest) {
        // The overlay takes all pointer input, so a panel drag in progress would never see its release.
        self.end_panel_drag();
        if self.region.is_some() {
            // No result for the old one: the dialog already cleared the result file for this request.
            SLOG.info(format_args!("Region selection restarted"));
            self.close_region_overlay();
        }
        let text = |b: &[u8]| String::from_utf8_lossy(label_text(b)).into_owned();
        let (save_label, cancel_label) = (text(&request.labels.save), text(&request.labels.cancel));
        let (save_w, cancel_w) = match cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).ok().and_then(|s| cairo::Context::new(&s).ok()) {
            Some(cr) => {
                let w = |t: &str| text_layout(&cr, t, true).pixel_size().0 + 2 * BUTTON_PAD_X as i32;
                (w(&save_label), w(&cancel_label))
            }
            None => (90, 90),
        };
        let mut state = RegionSelect {
            request,
            surfaces: Vec::new(),
            rect: request.edit_region,
            action: Action::None,
            pointer: (0, 0),
            pointer_surface: None,
            hover: None,
            enter_serial: 0,
            started_ms: now_ms(),
            save_label,
            cancel_label,
            hint_new: text(&request.labels.hint_new),
            hint_edit: text(&request.labels.hint_edit),
            hint_confirm: text(&request.labels.hint_confirm),
            save_w,
            cancel_w,
        };
        for (n, monitor) in self.monitors.iter().enumerate() {
            let id = SURFACE_ID_BASE + n as u64;
            let Some(output) = self.outputs.iter().find(|o| o.name == monitor.name).map(|o| o.wl.clone()) else { continue };
            let wl = self.compositor.create_surface(&self.qh, id);
            let layer = self.layer_shell.get_layer_surface(&wl, Some(&output), zwlr_layer_shell_v1::Layer::Overlay, NAMESPACE.to_owned(), &self.qh, id);
            use zwlr_layer_surface_v1::Anchor;
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_size(0, 0);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
            wl.commit();
            state.surfaces.push(OverlaySurface {
                id,
                wl,
                layer,
                bounds: Rect { left: monitor.bounds.left, top: monitor.bounds.top, right: monitor.bounds.right, bottom: monitor.bounds.bottom },
                size: (monitor.bounds.right - monitor.bounds.left, monitor.bounds.bottom - monitor.bounds.top),
                configured: false,
                frame_pending: false,
                shown: None,
            });
        }
        if state.surfaces.is_empty() {
            SLOG.warn(format_args!("No monitors to select a region on"));
            write_result(RegionSelectResult::cancelled());
            return;
        }
        SLOG.info(format_args!("Region selection started ({}, hide thumbnails: {})", if state.rect.is_some() { "edit" } else { "new" }, state.request.hide_thumbnails));
        let hide = state.request.hide_thumbnails;
        self.region = Some(state);
        if hide {
            for t in &mut self.thumbs {
                t.needs_render = true;
            }
            self.render_dirty();
        }
    }

    pub(super) fn cancel_region_select(&mut self) {
        self.finish_region_select(RegionSelectResult::cancelled());
    }

    fn finish_region_select(&mut self, result: RegionSelectResult) {
        if !self.close_region_overlay() {
            return;
        }
        SLOG.info(format_args!("Region selection finished: {:?} {}x{} at ({}, {})", result.status, result.width, result.height, result.x, result.y));
        write_result(result);
    }

    /// Takes the overlay down and brings back thumbnails it hid; false if none was up.
    fn close_region_overlay(&mut self) -> bool {
        let Some(state) = self.region.take() else { return false };
        for s in state.surfaces {
            s.layer.destroy();
            s.wl.destroy();
        }
        if state.request.hide_thumbnails {
            for t in &mut self.thumbs {
                t.needs_render = true;
            }
            self.render_dirty();
        }
        true
    }

    pub(super) fn region_select_tick(&mut self) {
        if self.region.as_ref().is_some_and(|r| now_ms().saturating_sub(r.started_ms) > TIMEOUT_MS) {
            SLOG.info(format_args!("Region selection timed out"));
            self.finish_region_select(RegionSelectResult::cancelled());
        }
    }

    fn confirm_region(&mut self) {
        let Some(rect) = self.region.as_ref().and_then(|r| r.rect) else { return };
        self.finish_region_select(RegionSelectResult::confirmed(rect));
    }

    pub(super) fn region_layer_event(&mut self, id: u64, event: zwlr_layer_surface_v1::Event) {
        let Some(state) = self.region.as_mut() else { return };
        let Some(s) = state.surfaces.iter_mut().find(|s| s.id == id) else { return };
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                s.layer.ack_configure(serial);
                if width > 0 && height > 0 {
                    s.size = (width as i32, height as i32);
                }
                s.configured = true;
                s.shown = None;
                self.region_redraw();
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // The compositor dropped a monitor's overlay (output gone); without it the selection can't finish cleanly.
                self.finish_region_select(RegionSelectResult::cancelled());
            }
            _ => {}
        }
    }

    pub(super) fn region_frame_done(&mut self, id: u64) {
        let Some(state) = self.region.as_mut() else { return };
        if let Some(s) = state.surfaces.iter_mut().find(|s| s.id == id) {
            s.frame_pending = false;
        }
        self.region_redraw();
    }

    /// `key` is a wl_keyboard keycode; the seat's keymap says what it is, with the standard evdev codes as fallback.
    pub(super) fn region_key(&mut self, key: u32) {
        let (enter, escape) = match &self.confirm_keys {
            Some(k) if !k.enter.is_empty() || !k.escape.is_empty() => (k.enter.contains(&key), k.escape.contains(&key)),
            _ => (matches!(key, KEY_ENTER | KEY_KPENTER), key == KEY_ESC),
        };
        SLOG.debug(format_args!("Key {key} (enter: {enter}, escape: {escape})"));
        if escape {
            self.finish_region_select(RegionSelectResult::cancelled());
        } else if enter {
            self.confirm_region();
        }
    }

    pub(super) fn region_pointer(&mut self, event: wayland_client::protocol::wl_pointer::Event) {
        use wayland_client::protocol::wl_pointer::{ButtonState, Event};
        let Some(state) = self.region.as_mut() else { return };
        match event {
            Event::Enter { serial, surface, surface_x, surface_y } => {
                state.enter_serial = serial;
                state.pointer_surface = surface.data::<u64>().and_then(|id| state.surfaces.iter().position(|s| s.id == *id));
                if let Some(i) = state.pointer_surface {
                    let b = state.surfaces[i].bounds;
                    state.pointer = (b.left + surface_x.round() as i32, b.top + surface_y.round() as i32);
                }
                self.region_pointer_moved();
            }
            Event::Leave { .. } => {
                // During a drag the pointer stays grabbed by the pressed surface, so a leave only ends hovering.
                if state.action == Action::None {
                    state.pointer_surface = None;
                    state.hover = None;
                    self.region_redraw();
                }
            }
            Event::Motion { surface_x, surface_y, .. } => {
                let Some(i) = state.pointer_surface else { return };
                let b = state.surfaces[i].bounds;
                state.pointer = (b.left + surface_x.round() as i32, b.top + surface_y.round() as i32);
                self.region_pointer_moved();
            }
            Event::Button { button, state: WEnum::Value(pressed), .. } => {
                if state.pointer_surface.is_none() {
                    return;
                }
                let pressed = pressed == ButtonState::Pressed;
                match (button, pressed) {
                    (BTN_RIGHT, true) => self.finish_region_select(RegionSelectResult::cancelled()),
                    (BTN_LEFT, true) => {
                        let p = state.pointer;
                        state.action = match state.hit(p) {
                            Hit::Button(b) => Action::Pressing(b),
                            Hit::Move => Action::Moving { grab: p, orig: state.rect.expect("Move needs a rect") },
                            Hit::Resize(edges) => Action::Resizing { edges, grab: p, orig: state.rect.expect("Resize needs a rect") },
                            Hit::Draw => Action::Drawing { anchor: p, previous: state.rect },
                        };
                        self.region_redraw();
                    }
                    (BTN_LEFT, false) => {
                        let action = std::mem::replace(&mut state.action, Action::None);
                        match action {
                            Action::Pressing(b) if state.hit(state.pointer) == Hit::Button(b) => match b {
                                Button::Save => return self.confirm_region(),
                                Button::Cancel => return self.finish_region_select(RegionSelectResult::cancelled()),
                            },
                            Action::Drawing { anchor, previous } => {
                                let (dx, dy) = (state.pointer.0 - anchor.0, state.pointer.1 - anchor.1);
                                if dx.abs() < MIN_DRAG || dy.abs() < MIN_DRAG {
                                    state.rect = previous;
                                }
                            }
                            _ => {}
                        }
                        self.region_pointer_moved();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn region_pointer_moved(&mut self) {
        let Some(state) = self.region.as_mut() else { return };
        let p = state.pointer;
        match state.action {
            Action::Drawing { anchor, .. } => state.rect = Some(normalized(anchor, p)),
            Action::Moving { grab, orig } => {
                let (dx, dy) = (p.0 - grab.0, p.1 - grab.1);
                state.rect = Some(Rect { left: orig.left + dx, top: orig.top + dy, right: orig.right + dx, bottom: orig.bottom + dy });
            }
            Action::Resizing { edges, grab, orig } => state.rect = Some(resized(orig, edges, (p.0 - grab.0, p.1 - grab.1))),
            Action::Pressing(_) | Action::None => {}
        }
        let hit = state.hit(p);
        state.hover = match (state.action, hit) {
            (Action::None | Action::Pressing(_), Hit::Button(b)) => Some(b),
            _ => None,
        };
        let shape = match state.action {
            Action::None | Action::Pressing(_) => cursor_for(hit),
            Action::Drawing { .. } => Shape::Crosshair,
            Action::Moving { .. } => Shape::Move,
            Action::Resizing { edges, .. } => cursor_for(Hit::Resize(edges)),
        };
        let serial = state.enter_serial;
        if let (Some(mgr), Some(pointer)) = (&self.cursor_shape, &self.pointer) {
            if self.cursor_device.is_none() {
                self.cursor_device = Some(mgr.get_pointer(pointer, &self.qh, ()));
            }
            if let Some(d) = &self.cursor_device {
                d.set_shape(serial, shape);
            }
        }
        self.region_redraw();
    }

    /// Repaints every configured surface whose scene changed and that isn't waiting on a frame.
    fn region_redraw(&mut self) {
        let Some(state) = self.region.as_ref() else { return };
        let buttons = state.button_rects(state.buttons_monitor());
        for i in 0..state.surfaces.len() {
            let Some(state) = self.region.as_ref() else { return };
            let s = &state.surfaces[i];
            let scene = state.scene_for(s, buttons);
            if !s.configured || s.frame_pending || s.shown.as_ref() == Some(&scene) {
                continue;
            }
            let (w, h) = s.size;
            let Some((buffer, ptr)) = alloc_buffer(self, w, h) else {
                SLOG.err(format_args!("Can't allocate a {w}x{h} overlay buffer"));
                continue;
            };
            let Some(state) = self.region.as_mut() else { return };
            if let Err(e) = paint(state, i, ptr, w, h, buttons) {
                SLOG.err(format_args!("Painting the region overlay failed: {e}"));
            }
            let s = &mut state.surfaces[i];
            s.wl.attach(Some(&buffer), 0, 0);
            s.wl.damage_buffer(0, 0, w, h);
            s.wl.frame(&self.qh, s.id);
            s.wl.commit();
            s.frame_pending = true;
            s.shown = Some(scene);
        }
    }
}

fn write_result(result: RegionSelectResult) {
    let path = protocol::region_select_result_path();
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, result.to_file_json()).and_then(|_| std::fs::rename(&tmp, &path)) {
        SLOG.err(format_args!("Can't write the region selection result to {}: {e}", path.display()));
    }
}

fn paint(state: &RegionSelect, i: usize, ptr: *mut u8, w: i32, h: i32, buttons: Option<(Rect, Rect)>) -> Result<(), cairo::Error> {
    let s = &state.surfaces[i];
    // SAFETY: ptr maps w*h*4 bytes that stay mapped until the compositor releases the buffer, after this returns.
    let surface = unsafe { cairo::ImageSurface::create_for_data_unsafe(ptr, cairo::Format::ARgb32, w, h, w * 4)? };
    let cr = cairo::Context::new(&surface)?;
    // Layout space -> this surface (scaled when the compositor's logical size differs from the monitor's bounds).
    let (bw, bh) = ((s.bounds.right - s.bounds.left).max(1) as f64, (s.bounds.bottom - s.bounds.top).max(1) as f64);
    cr.scale(w as f64 / bw, h as f64 / bh);
    cr.translate(-s.bounds.left as f64, -s.bounds.top as f64);

    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
    cr.paint()?;

    if let Some(r) = state.rect {
        let (x, y, rw, rh) = (r.left as f64, r.top as f64, r.width() as f64, r.height() as f64);
        cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 0.08);
        cr.rectangle(x, y, rw, rh);
        cr.fill()?;
        cr.set_operator(cairo::Operator::Over);
        cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
        cr.set_line_width(2.0);
        cr.rectangle(x + 1.0, y + 1.0, (rw - 2.0).max(0.0), (rh - 2.0).max(0.0));
        cr.stroke()?;
        for (hx, hy) in [(x, y), (x + rw / 2.0, y), (x + rw, y), (x, y + rh / 2.0), (x + rw, y + rh / 2.0), (x, y + rh), (x + rw / 2.0, y + rh), (x + rw, y + rh)] {
            cr.rectangle(hx - HANDLE / 2.0, hy - HANDLE / 2.0, HANDLE, HANDLE);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.fill_preserve()?;
            cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
            cr.set_line_width(1.5);
            cr.stroke()?;
        }
        // Size and position, above the top-left corner (inside it when there's no room).
        let label = text_layout(&cr, &format!("{} \u{00d7} {}   ({}, {})", r.width(), r.height(), r.left, r.top), false);
        let (lw, lh) = label.pixel_size();
        let ly = if r.top - lh - 14 >= s.bounds.top { y - lh as f64 - 12.0 } else { y + 8.0 };
        cr.set_source_rgba(0.08, 0.08, 0.08, 0.85);
        rounded(&cr, x, ly, lw as f64 + 12.0, lh as f64 + 6.0, 4.0);
        cr.fill()?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(x + 6.0, ly + 3.0);
        pangocairo::functions::show_layout(&cr, &label);
    }
    cr.set_operator(cairo::Operator::Over);

    // The hint, top centre of every monitor.
    let line1 = if state.rect.is_some() { &state.hint_edit } else { &state.hint_new };
    let hint = text_layout(&cr, &format!("{line1}\n{}", state.hint_confirm), false);
    hint.set_alignment(pango::Alignment::Center);
    let (tw, th) = hint.pixel_size();
    let (px, py) = (s.bounds.left as f64 + (bw - tw as f64) / 2.0 - 16.0, s.bounds.top as f64 + 48.0);
    cr.set_source_rgba(0.08, 0.08, 0.08, 0.88);
    rounded(&cr, px, py, tw as f64 + 32.0, th as f64 + 20.0, 8.0);
    cr.fill()?;
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.move_to(px + 16.0, py + 10.0);
    pangocairo::functions::show_layout(&cr, &hint);

    if let Some((save, cancel)) = buttons {
        for (b, r, label, primary) in [(Button::Save, save, &state.save_label, true), (Button::Cancel, cancel, &state.cancel_label, false)] {
            if !intersects(r, s.bounds) {
                continue;
            }
            let hovered = state.hover == Some(b);
            let (x, y, bw, bh) = (r.left as f64, r.top as f64, r.width() as f64, r.height() as f64);
            match (primary, hovered) {
                (true, false) => cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2),
                (true, true) => cr.set_source_rgb(ACCENT.0 * 1.2, ACCENT.1 * 1.15, (ACCENT.2 * 1.08).min(1.0)),
                (false, false) => cr.set_source_rgba(0.15, 0.15, 0.15, 0.92),
                (false, true) => cr.set_source_rgba(0.28, 0.28, 0.28, 0.95),
            }
            rounded(&cr, x, y, bw, bh, 5.0);
            cr.fill()?;
            let text = text_layout(&cr, label, true);
            let (lw, lh) = text.pixel_size();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.move_to(x + (bw - lw as f64) / 2.0, y + (bh - lh as f64) / 2.0);
            pangocairo::functions::show_layout(&cr, &text);
        }
    }
    drop(cr);
    surface.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = Rect { left: 100, top: 100, right: 300, bottom: 200 };

    #[test]
    fn hits_edges_corners_inside_and_outside() {
        assert_eq!(hit_rect(R, 100, 150), Hit::Resize(Edges { left: true, ..Default::default() }));
        assert_eq!(hit_rect(R, 305, 150), Hit::Resize(Edges { right: true, ..Default::default() }));
        assert_eq!(hit_rect(R, 200, 96), Hit::Resize(Edges { top: true, ..Default::default() }));
        assert_eq!(hit_rect(R, 299, 199), Hit::Resize(Edges { right: true, bottom: true, ..Default::default() }));
        assert_eq!(hit_rect(R, 200, 150), Hit::Move);
        assert_eq!(hit_rect(R, 50, 150), Hit::Draw);
        assert_eq!(hit_rect(R, 200, 220), Hit::Draw);
        // Too thin to tell the edges apart: the far ones win.
        let thin = Rect { left: 100, top: 100, right: 104, bottom: 200 };
        assert_eq!(hit_rect(thin, 102, 150), Hit::Resize(Edges { right: true, ..Default::default() }));
    }

    #[test]
    fn resizing_moves_only_grabbed_edges_and_flips() {
        let e = Edges { right: true, bottom: true, ..Default::default() };
        assert_eq!(resized(R, e, (50, -20)), Rect { left: 100, top: 100, right: 350, bottom: 180 });
        let l = Edges { left: true, ..Default::default() };
        assert_eq!(resized(R, l, (250, 0)), Rect { left: 300, top: 100, right: 350, bottom: 200 });
    }

    #[test]
    fn cursors_match_edges() {
        let r = |l, t, rt, b| cursor_for(Hit::Resize(Edges { left: l, top: t, right: rt, bottom: b }));
        assert_eq!(r(true, true, false, false), Shape::NwseResize);
        assert_eq!(r(false, true, true, false), Shape::NeswResize);
        assert_eq!(r(false, false, true, false), Shape::EwResize);
        assert_eq!(r(false, false, false, true), Shape::NsResize);
    }
}
