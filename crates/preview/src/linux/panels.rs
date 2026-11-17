//! The Client List view (`viewMode: ClientList`) and the notification History Panel on Linux: two small layer-shell
//! panels drawn with cairo, like the Zig build's list_view.zig and notif_info_view.zig. What they show comes from
//! eve_maj_core::panels; this is the drawing, placement, dragging and clicks.
//!
//! Each panel is dragged by its header (left button, when dragging is enabled), snapping to screen and thumbnail
//! edges, and its position is saved when the profile says to remember it. Clicking a list row focuses that client
//! (Shift+click toggles its cycle exclusion); clicking a history row focuses the client it came from, and the
//! footer's buttons filter the history by category.

use eve_maj_core::config::Position;
use eve_maj_core::display_grid::Rect;
use eve_maj_core::layout::{self, is_generic_character_name};
use eve_maj_core::log::Scope;
use eve_maj_core::panels::{self as model, HistoryPanelVisibility, HistoryRow, ListItem, ListRow, NotificationHistory};
use eve_maj_core::render::now_ms;
use eve_maj_core::state::VisibilityState;
use eve_maj_core::types::{FontWeight, ViewMode};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1}};

use super::backend::WindowId;
use super::shm::alloc_buffer;
use super::App;

const SLOG: Scope = Scope::new("panels");
const NAMESPACE: &str = "eve-maj-preview-panel";
/// Surface ids of the two panels (thumbnails count up from 1, region-select surfaces start at 1 << 62).
pub(super) const LIST_ID: u64 = 1 << 61;
pub(super) const HISTORY_ID: u64 = (1 << 61) + 1;

pub(super) fn is_panel(id: u64) -> bool {
    id == LIST_ID || id == HISTORY_ID
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    List,
    History,
}

impl Kind {
    fn id(self) -> u64 {
        match self {
            Kind::List => LIST_ID,
            Kind::History => HISTORY_ID,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Font {
    name: String,
    size: i32,
    weight: FontWeight,
}

#[derive(Debug, Clone, PartialEq)]
struct ListContent {
    rows: Vec<ListRow>,
    columns: u32,
    opacity: u8,
    font: Font,
}

#[derive(Debug, Clone, PartialEq)]
struct HistoryContent {
    rows: Vec<HistoryRow<WindowId>>,
    empty_text: &'static str,
    opacity: u8,
    font: Font,
    show_filters: bool,
    enabled: [bool; 5],
    footer_top: i32,
}

#[derive(Debug, Clone, PartialEq)]
enum Content {
    List(ListContent),
    History(HistoryContent),
}

struct Panel {
    kind: Kind,
    wl: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// The monitor (by name) the surface lives on, and its origin in layout space.
    output: String,
    origin: (i32, i32),
    /// Top-left in layout space, and size.
    pos: (i32, i32),
    size: (i32, i32),
    /// The configured position this panel was last placed from; a new one (reload, live preview) moves it.
    config_pos: (i32, i32),
    configured: bool,
    frame_pending: bool,
    content: Option<Content>,
    shown: Option<Content>,
}

/// What a panel should show, where it's configured to be, and its size.
type Wanted = (Content, (i32, i32), (i32, i32));

struct PanelDrag {
    kind: Kind,
    /// Where on the panel it was grabbed (surface-local).
    grab: (f64, f64),
    refocus: Option<WindowId>,
}

/// Both panels' state, owned by the App.
#[derive(Default)]
pub(super) struct Panels {
    list: Option<Panel>,
    history_panel: Option<Panel>,
    /// The window behind each list row, in display order; and behind each history row drawn.
    list_sources: Vec<WindowId>,
    history_sources: Vec<WindowId>,
    drag: Option<PanelDrag>,
    pub(super) history: NotificationHistory<WindowId>,
    visibility: HistoryPanelVisibility,
}

impl Panels {
    fn get(&self, kind: Kind) -> Option<&Panel> {
        match kind {
            Kind::List => self.list.as_ref(),
            Kind::History => self.history_panel.as_ref(),
        }
    }

    fn get_mut(&mut self, kind: Kind) -> Option<&mut Panel> {
        match kind {
            Kind::List => self.list.as_mut(),
            Kind::History => self.history_panel.as_mut(),
        }
    }

    fn slot(&mut self, kind: Kind) -> &mut Option<Panel> {
        match kind {
            Kind::List => &mut self.list,
            Kind::History => &mut self.history_panel,
        }
    }

    /// The surface id of the panel being dragged.
    pub(super) fn dragged_id(&self) -> Option<u64> {
        self.drag.as_ref().map(|d| d.kind.id())
    }

    pub(super) fn dragging(&self) -> bool {
        self.drag.is_some()
    }
}

fn kind_of(id: u64) -> Option<Kind> {
    match id {
        LIST_ID => Some(Kind::List),
        HISTORY_ID => Some(Kind::History),
        _ => None,
    }
}

impl App {
    fn any_logged_in(&self) -> bool {
        self.thumbs.iter().any(|t| !is_generic_character_name(&t.name))
    }

    pub(super) fn history_panel_visible(&self) -> bool {
        self.panels.visibility.is_visible(&self.config.display, self.any_logged_in())
    }

    /// The tray's "Show History Panel".
    pub(super) fn toggle_history_panel(&mut self) {
        let any = self.any_logged_in();
        self.panels.visibility.toggle(&mut self.config.display, any);
        if let Err(e) = self.config.save_current_profile() {
            SLOG.err(format_args!("Failed to save profile after toggling the history panel: {e}"));
        }
        self.profile_mtime = super::profile_mtime(&self.config.profile_name);
        self.update_panels();
    }

    pub(super) fn clear_notification_history(&mut self) {
        self.panels.history.clear();
        self.update_panels();
    }

    /// Brings both panels in line with the config and the tracked clients: shows, hides, moves, resizes and
    /// repaints them as needed. Cheap when nothing changed.
    pub(super) fn update_panels(&mut self) {
        let list = self.list_content();
        self.place_panel(Kind::List, list);
        let any = self.any_logged_in();
        self.panels.visibility.update(any);
        let history = self.history_panel_visible().then(|| self.history_content());
        self.place_panel(Kind::History, history);
        self.redraw_panels();
    }

    fn list_content(&mut self) -> Option<Wanted> {
        let display = &self.config.display;
        if display.view_mode != ViewMode::ClientList {
            return None;
        }
        // Per-character hidden clients get no row; with every client hidden (visibility hotkey, auto-hide), no list.
        let shown: Vec<usize> = (0..self.thumbs.len()).filter(|&i| !self.thumbs[i].hide_thumbnail).collect();
        if shown.is_empty() || !shown.iter().any(|&i| self.thumbs[i].visibility == VisibilityState::Visible) {
            return None;
        }
        let states: Vec<_> = shown.iter().map(|&i| self.effective_state(&self.thumbs[i])).collect();
        let items: Vec<ListItem<'_>> = shown
            .iter()
            .zip(&states)
            .map(|(&i, &state)| {
                let t = &self.thumbs[i];
                ListItem {
                    character_name: &t.name,
                    display_name: self.config.display_name(&t.name),
                    state,
                    excluded: t.excluded,
                    newest_notification: t.notifications.first(),
                    system_name: &t.system_name,
                    system_color: t.system_color,
                    character_color: t.character_color,
                    active_border_override: t.border_colors.and_then(|b| b.active_border_color),
                    metrics: &t.metrics,
                }
            })
            .collect();
        let rows = model::list_rows(&self.config, &items);
        let sources = rows.iter().map(|r| self.thumbs[shown[r.source]].window).collect();
        let display = &self.config.display;
        let size = model::list_size(display.list_view_columns, rows.len());
        let content = Content::List(ListContent {
            rows,
            columns: display.list_view_columns,
            opacity: display.list_view_opacity,
            font: Font { name: display.list_view_font_name.clone(), size: display.list_view_font_size, weight: display.list_view_font_weight },
        });
        self.panels.list_sources = sources;
        Some((content, (display.start_x, display.start_y), size))
    }

    fn history_content(&mut self) -> Wanted {
        let display = &self.config.display;
        let size = (display.notif_info_panel_width.max(1), display.notif_info_panel_height.max(1));
        let rows = model::history_rows(&self.config, &self.panels.history, size.1, now_ms());
        self.panels.history_sources = rows.iter().map(|r| r.source).collect();
        let content = Content::History(HistoryContent {
            rows,
            empty_text: model::history_empty_text(self.panels.history.len()),
            opacity: display.notif_info_panel_opacity,
            font: Font { name: display.notif_info_panel_font_name.clone(), size: display.notif_info_panel_font_size, weight: display.notif_info_panel_font_weight },
            show_filters: display.notif_info_panel_show_category_filters,
            enabled: model::CATEGORIES.map(|(c, _)| model::category_enabled(display, c)),
            footer_top: model::history_footer_top(display, size.1),
        });
        (content, (display.notif_info_panel_x, display.notif_info_panel_y), size)
    }

    /// A configured position moved onto a monitor: Hyprland's layout space can have gaps where no monitor is.
    fn on_screen(&self, (x, y): (i32, i32), (w, h): (i32, i32)) -> (i32, i32) {
        match self.monitor_for(x, y) {
            Some(m) => {
                let b = m.bounds;
                (x.clamp(b.left, (b.right - w).max(b.left)), y.clamp(b.top, (b.bottom - h).max(b.top)))
            }
            None => (x, y),
        }
    }

    fn place_panel(&mut self, kind: Kind, want: Option<Wanted>) {
        let Some((content, config_pos, size)) = want else {
            if let Some(p) = self.panels.slot(kind).take() {
                destroy(p);
            }
            if self.panels.drag.as_ref().is_some_and(|d| d.kind == kind) {
                self.panels.drag = None;
            }
            return;
        };
        let dragging = self.panels.drag.as_ref().is_some_and(|d| d.kind == kind);
        let current = self.panels.get(kind).map(|p| (p.pos, p.config_pos));
        let pos = match current {
            // Keep a dragged (or dragged-but-not-remembered) panel where it is until the configured position changes.
            Some((pos, last_config)) if dragging || last_config == config_pos => pos,
            _ => self.on_screen(config_pos, size),
        };
        let target = self.monitor_for(pos.0, pos.1).map(|m| (m.name.clone(), (m.bounds.left, m.bounds.top)));
        let Some((output_name, origin)) = target else { return };
        // Another monitor, or the same one moved in the layout: margins are relative to its origin, so start over.
        let moved_monitor = self.panels.get(kind).is_some_and(|p| p.output != output_name || p.origin != origin);
        if moved_monitor && !dragging {
            if let Some(p) = self.panels.slot(kind).take() {
                destroy(p);
            }
        }
        if self.panels.get(kind).is_none() {
            // The monitor's wl_output hasn't arrived yet (just plugged in): place the panel on a later update.
            let Some(output) = self.outputs.iter().find(|o| o.name == output_name).map(|o| o.wl.clone()) else { return };
            let wl = self.compositor.create_surface(&self.qh, kind.id());
            let layer = self.layer_shell.get_layer_surface(&wl, Some(&output), zwlr_layer_shell_v1::Layer::Overlay, NAMESPACE.to_owned(), &self.qh, kind.id());
            layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
            layer.set_size(size.0 as u32, size.1 as u32);
            layer.set_margin(pos.1 - origin.1, 0, 0, pos.0 - origin.0);
            layer.set_exclusive_zone(-1);
            // The list takes focus on click so Shift+click can see Shift (focus goes straight to a client again).
            let keyboard = if kind == Kind::List { zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand } else { zwlr_layer_surface_v1::KeyboardInteractivity::None };
            layer.set_keyboard_interactivity(keyboard);
            wl.commit();
            *self.panels.slot(kind) = Some(Panel {
                kind,
                wl,
                layer,
                output: output_name,
                origin,
                pos,
                size,
                config_pos,
                configured: false,
                frame_pending: false,
                content: None,
                shown: None,
            });
        }
        let Some(p) = self.panels.get_mut(kind) else { return };
        p.config_pos = config_pos;
        let mut commit = false;
        if p.size != size {
            p.size = size;
            p.layer.set_size(size.0 as u32, size.1 as u32);
            p.shown = None;
            // Draw at the new size once the compositor has configured it.
            p.configured = false;
            commit = true;
        }
        if p.pos != pos && !dragging {
            p.pos = pos;
            p.layer.set_margin(pos.1 - p.origin.1, 0, 0, pos.0 - p.origin.0);
            commit = true;
        }
        if commit {
            p.wl.commit();
        }
        p.content = Some(content);
    }

    fn redraw_panels(&mut self) {
        for kind in [Kind::List, Kind::History] {
            let Some(p) = self.panels.get(kind) else { continue };
            if !p.configured || p.frame_pending || p.content.is_none() || p.shown == p.content {
                continue;
            }
            let (w, h) = p.size;
            let Some((buffer, ptr)) = alloc_buffer(self, w, h) else {
                SLOG.err(format_args!("Can't allocate a {w}x{h} panel buffer"));
                continue;
            };
            let Some(p) = self.panels.get_mut(kind) else { continue };
            let content = p.content.clone().expect("checked above");
            if let Err(e) = paint(&content, ptr, w, h) {
                SLOG.err(format_args!("Painting the {kind:?} panel failed: {e}"));
            }
            p.wl.attach(Some(&buffer), 0, 0);
            p.wl.damage_buffer(0, 0, w, h);
            p.wl.frame(&self.qh, p.kind.id());
            p.wl.commit();
            p.frame_pending = true;
            p.shown = Some(content);
        }
    }

    pub(super) fn panel_layer_event(&mut self, id: u64, event: zwlr_layer_surface_v1::Event) {
        let Some(kind) = kind_of(id) else { return };
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                let Some(p) = self.panels.get_mut(kind) else { return };
                p.layer.ack_configure(serial);
                p.configured = true;
                p.shown = None;
                self.redraw_panels();
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(p) = self.panels.slot(kind).take() {
                    destroy(p);
                }
            }
            _ => {}
        }
    }

    pub(super) fn panel_frame_done(&mut self, id: u64) {
        let Some(kind) = kind_of(id) else { return };
        if let Some(p) = self.panels.get_mut(kind) {
            p.frame_pending = false;
        }
        self.redraw_panels();
    }

    /// A left press on a panel at surface-local (x, y).
    pub(super) fn panel_press(&mut self, id: u64, (x, y): (f64, f64)) {
        let Some(kind) = kind_of(id) else { return };
        let (xi, yi) = (x as i32, y as i32);
        let header = match kind {
            Kind::List => model::LIST_HEADER_HEIGHT,
            Kind::History => model::HISTORY_HEADER_HEIGHT,
        };
        if yi < header {
            if self.config.interaction.enable_dragging {
                let refocus = self.active.filter(|a| self.thumbs.iter().any(|t| t.window == *a));
                self.panels.drag = Some(PanelDrag { kind, grab: (x, y), refocus });
            }
            return;
        }
        match kind {
            Kind::List => {
                let n = self.panels.list_sources.len();
                let Some(row) = model::list_row_at(self.config.display.list_view_columns, n, xi, yi) else { return };
                let window = self.panels.list_sources[row];
                if let Some(t) = self.thumbs.iter().find(|t| t.window == window) {
                    let id = t.id;
                    self.on_click(id);
                }
            }
            Kind::History => {
                let Some(p) = self.panels.get(kind) else { return };
                let footer_top = model::history_footer_top(&self.config.display, p.size.1);
                if self.config.display.notif_info_panel_show_category_filters && yi >= footer_top {
                    let spans = model::history_button_spans(p.size.0);
                    if let Some(i) = spans.iter().position(|&(l, r)| xi >= l && xi < r) {
                        let category = model::CATEGORIES[i].0;
                        let on = model::category_enabled(&self.config.display, category);
                        model::set_category_enabled(&mut self.config.display, category, !on);
                        if let Err(e) = self.config.save_notif_info_panel_category_filter() {
                            SLOG.err(format_args!("Failed to save the history panel's category filter: {e}"));
                        }
                        self.profile_mtime = super::profile_mtime(&self.config.profile_name);
                        self.update_panels();
                    }
                    return;
                }
                let row = ((yi - header) / model::HISTORY_ROW_HEIGHT) as usize;
                let Some(&window) = self.panels.history_sources.get(row) else { return };
                if let Some(i) = self.thumbs.iter().position(|t| t.window == window) {
                    self.thumbs[i].last_click_ms = now_ms();
                    self.activate_window(window);
                }
            }
        }
    }

    /// Pointer motion at surface-local (x, y) during a panel drag.
    pub(super) fn panel_drag_motion(&mut self, (x, y): (f64, f64)) {
        let Some(drag) = &self.panels.drag else { return };
        let (kind, grab) = (drag.kind, drag.grab);
        let Some(p) = self.panels.get(kind) else { return };
        let (w, h) = p.size;
        let new_x = p.pos.0 + (x - grab.0).round() as i32;
        let new_y = p.pos.1 + (y - grab.1).round() as i32;
        if (new_x, new_y) == p.pos {
            return;
        }
        let (mut sx, mut sy) = (new_x, new_y);
        let snapping = &self.config.snapping;
        if snapping.enabled {
            if snapping.screen_edges {
                if let Some(m) = self.monitor_for(new_x + w / 2, new_y + h / 2) {
                    (sx, sy) = layout::screen_edge_snap(sx, sy, w, h, snapping.threshold, m.bounds);
                }
            }
            if snapping.thumbnail_edges {
                let others: Vec<Rect> = self
                    .thumbs
                    .iter()
                    .filter(|t| t.surf.is_some())
                    .map(|t| Rect { left: t.rect.x, top: t.rect.y, right: t.rect.x + t.rect.width, bottom: t.rect.y + t.rect.height })
                    .collect();
                (sx, sy) = layout::thumbnail_edge_snap(sx, sy, w, h, snapping.threshold, &others);
            }
        }
        let Some(p) = self.panels.get_mut(kind) else { return };
        p.pos = (sx, sy);
        p.layer.set_margin(sy - p.origin.1, 0, 0, sx - p.origin.0);
        p.wl.commit();
    }

    pub(super) fn end_panel_drag(&mut self) {
        let Some(drag) = self.panels.drag.take() else { return };
        let Some(p) = self.panels.get(drag.kind) else { return };
        let pos = p.pos;
        let display = &self.config.display;
        let saved = match drag.kind {
            Kind::List if display.remember_list_view_position => Some(self.config.save_list_view_position(Position { x: pos.0, y: pos.1 })),
            Kind::History if display.remember_notif_info_panel_position => Some(self.config.save_notif_info_panel_position(Position { x: pos.0, y: pos.1 })),
            _ => None,
        };
        match saved {
            Some(Err(e)) => SLOG.err(format_args!("Failed to save the {:?} panel position: {e}", drag.kind)),
            Some(Ok(())) => {
                self.profile_mtime = super::profile_mtime(&self.config.profile_name);
                // The saved position is where it is now; don't jump.
                if let Some(p) = self.panels.get_mut(drag.kind) {
                    p.config_pos = pos;
                }
            }
            None => {}
        }
        // Dropped on another monitor: the surface moves to that output.
        let target = self.monitor_for(pos.0, pos.1).map(|m| m.name.clone());
        if let Some(p) = self.panels.get(drag.kind) {
            if target.is_some_and(|name| name != p.output) {
                let p = self.panels.slot(drag.kind).take().expect("checked");
                let config_pos = p.config_pos;
                destroy(p);
                // Recreated at `pos` by the next update; a remembered position equals it, an unremembered one is kept.
                self.panels_pending_pos(drag.kind, pos, config_pos);
            }
        }
        if let Some(window) = drag.refocus {
            self.wm.focus(window);
        }
        self.update_panels();
    }

    /// After a cross-monitor drop of a panel whose position isn't saved, keeps it where it was dropped.
    fn panels_pending_pos(&mut self, kind: Kind, pos: (i32, i32), config_pos: (i32, i32)) {
        if pos == config_pos {
            return;
        }
        let want = match kind {
            Kind::List => self.list_content(),
            Kind::History => self.history_panel_visible().then(|| self.history_content()),
        };
        let Some((content, _, size)) = want else { return };
        // Place it as if `pos` were configured, then restore the real configured position so a later change still moves it.
        self.place_panel(kind, Some((content, pos, size)));
        if let Some(p) = self.panels.get_mut(kind) {
            p.config_pos = config_pos;
        }
    }
}

fn destroy(p: Panel) {
    p.layer.destroy();
    p.wl.destroy();
}

// --------------------------------------------------------------------------------------------------- drawing --

fn rgb(cr: &cairo::Context, color: u32, alpha: f64) {
    cr.set_source_rgba(((color >> 16) & 0xFF) as f64 / 255.0, ((color >> 8) & 0xFF) as f64 / 255.0, (color & 0xFF) as f64 / 255.0, alpha);
}

fn fill(cr: &cairo::Context, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: f64) {
    if w <= 0 || h <= 0 {
        return;
    }
    rgb(cr, color, alpha);
    cr.rectangle(x as f64, y as f64, w as f64, h as f64);
    let _ = cr.fill();
}

fn layout(cr: &cairo::Context, font: &pango::FontDescription, text: &str, max_w: Option<i32>) -> pango::Layout {
    let l = pangocairo::functions::create_layout(cr);
    l.set_font_description(Some(font));
    l.set_text(text);
    if let Some(w) = max_w {
        l.set_width(w.max(0) * pango::SCALE);
        l.set_ellipsize(pango::EllipsizeMode::End);
    }
    l
}

fn width_of(l: &pango::Layout) -> i32 {
    l.pixel_size().0
}

fn show(cr: &cairo::Context, l: &pango::Layout, x: i32, y: i32, color: u32) {
    rgb(cr, color, 1.0);
    cr.move_to(x as f64, y as f64);
    pangocairo::functions::show_layout(cr, l);
}

/// The list's text: a dim copy one pixel right and one down under the real one, a soft glow.
fn show_glow(cr: &cairo::Context, l: &pango::Layout, x: i32, y: i32, color: u32) {
    let glow = scale_rgb(color, 108);
    show(cr, l, x + 1, y, glow);
    show(cr, l, x, y + 1, glow);
    show(cr, l, x, y, color);
}

fn scale_rgb(color: u32, factor: u32) -> u32 {
    let c = |shift: u32| (((color >> shift) & 0xFF) * factor / 255) << shift;
    c(16) | c(8) | c(0)
}

fn paint(content: &Content, ptr: *mut u8, w: i32, h: i32) -> Result<(), cairo::Error> {
    // SAFETY: ptr maps w*h*4 bytes that stay mapped until the compositor releases the buffer, after this returns.
    let surface = unsafe { cairo::ImageSurface::create_for_data_unsafe(ptr, cairo::Format::ARgb32, w, h, w * 4)? };
    let cr = cairo::Context::new(&surface)?;
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    cr.paint()?;
    cr.set_operator(cairo::Operator::Over);
    let opacity = match content {
        Content::List(c) => {
            paint_list(&cr, c, w, h);
            c.opacity
        }
        Content::History(c) => {
            paint_history(&cr, c, w, h);
            c.opacity
        }
    };
    drop(cr);
    surface.flush();
    // SAFETY: as above; cairo is done with the pixels.
    let pixels = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, (w * h) as usize) };
    if let Content::List(_) = content {
        scanlines(pixels, w as usize, h as usize);
        vignette(pixels, w as usize, h as usize);
    }
    // The whole panel fades with its opacity, on top of the backgrounds' own alpha (as the Windows layered window does).
    if opacity < 255 {
        let a = opacity as u32;
        for p in pixels.iter_mut() {
            let ch = |shift: u32| ((((*p >> shift) & 0xFF) * a / 255) & 0xFF) << shift;
            *p = ch(24) | ch(16) | ch(8) | ch(0);
        }
    }
    Ok(())
}

fn paint_list(cr: &cairo::Context, c: &ListContent, w: i32, h: i32) {
    use model::*;
    let alpha = c.opacity as f64 / 255.0;
    let n = c.rows.len() as i32;
    let columns = effective_columns(c.columns, c.rows.len());
    let font = super::overlay::font_description(&c.font.name, c.font.size, c.font.weight);

    fill(cr, 0, 0, w, LIST_HEADER_HEIGHT, LIST_HEADER_BG, alpha);
    show_glow(cr, &layout(cr, &font, &format!("EVE-Maj Preview // {n}"), None), 8, 5, PANEL_HEADER_TEXT);
    fill(cr, 0, LIST_HEADER_HEIGHT - 1, w, 1, PANEL_SEPARATOR, 1.0);

    for (i, row) in c.rows.iter().enumerate() {
        let (r, col) = (i as i32 / columns, i as i32 % columns);
        let top = LIST_HEADER_HEIGHT + r * LIST_ROW_HEIGHT;
        let left = col * LIST_WIDTH;
        fill(cr, left, top, LIST_WIDTH, LIST_ROW_HEIGHT, row.background, alpha);
        if r + 1 < items_in_column(n, columns, col) {
            fill(cr, left, top + LIST_ROW_HEIGHT - 1, LIST_WIDTH, 1, PANEL_SEPARATOR, 1.0);
        }
        let (cx, cy) = ((left + LIST_BADGE_LEFT + LIST_BADGE_RADIUS) as f64 + 0.5, (top + LIST_ROW_HEIGHT / 2) as f64 + 0.5);
        let radius = LIST_BADGE_RADIUS as f64;
        let dot = |color: u32| {
            rgb(cr, color, 1.0);
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        };
        match row.badge {
            Badge::Dot(color) => dot(color),
            Badge::Excluded => {
                dot(BADGE_DISABLED_BG);
                let arm = (radius - 1.0).max(2.0);
                rgb(cr, BADGE_DISABLED_X, 1.0);
                cr.set_line_width(1.2);
                cr.move_to(cx - arm, cy - arm);
                cr.line_to(cx + arm, cy + arm);
                cr.move_to(cx - arm, cy + arm);
                cr.line_to(cx + arm, cy - arm);
                let _ = cr.stroke();
            }
        }
        let (text_x, text_y) = (left + LIST_TEXT_LEFT, top + LIST_TEXT_PAD_Y);
        let name_max = LIST_WIDTH - LIST_TEXT_LEFT - LIST_RIGHT_MARGIN - LIST_NAME_RESERVE;
        let name = layout(cr, &font, &row.name, None);
        let name = if width_of(&name) > name_max { layout(cr, &font, &row.name, Some(name_max)) } else { name };
        show_glow(cr, &name, text_x, text_y, row.name_color);
        if !row.right_text.is_empty() {
            // Right-aligned after the name with a small gap; shortened with "…" rather than drawn over the name (the
            // Windows list lets them overlap, which happens more with Linux fallback fonts for "Segoe UI").
            let right_edge = left + LIST_WIDTH - LIST_RIGHT_MARGIN;
            let room = right_edge - (text_x + width_of(&name) + 8);
            let right = layout(cr, &font, &row.right_text, None);
            let right = if width_of(&right) > room { layout(cr, &font, &row.right_text, Some(room)) } else { right };
            if room >= 16 {
                show_glow(cr, &right, right_edge - width_of(&right), text_y, row.right_color);
            }
        }
    }

    for col in 1..columns {
        let rows = items_in_column(n, columns, col - 1).max(items_in_column(n, columns, col));
        fill(cr, col * LIST_WIDTH, LIST_HEADER_HEIGHT, 1, rows * LIST_ROW_HEIGHT, PANEL_SEPARATOR, 1.0);
    }
    // Outer frame, stepped per column so it hugs each column's rows.
    fill(cr, 0, 0, w, 1, PANEL_FRAME, alpha);
    fill(cr, 0, 0, 1, h, PANEL_FRAME, alpha);
    for col in 0..columns {
        let bottom = LIST_HEADER_HEIGHT + items_in_column(n, columns, col) * LIST_ROW_HEIGHT - 1;
        fill(cr, col * LIST_WIDTH, bottom, LIST_WIDTH, 1, PANEL_FRAME, alpha);
    }
    let right_h = LIST_HEADER_HEIGHT + items_in_column(n, columns, columns - 1) * LIST_ROW_HEIGHT;
    fill(cr, w - 1, 0, 1, right_h, PANEL_FRAME, alpha);
}

fn paint_history(cr: &cairo::Context, c: &HistoryContent, w: i32, h: i32) {
    use model::*;
    let alpha = c.opacity as f64 / 255.0;
    let font = super::overlay::font_description(&c.font.name, c.font.size, c.font.weight);
    fill(cr, 0, 0, w, HISTORY_HEADER_HEIGHT, HISTORY_HEADER_BG, alpha);
    fill(cr, 0, HISTORY_HEADER_HEIGHT, w, h - HISTORY_HEADER_HEIGHT, HISTORY_BODY_BG, alpha);

    let spans = history_button_spans(w);
    if c.show_filters {
        fill(cr, 0, c.footer_top, w, h - c.footer_top, HISTORY_HEADER_BG, alpha);
        for (i, &(l, r)) in spans.iter().enumerate() {
            if c.enabled[i] {
                fill(cr, l, c.footer_top, r - l, h - c.footer_top, HISTORY_BUTTON_ACTIVE_BG, alpha);
            }
        }
        fill(cr, 0, c.footer_top, w, 1, PANEL_SEPARATOR, 1.0);
        for &(l, _) in &spans[1..] {
            fill(cr, l, c.footer_top, 1, h - c.footer_top, PANEL_SEPARATOR, 1.0);
        }
    }

    let title = layout(cr, &font, HISTORY_TITLE, None);
    show(cr, &title, HISTORY_TEXT_LEFT, ((HISTORY_HEADER_HEIGHT - title.pixel_size().1) / 2).max(0), PANEL_HEADER_TEXT);

    let max_w = (w - HISTORY_TEXT_LEFT - HISTORY_RIGHT_MARGIN).max(0);
    for (i, row) in c.rows.iter().enumerate() {
        let y = HISTORY_HEADER_HEIGHT + i as i32 * HISTORY_ROW_HEIGHT + 1;
        draw_history_row(cr, &font, row, HISTORY_TEXT_LEFT, y, max_w);
    }
    if c.rows.is_empty() {
        show(cr, &layout(cr, &font, c.empty_text, Some(max_w)), HISTORY_TEXT_LEFT, HISTORY_HEADER_HEIGHT + 2, HISTORY_DIM_TEXT);
    }
    if c.show_filters {
        for (i, &(l, r)) in spans.iter().enumerate() {
            let label = layout(cr, &font, CATEGORIES[i].1, None);
            let (lw, lh) = label.pixel_size();
            let color = if c.enabled[i] { PANEL_HEADER_TEXT } else { HISTORY_DIM_TEXT };
            show(cr, &label, l + ((r - l - lw).max(0)) / 2, c.footer_top + ((HISTORY_FOOTER_HEIGHT - lh) / 2).max(0), color);
        }
    }
    fill(cr, 0, HISTORY_HEADER_HEIGHT - 1, w, 1, PANEL_SEPARATOR, 1.0);
    fill(cr, 0, 0, w, 1, PANEL_FRAME, alpha);
    fill(cr, 0, h - 1, w, 1, PANEL_FRAME, alpha);
    fill(cr, 0, 0, 1, h, PANEL_FRAME, alpha);
    fill(cr, w - 1, 0, 1, h, PANEL_FRAME, alpha);
}

/// "Name: message", cut short to fit, with the timestamp (when shown) right-aligned and never dropped.
fn draw_history_row(cr: &cairo::Context, font: &pango::FontDescription, row: &HistoryRow<WindowId>, x: i32, y: i32, max_w: i32) {
    let mut remaining = max_w;
    if let Some(ts) = &row.timestamp {
        let l = layout(cr, font, ts, None);
        let ts_w = width_of(&l).min(max_w);
        let gap = width_of(&layout(cr, font, " ", None));
        remaining = (max_w - ts_w - gap).max(0);
        show(cr, &l, x + max_w - ts_w, y, model::HISTORY_DIM_TEXT);
    }
    let name = layout(cr, font, &row.name, None);
    let name_w = width_of(&name);
    if name_w > remaining {
        show(cr, &layout(cr, font, &row.name, Some(remaining)), x, y, row.name_color);
        return;
    }
    show(cr, &name, x, y, row.name_color);
    let sep = layout(cr, font, ": ", None);
    let sep_w = width_of(&sep);
    if remaining - name_w < sep_w {
        return;
    }
    show(cr, &sep, x + name_w, y, row.text_color);
    let room = remaining - name_w - sep_w;
    if room > 0 {
        show(cr, &layout(cr, font, &row.text, Some(room)), x + name_w + sep_w, y, row.text_color);
    }
}

/// Every other row of pixels slightly darker, as on Windows.
fn scanlines(pixels: &mut [u32], w: usize, h: usize) {
    for y in (1..h).step_by(2) {
        for p in &mut pixels[y * w..(y + 1) * w] {
            if *p >> 24 != 0 {
                *p = (*p & 0xFF00_0000) | scale_rgb(*p & 0xFF_FFFF, 228);
            }
        }
    }
}

/// Darkens towards the edges (up to about a third at the very edge), as on Windows.
fn vignette(pixels: &mut [u32], w: usize, h: usize) {
    if w < 2 || h < 2 {
        return;
    }
    let (cx, cy) = (w / 2, h / 2);
    let (max_dx, max_dy) = (cx.max(1), cy.max(1));
    for y in 0..h {
        let dy = y.abs_diff(cy);
        let edge_y = if dy * 2 > max_dy { ((dy * 2 - max_dy) * 255 / max_dy) as u32 } else { 0 };
        for x in 0..w {
            let p = &mut pixels[y * w + x];
            if *p >> 24 == 0 {
                continue;
            }
            let dx = x.abs_diff(cx);
            let edge_x = if dx * 2 > max_dx { ((dx * 2 - max_dx) * 255 / max_dx) as u32 } else { 0 };
            let edge = edge_x.max(edge_y).min(255);
            if edge > 0 {
                *p = (*p & 0xFF00_0000) | scale_rgb(*p & 0xFF_FFFF, 255 - edge * 80 / 255);
            }
        }
    }
}
