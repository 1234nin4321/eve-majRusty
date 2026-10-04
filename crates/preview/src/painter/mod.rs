//! The central state owner: every tracked client's thumbnail (a DWM thumbnail window plus a layered GDI overlay
//! window on top), their lifecycle, focus/visibility state, notifications, and per-tick reconciliation against
//! Scout's window list. Layout lives in `layout`, overlay drawing in `render`, drag-time overlays in `overlays`.

mod layout;
mod overlays;
mod render;

use std::collections::HashMap;

use eve_maj_app::{sound, tts};
use eve_maj_core::accounts_store;
use eve_maj_core::config::{CharacterBorderColors, CharacterThumbnailSize, NotificationTypeConfig, Position};
use eve_maj_core::log::Scope;
use eve_maj_core::state::{try_transition_visibility, ThumbnailState, VisibilityState};
use eve_maj_core::types::{FontWeight, NotificationType, ViewMode};
use eve_maj_win::geometry::scale_pixels;
use eve_maj_win::time::Ticks;
use eve_maj_win::window::{is_window, is_window_iconic, process_exe_path, class_name};
use eve_maj_win::wide;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub use layout::RegionFitGrid;
pub use overlays::GhostGroup;
pub use render::TextDimensions;

use crate::gdi_overlay::{self, OverlayBitmap};
use crate::globals;
use crate::input::{self, is_thumbnail_dragging, HIDE_DEBOUNCE_TIMER_ID, SOURCE_HWND_PROP};
use crate::manager;
use crate::scout::{is_generic_character_name, ClosedWindow, EveWindow, NameChange};
use layout::*;
use render::{create_render_settings, RenderSettings};

const SLOG: Scope = Scope::new("painter");

const WINDOW_CLASS_NAME: &str = "EVE_THUMBNAIL_CLASS";
const TEXT_WINDOW_CLASS_NAME: &str = "EVE_TEXT_OVERLAY_CLASS";
const GHOST_WINDOW_CLASS_NAME: &str = "EVE_GHOST_OVERLAY_CLASS";

#[derive(Debug, Clone)]
pub struct ActiveNotification {
    pub text: String,
    pub notification_type: NotificationType,
    pub start_time: Ticks,
    pub duration_ms: u32,
    pub suppress_when_focused: bool,
    pub suppress_when_clicked: bool,
    pub border_color_override: Option<u32>,
    pub text_color_override: Option<u32>,
    pub show_border: bool,
    pub flash_border: bool,
}

/// Ring-buffer capacity for Painter::notification_history, feeding the History Panel's list. Mirrors DisplayConfig::NOTIF_PANEL_MAX_ROWS_MAX.
pub const NOTIF_HISTORY_CAPACITY: usize = 30;

/// One past notification retained for the History Panel.
#[derive(Debug, Clone)]
pub struct NotificationHistoryEntry {
    pub source_hwnd: HWND,
    pub notification_type: NotificationType,
    pub character_name: String,
    pub text: String,
    pub timestamp: Ticks,
    /// The character's name color resolved once at push time; a past entry's color is fixed once recorded, so the history panel reads this instead of re-resolving every render tick.
    pub character_color: Option<u32>,
}

/// Cap on simultaneously stacked notifications per thumbnail; kept small since the overlay is drawn onto a small thumbnail bitmap.
const MAX_STACKED_NOTIFICATIONS: usize = 3;

const TEST_NOTIFICATION_PERMANENT_FALLBACK_MS: u32 = 5000;

const NOTIFICATION_FLASH_PHASE_MS: u64 = 150;
const NOTIFICATION_FLASH_CYCLES: u64 = 4;
const NOTIFICATION_FLASH_TOTAL_MS: u64 = NOTIFICATION_FLASH_PHASE_MS * NOTIFICATION_FLASH_CYCLES * 2;

/// Whether a flashing notification's border should currently be hidden; false once the flash sequence has finished and the border settles steady-on.
fn is_notification_flash_off(notif: &ActiveNotification, now: Ticks) -> bool {
    if !notif.show_border || !notif.flash_border {
        return false;
    }
    let elapsed = now.elapsed_since(notif.start_time);
    if elapsed >= NOTIFICATION_FLASH_TOTAL_MS {
        return false;
    }
    (elapsed / NOTIFICATION_FLASH_PHASE_MS) % 2 == 1
}

#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
pub enum RenderError {
    GetDcFailed,
    CreateBitmapFailed,
    CreateFontFailed,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GetDcFailed => "GetDCFailed",
            Self::CreateBitmapFailed => "CreateBitmapFailed",
            Self::CreateFontFailed => "CreateFontFailed",
        })
    }
}

#[derive(Debug)]
pub enum CreateError {
    CreateWindowFailed,
    CreateTextWindowFailed,
    DwmRegisterThumbnailFailed,
    DwmUpdateThumbnailPropertiesFailed,
    Render(RenderError),
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateWindowFailed => f.write_str("CreateWindowFailed"),
            Self::CreateTextWindowFailed => f.write_str("CreateTextWindowFailed"),
            Self::DwmRegisterThumbnailFailed => f.write_str("DwmRegisterThumbnailFailed"),
            Self::DwmUpdateThumbnailPropertiesFailed => f.write_str("DwmUpdateThumbnailPropertiesFailed"),
            Self::Render(e) => write!(f, "{e}"),
        }
    }
}

pub struct ThumbnailWindow {
    pub hwnd: HWND,
    /// Layered window used for both the text overlay and the border.
    pub text_hwnd: HWND,
    pub thumbnail_id: isize,
    pub source_hwnd: HWND,
    pub title: String,
    pub character_name: String,
    pub system_name: String,
    /// In-game timestamp of the event that set system_name (YYYYMMDD*1000000+HHMMSS); 0 = untimestamped source (e.g. live tailing), which always applies.
    pub system_name_event_ts: u64,
    /// Tick of last stargate/conduit jump; zero = hasn't jumped this session.
    pub last_jump: Ticks,
    /// Guards the left-behind alert to one-per-episode; cleared on jump.
    pub travel_alert_fired: bool,
    /// Newest first, at most MAX_STACKED_NOTIFICATIONS; see Painter::push_notification.
    pub active_notifications: Vec<ActiveNotification>,
    pub last_click_time: Ticks,
    /// Tick of the last notification actually shown per type; suppressed attempts don't update this, so throttle_ms anchors to the last one actually displayed.
    last_notification_time_by_type: Vec<Ticks>,
    pub is_excluded_from_cycle: bool,
    pub needs_render: bool,
    /// False in ClientList/Nothing view modes, where hwnd/text_hwnd are sentinels never passed to Win32.
    pub win32_enabled: bool,

    // None means not enough span yet to trust a rate (see activity_tracker).
    pub last_incoming_dps: Option<f32>,
    pub last_outgoing_dps: Option<f32>,
    pub last_mining_rate: Option<f32>,
    pub last_mining_isk_rate: Option<f32>,
    pub last_bounty_isk_rate: Option<f32>,
    pub last_cpu_percent: f32,
    pub last_ram_mb: f32,
    pub last_vram_mb: f32,

    // False until the tracker's first push actually arrives; distinguishes "never heard from the tracker yet" (show nothing) from a genuine None rate the tracker reported (show "??").
    pub has_dps_data: bool,
    pub has_mining_data: bool,
    pub has_bounty_data: bool,
    // has_vram_data is separate: VRAM can stay unavailable (no PDH support, no matching GPU instance) even once CPU/RAM are known.
    pub has_resource_data: bool,
    pub has_vram_data: bool,

    /// Cached overlay bitmap — kept alive between renders, recreated only on resize
    cached_overlay: Option<OverlayBitmap>,

    pub visibility_state: VisibilityState,
    /// Set while a Test Notification has force-shown a hidden thumbnail; restored once its notifications clear.
    test_restore_visibility: Option<VisibilityState>,
    /// When check_auto_minimize's delay should count from; refreshed every tick this thumbnail is Active or Minimized, left untouched otherwise so its frozen value is the moment it last became eligible.
    inactive_since: Ticks,
    /// Edge-detector so a minimize/restore with no accompanying focus change still marks this dirty for repaint.
    was_minimized: bool,

    cached_render_settings: Option<RenderSettings>,
    cached_char_dims: Option<TextDimensions>,
    cached_sys_dims: Option<TextDimensions>,
    cached_badge_dims: Option<TextDimensions>,
    cached_font: (String, i32, FontWeight),
    cached_sys_font: (String, i32, FontWeight),
    cached_badge_font: (String, i32, FontWeight),
    pub cached_system_color: u32,
    /// Auto-generated per-character name color resolved from config; None when "Unique Character Name Colors" is disabled, and callers fall back to their own default.
    pub cached_character_color: Option<u32>,
    // Display name and per-character overrides, resolved from config on character_name change or (re)creation rather than every tick (the list view reads these every ~50ms per thumbnail).
    pub cached_display_name: String,
    pub cached_border_colors: Option<CharacterBorderColors>,
    pub cached_excluded_from_minimize: bool,
    pub cached_hide_thumbnail: bool,
    pub cached_thumbnail_size: Option<CharacterThumbnailSize>,
    pub cached_opacity: u8,
    /// Comma-joined label of the badge-enabled groups this character is in ("1, 3"); "" = none.
    pub cached_group_badge_label: String,
}

fn no_font() -> (String, i32, FontWeight) {
    (String::new(), 0, FontWeight::Regular)
}

impl ThumbnailWindow {
    /// Whether this thumbnail's source_hwnd is the live "who's focused" pointer.
    pub fn is_focused(&self, active_source_hwnd: Option<HWND>) -> bool {
        Some(self.source_hwnd) == active_source_hwnd
    }

    /// The single canonical "what should this render/style as" computation. Used purely as a style-lookup key - never stored back onto the thumbnail.
    pub fn effective_render_state(&self, active_source_hwnd: Option<HWND>) -> ThumbnailState {
        if is_thumbnail_dragging(self) {
            ThumbnailState::Dragging
        } else if !self.active_notifications.is_empty() {
            ThumbnailState::Alert
        } else if self.is_focused(active_source_hwnd) {
            ThumbnailState::Active
        } else if is_window_iconic(self.source_hwnd) {
            ThumbnailState::Minimized
        } else {
            ThumbnailState::Inactive
        }
    }

    /// Sets visibility state, silently refusing an invalid transition.
    pub fn set_visibility(&mut self, new_visibility: VisibilityState) {
        let blocks_hiding = !self.active_notifications.is_empty() || is_thumbnail_dragging(self);
        if new_visibility != VisibilityState::Visible && blocks_hiding {
            SLOG.warn(format_args!("Cannot hide {} while alerting/dragging", self.character_name));
            return;
        }
        self.visibility_state = try_transition_visibility(self.visibility_state, new_visibility, &self.character_name);
    }

    pub fn is_visible(&self) -> bool {
        self.visibility_state.is_visible()
    }
}

/// Per-purpose font cache slot (see Painter::cached_fonts). `Combat` is incoming DPS's slot; outgoing DPS has its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontSlot {
    Main,
    Combat,
    Mining,
    Bounty,
    SystemName,
    GroupBadge,
    Notification,
    CombatOutgoing,
    Resources,
}

struct FontCacheEntry {
    font: HFONT,
    name: String,
    size: i32,
    weight: FontWeight,
}

fn is_character_travel_excluded(character_name: &str) -> bool {
    globals::hotkey_manager().is_some_and(|m| m.is_character_excluded(character_name))
}

/// One entry in the "recently notified" FIFO queue.
struct NotifiedCharacterEntry {
    character_name: String,
    notified_at: Ticks,
}

/// Re-checked after login because EVE can reposition its own window while still loading.
struct PendingAutoMove {
    hwnd: HWND,
    target: Position,
    last_move: Ticks,
    checks_left: u8,
    last_seen: Option<POINT>,
}

/// Registration persists across Painter instances, so only the first ever Painter registers the classes.
static WINDOW_CLASS_REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct Painter {
    pub thumbnails: Vec<ThumbnailWindow>,
    hwnd_to_thumbnail_index: HashMap<HWND, usize>,
    // thumbnail.hwnd → index, for O(1) lookups.
    thumbnail_hwnd_to_index: HashMap<HWND, usize>,
    // thumbnail.text_hwnd → index, for O(1) lookups.
    text_hwnd_to_index: HashMap<HWND, usize>,
    last_hwnd_index_rebuild: Ticks,
    instance: HINSTANCE,
    focus_event_hook: HWINEVENTHOOK,
    destroy_event_hook: HWINEVENTHOOK,
    pub hide_debounce_timer_hwnd: Option<HWND>,
    /// Keyed by (FontSlot, DPI), so different-DPI monitors don't evict each other's fonts every render.
    cached_fonts: HashMap<(FontSlot, u32), FontCacheEntry>,
    /// FIFO queue of recently-notified characters, oldest first; consumed by HotkeyManager::cycle_notified via notified_character_names.
    notified_queue: Vec<NotifiedCharacterEntry>,
    /// Thumbnails hide_thumbnails_for_region_select hid, so the restore only re-shows exactly those (not ones already manually hidden beforehand).
    region_select_hidden_hwnds: Vec<HWND>,
    pending_auto_moves: Vec<PendingAutoMove>,
    /// The last NOTIF_HISTORY_CAPACITY notifications shown, across all characters, oldest first.
    notification_history: std::collections::VecDeque<NotificationHistoryEntry>,
    /// Transient overlay shown only while dragging, outlining other characters' saved positions; created lazily, hidden (not destroyed) between drags.
    ghost_overlay_hwnd: Option<HWND>,
    ghost_overlay_bitmap: Option<OverlayBitmap>,
    /// Ghost groups computed once by show_ghost_overlay at drag-start; input's apply_ghost_snapping reuses this for the rest of the drag instead of recomputing on every mouse move.
    pub current_drag_ghost_groups: Option<Vec<GhostGroup>>,
    /// Transient hint box shown only while dragging; created lazily, hidden (not destroyed) between drags.
    drag_hint_hwnd: Option<HWND>,
    drag_hint_bitmap: Option<OverlayBitmap>,
    /// Sole "who's focused" source of truth; write only via reconcile_thumbnail_states.
    pub active_source_hwnd: Option<HWND>,
    /// Last EVE client hwnd that held focus on each monitor; used by check_auto_minimize's exemptLastActiveOnFocusLoss option to spare one client per monitor once EVE itself has no window focused.
    last_focused_by_monitor: HashMap<HMONITOR, HWND>,
    /// Most recent foreground window that belongs to neither an EVE client nor this process; used by the return-to-last-app hotkey.
    pub last_non_eve_foreground: Option<HWND>,
    /// Character -> Account Config account, for Display Regions' account cells; re-read from accounts.json on each grid reflow so the dialog's edits apply without a restart.
    account_membership: accounts_store::Membership,
}

/// Builds a DWM_THUMBNAIL_PROPERTIES sized to (width, height); rcSource stays zeroed (whole source window) on every caller.
fn make_thumbnail_props(width: i32, height: i32, flags: u32) -> DWM_THUMBNAIL_PROPERTIES {
    DWM_THUMBNAIL_PROPERTIES {
        dwFlags: flags,
        rcDestination: RECT { left: 0, top: 0, right: width, bottom: height },
        rcSource: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        opacity: 255,
        fVisible: 1,
        fSourceClientAreaOnly: 1,
    }
}

/// Toggles WS_EX_TRANSPARENT on an already-created window, so clickThrough can change live without recreating it.
fn set_click_through_style(hwnd: HWND, enabled: bool) {
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style = if enabled { current | WS_EX_TRANSPARENT as isize } else { current & !(WS_EX_TRANSPARENT as isize) };
        if new_style != current {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
        }
    }
}

impl Painter {
    pub fn new() -> Result<Painter, CreateError> {
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };

        let mut painter = Painter {
            thumbnails: Vec::new(),
            hwnd_to_thumbnail_index: HashMap::new(),
            thumbnail_hwnd_to_index: HashMap::new(),
            text_hwnd_to_index: HashMap::new(),
            last_hwnd_index_rebuild: Ticks::default(),
            instance,
            focus_event_hook: std::ptr::null_mut(),
            destroy_event_hook: std::ptr::null_mut(),
            hide_debounce_timer_hwnd: None,
            cached_fonts: HashMap::new(),
            notified_queue: Vec::new(),
            region_select_hidden_hwnds: Vec::new(),
            pending_auto_moves: Vec::new(),
            notification_history: std::collections::VecDeque::with_capacity(NOTIF_HISTORY_CAPACITY),
            ghost_overlay_hwnd: None,
            ghost_overlay_bitmap: None,
            current_drag_ghost_groups: None,
            drag_hint_hwnd: None,
            drag_hint_bitmap: None,
            active_source_hwnd: None,
            last_focused_by_monitor: HashMap::new(),
            last_non_eve_foreground: None,
            account_membership: accounts_store::load_membership(),
        };

        painter.register_window_classes()?;

        painter.focus_event_hook = unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, std::ptr::null_mut(), Some(win_event_proc), 0, 0, WINEVENT_OUTOFCONTEXT) };
        if painter.focus_event_hook.is_null() {
            SLOG.err(format_args!("Failed to set up focus event hook"));
        }
        painter.destroy_event_hook = unsafe { SetWinEventHook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_DESTROY, std::ptr::null_mut(), Some(window_destroy_proc), 0, 0, WINEVENT_OUTOFCONTEXT) };
        if painter.destroy_event_hook.is_null() {
            SLOG.err(format_args!("Failed to set up destroy event hook"));
        }

        let display = &globals::config().display;
        if display.view_mode == ViewMode::ClientList {
            SLOG.warn(format_args!("Client List view is not available in this build yet; tracking clients without a list window"));
        }
        if display.show_notif_info_panel {
            SLOG.warn(format_args!("The History Panel is not available in this build yet"));
        }
        Ok(painter)
    }

    fn register_window_classes(&self) -> Result<(), CreateError> {
        if WINDOW_CLASS_REGISTERED.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        // Black, not white COLOR_WINDOW: shows through whenever DWM has no live thumbnail frame to composite.
        let brush = unsafe { CreateSolidBrush(0x0000_0000) };
        let ok = gdi_overlay::register_window_class(self.instance, Some(input::window_proc), WINDOW_CLASS_NAME, brush)
            // No background brush for a layered window.
            && gdi_overlay::register_window_class(self.instance, Some(input::text_window_proc), TEXT_WINDOW_CLASS_NAME, std::ptr::null_mut())
            && gdi_overlay::register_window_class(self.instance, Some(DefWindowProcW), GHOST_WINDOW_CLASS_NAME, std::ptr::null_mut());
        if !ok {
            return Err(CreateError::CreateWindowFailed);
        }
        WINDOW_CLASS_REGISTERED.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Gets or creates the cached font for the given (slot, dpi), recreating it only if its settings changed.
    fn cached_font(&mut self, slot: FontSlot, dpi: u32, name: &str, size: i32, weight: FontWeight) -> Result<HFONT, RenderError> {
        if let Some(entry) = self.cached_fonts.get(&(slot, dpi)) {
            if entry.name == name && entry.size == size && entry.weight == weight {
                return Ok(entry.font);
            }
            unsafe { DeleteObject(entry.font) };
            self.cached_fonts.remove(&(slot, dpi));
            SLOG.debug(format_args!("Font cache invalidated for slot {slot:?} @ {dpi} DPI (settings changed)"));
        }
        let font = gdi_overlay::create_font(name, size, weight);
        if font.is_null() {
            return Err(RenderError::CreateFontFailed);
        }
        self.cached_fonts.insert((slot, dpi), FontCacheEntry { font, name: name.to_owned(), size, weight });
        SLOG.debug(format_args!("Created cached font for slot {slot:?} @ {dpi} DPI: {name} size={size} weight={weight:?}"));
        Ok(font)
    }

    /// Single point for rendering any thumbnail overlay; skips the re-render when RenderSettings haven't changed.
    pub fn render_thumbnail(&mut self, index: usize) -> Result<(), RenderError> {
        let thumbnail = &self.thumbnails[index];
        // ClientList mode renders via the list window instead; Nothing mode renders nothing
        if !thumbnail.win32_enabled {
            return Ok(());
        }
        let settings = create_render_settings(globals::config(), thumbnail, self.active_source_hwnd);
        let (hwnd, text_hwnd) = (thumbnail.hwnd, thumbnail.text_hwnd);
        let show = |visible: bool| unsafe {
            let cmd = if visible { SW_SHOW } else { SW_HIDE };
            ShowWindow(hwnd, cmd);
            ShowWindow(text_hwnd, cmd);
        };

        if let Some(cached) = &thumbnail.cached_render_settings {
            if cached == &settings {
                return Ok(());
            }
            // Only visibility changed? Just show/hide windows without re-rendering
            if cached.only_visibility_changed(&settings) {
                show(settings.show_thumbnail);
                self.thumbnails[index].cached_render_settings = Some(settings);
                return Ok(());
            }
        }

        show(settings.show_thumbnail);
        if settings.show_thumbnail {
            self.render_thumbnail_overlay(index, &settings)?;
        }
        self.thumbnails[index].cached_render_settings = Some(settings);
        Ok(())
    }

    /// render_thumbnail, logging (not propagating) a failure with context folded into the message.
    pub fn render_thumbnail_logged(&mut self, index: usize, context: &str) {
        if let Err(err) = self.render_thumbnail(index) {
            SLOG.err(format_args!("Failed to render thumbnail for {} ({context}): {err}", self.thumbnails[index].character_name));
        }
    }

    /// Destroys a single thumbnail's windows and DWM registration; the overlay bitmap and child windows go before the parent window.
    fn destroy_thumbnail_resources(thumbnail: &mut ThumbnailWindow) {
        if thumbnail.win32_enabled {
            // GDI resources must be freed before window destruction
            thumbnail.cached_overlay = None;
            unsafe {
                DestroyWindow(thumbnail.text_hwnd);
                DwmUnregisterThumbnail(thumbnail.thumbnail_id);
                DestroyWindow(thumbnail.hwnd);
            }
        }
    }

    pub fn has_thumbnail(&self, source_hwnd: HWND) -> bool {
        self.hwnd_to_thumbnail_index.contains_key(&source_hwnd)
    }

    fn remove_thumbnail_at(&mut self, index: usize) {
        let mut thumbnail = self.thumbnails.remove(index);
        self.hwnd_to_thumbnail_index.remove(&thumbnail.source_hwnd);
        self.thumbnail_hwnd_to_index.remove(&thumbnail.hwnd);
        self.text_hwnd_to_index.remove(&thumbnail.text_hwnd);
        Self::destroy_thumbnail_resources(&mut thumbnail);
    }

    /// Remove thumbnails whose source windows are gone; returns whether the survivors need a layout reflow.
    pub fn cleanup_closed_thumbnails(&mut self, closed_windows: &[ClosedWindow]) -> bool {
        // By source_hwnd, not name: multiple windows can share a name (e.g. "EVE").
        let mut removed_any = false;
        for cw in closed_windows {
            if let Some(i) = self.thumbnails.iter().position(|t| t.source_hwnd == cw.hwnd) {
                SLOG.info(format_args!("Cleaning up closed thumbnail for {}", self.thumbnails[i].character_name));
                self.remove_thumbnail_at(i);
                removed_any = true;
            }
        }
        if removed_any {
            self.rebuild_hwnd_index(false);
            if self.thumbnails.is_empty() {
                globals::config().flush_auto_colors();
            }
        }
        // A logout must reflow the survivors to refill the region.
        removed_any && is_layout_managed(&globals::config().display)
    }

    /// Rebuilds all HWND → index mappings; call after removing thumbnails to keep indices consistent. `force` bypasses the rate limit when the caller needs a correct index immediately.
    fn rebuild_hwnd_index(&mut self, force: bool) {
        let now = Ticks::now();
        if !force && now.elapsed_since(self.last_hwnd_index_rebuild) < 100 {
            SLOG.debug(format_args!("Skipping HWND index rebuild (rate limited: {}ms since last rebuild)", now.elapsed_since(self.last_hwnd_index_rebuild)));
            return;
        }
        self.last_hwnd_index_rebuild = now;
        SLOG.debug(format_args!("Rebuilding HWND index for {} thumbnails...", self.thumbnails.len()));

        self.hwnd_to_thumbnail_index.clear();
        self.thumbnail_hwnd_to_index.clear();
        self.text_hwnd_to_index.clear();
        for (i, t) in self.thumbnails.iter().enumerate() {
            self.hwnd_to_thumbnail_index.insert(t.source_hwnd, i);
            // Thumbnail / text window HWNDs only exist in Thumbnails view mode
            if t.win32_enabled {
                self.thumbnail_hwnd_to_index.insert(t.hwnd, i);
                self.text_hwnd_to_index.insert(t.text_hwnd, i);
            }
        }
    }

    fn overlay_index_matches(&self, index: usize, hwnd: HWND) -> bool {
        self.thumbnails.get(index).is_some_and(|t| t.hwnd == hwnd || t.text_hwnd == hwnd)
    }

    /// Resolves hwnd to its thumbnails[] index; only matches Painter's own thumbnail/text windows, since a source EVE window closing is Scout's call.
    fn resolve_thumbnail_index_for_destroy(&mut self, hwnd: HWND) -> Option<usize> {
        let lookup = |p: &Painter| p.thumbnail_hwnd_to_index.get(&hwnd).or_else(|| p.text_hwnd_to_index.get(&hwnd)).copied();
        let raw = lookup(self)?;
        if self.overlay_index_matches(raw, hwnd) {
            return Some(raw);
        }
        self.rebuild_hwnd_index(true);
        let retry = lookup(self)?;
        self.overlay_index_matches(retry, hwnd).then_some(retry)
    }

    /// Index of the thumbnail for a source EVE window with O(1) lookup; rebuilds the index and retries once if the entry is stale.
    pub fn index_by_source(&mut self, source_hwnd: HWND) -> Option<usize> {
        let index = *self.hwnd_to_thumbnail_index.get(&source_hwnd)?;
        if self.thumbnails.get(index).is_some_and(|t| t.source_hwnd == source_hwnd) {
            return Some(index);
        }
        SLOG.warn(format_args!("HWND index mismatch for {source_hwnd:?} at index {index}. Rebuilding index..."));
        // Force past the rate limit: a mismatch means the map is stale right now, not just due for its next routine rebuild.
        self.rebuild_hwnd_index(true);
        let retry = *self.hwnd_to_thumbnail_index.get(&source_hwnd)?;
        self.thumbnails.get(retry).is_some_and(|t| t.source_hwnd == source_hwnd).then_some(retry)
    }

    /// Index of the thumbnail owning an overlay (thumbnail or text) window; resolved fresh each call.
    pub fn index_by_overlay(&self, hwnd: HWND) -> Option<usize> {
        let index = *self.thumbnail_hwnd_to_index.get(&hwnd).or_else(|| self.text_hwnd_to_index.get(&hwnd))?;
        self.overlay_index_matches(index, hwnd).then_some(index)
    }

    /// Comma-joined names of the badge-enabled groups `character_name` belongs to; "" when none.
    fn build_group_badge_label(character_name: &str) -> String {
        globals::config()
            .hotkey_groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.show_badge && g.characters.iter().any(|c| c == character_name))
            .map(|(i, g)| if g.name.is_empty() { (i + 1).to_string() } else { g.name.clone() })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Recompute and cache a thumbnail's group badge label after its membership changed.
    pub fn refresh_group_badge(&mut self, index: usize) {
        let t = &mut self.thumbnails[index];
        t.cached_group_badge_label = Self::build_group_badge_label(&t.character_name);
        t.cached_badge_dims = None;
    }

    /// Re-resolves every config-derived cache field for one thumbnail from its current character name.
    fn refresh_character_caches(&mut self, index: usize) {
        let config = globals::config();
        let name = self.thumbnails[index].character_name.clone();
        let character_color = config.character_name_color(&name);
        let border_colors = config.character_border_colors(&name);
        let t = &mut self.thumbnails[index];
        t.cached_character_color = character_color;
        t.cached_display_name = config.display_name(&name).to_owned();
        t.cached_border_colors = border_colors;
        t.cached_excluded_from_minimize = config.is_excluded_from_minimize(&name);
        t.cached_hide_thumbnail = config.is_thumbnail_hidden(&name);
        t.cached_thumbnail_size = config.character_size(&name);
        t.cached_opacity = config.character_opacity(&name);
        self.refresh_group_badge(index);
    }

    /// Reconciles focus, then refreshes minimized-state bookkeeping (inactive_since, dirty-on-minimize-change); call periodically from the timer.
    pub fn update_thumbnail_states(&mut self) {
        if self.thumbnails.is_empty() {
            return;
        }
        let foreground = unsafe { GetForegroundWindow() };
        self.reconcile_thumbnail_states((!foreground.is_null()).then_some(foreground));

        let now = Ticks::now();
        let active = self.active_source_hwnd;
        for t in &mut self.thumbnails {
            if is_thumbnail_dragging(t) {
                continue;
            }
            let is_minimized = is_window_iconic(t.source_hwnd);
            if t.is_focused(active) || is_minimized {
                t.inactive_since = now;
            }
            if is_minimized != t.was_minimized {
                t.was_minimized = is_minimized;
                t.needs_render = true;
            }
        }
    }

    /// A monitor with no live recorded last-active client exempts everyone on it rather than minimize clients with no known "last active".
    fn is_last_active_on_its_monitor(&self, source_hwnd: HWND) -> bool {
        let monitor = unsafe { MonitorFromWindow(source_hwnd, MONITOR_DEFAULTTONEAREST) };
        if monitor.is_null() {
            return true;
        }
        let Some(&last_active) = self.last_focused_by_monitor.get(&monitor) else { return true };
        if !self.hwnd_to_thumbnail_index.contains_key(&last_active) {
            return true;
        }
        last_active == source_hwnd
    }

    fn record_last_focused(&mut self, source_hwnd: HWND) {
        let monitor = unsafe { MonitorFromWindow(source_hwnd, MONITOR_DEFAULTTONEAREST) };
        if monitor.is_null() {
            return;
        }
        // A window that moved monitors must not stay recorded under its old one.
        self.last_focused_by_monitor.retain(|&m, &mut h| !(h == source_hwnd && m != monitor));
        self.last_focused_by_monitor.insert(monitor, source_hwnd);
    }

    /// Minimizes each EVE window `autoMinimize.delayMs` after it last stopped being Active/Minimized (see inactive_since); call once per tick. Each monitor's last-active client is spared while focus is on another monitor.
    fn check_auto_minimize(&mut self) {
        let auto = &globals::config().auto_minimize;
        if !auto.enabled || self.thumbnails.is_empty() {
            return;
        }
        let now = Ticks::now();
        let delay_ms = auto.delay_ms as u64;
        let mut minimized_any = false;

        // active_source_hwnd is the literal foreground window, so it's Some even on a non-EVE app.
        let eve_has_focus = self.active_source_hwnd.is_some_and(|h| self.hwnd_to_thumbnail_index.contains_key(&h));
        let focused_monitor = if eve_has_focus { Some(unsafe { MonitorFromWindow(self.active_source_hwnd.unwrap(), MONITOR_DEFAULTTONEAREST) }) } else { None };

        for t in &self.thumbnails {
            if is_thumbnail_dragging(t) || t.is_focused(self.active_source_hwnd) || is_window_iconic(t.source_hwnd) {
                continue;
            }
            // Checked after the iconic skip: a minimized window is parked off-screen and reports the wrong monitor.
            let on_other_monitor = focused_monitor.is_some_and(|m| unsafe { MonitorFromWindow(t.source_hwnd, MONITOR_DEFAULTTONEAREST) } != m);
            let spared_by_focus_loss = auto.exempt_last_active_on_focus_loss && !eve_has_focus;
            if (on_other_monitor || spared_by_focus_loss) && self.is_last_active_on_its_monitor(t.source_hwnd) {
                continue;
            }
            if now.elapsed_since(t.inactive_since) < delay_ms || t.cached_excluded_from_minimize || !is_window(t.source_hwnd) {
                continue;
            }
            unsafe { ShowWindowAsync(t.source_hwnd, SW_FORCEMINIMIZE) };
            minimized_any = true;
            SLOG.info(format_args!("Auto-minimized {} (inactive {}ms)", t.character_name, now.elapsed_since(t.inactive_since)));
        }

        if minimized_any {
            if let Some(t) = self.thumbnails.iter().find(|t| t.is_focused(self.active_source_hwnd) && is_window(t.source_hwnd)) {
                // Minimizing the other windows can transiently steal focus from the active one.
                input::force_set_foreground_window(t.source_hwnd);
            }
        }
    }

    fn eve_windows_or_log(action: &str) -> Option<&'static [EveWindow]> {
        match globals::scout() {
            Some(scout) => Some(&scout.windows),
            None => {
                SLOG.err(format_args!("Scout not available for {action}"));
                None
            }
        }
    }

    /// Minimize all EVE client windows regardless of their current state (hotkey action).
    pub fn minimize_all_clients(&self) {
        if let Some(windows) = Self::eve_windows_or_log("minimize all clients") {
            manager::minimize_all_clients(windows);
        }
    }

    /// Move all EVE client windows with a saved position to that position (hotkey action).
    pub fn move_all_clients_to_saved_positions(&self) {
        if let Some(windows) = Self::eve_windows_or_log("move all clients to saved positions") {
            manager::move_all_clients_to_saved_positions(windows, globals::config());
        }
    }

    /// Close all EVE client windows except those in the exclude list (hotkey action).
    pub fn close_all_clients(&self) {
        if let Some(windows) = Self::eve_windows_or_log("close all clients") {
            manager::close_all_clients(windows, globals::config());
        }
    }

    /// Toggle all thumbnails between hidden and visible, preserving active/inactive state (hotkey action).
    pub fn toggle_all_thumbnails_visibility(&mut self) {
        let Some(first) = self.thumbnails.first() else {
            SLOG.debug(format_args!("No thumbnails to toggle visibility"));
            return;
        };
        let new_visibility = if first.visibility_state == VisibilityState::Visible { VisibilityState::HiddenManual } else { VisibilityState::Visible };
        SLOG.info(format_args!("Toggling all thumbnails visibility: {new_visibility:?}"));
        // Manual hiding persists through focus changes
        for i in 0..self.thumbnails.len() {
            self.thumbnails[i].set_visibility(new_visibility);
            self.render_thumbnail_logged(i, "visibility toggle");
        }
    }

    /// Hides every visible thumbnail automatically (they can be auto-shown again when EVE gets focus); the hide-debounce timer's action.
    pub fn hide_all_automatically(&mut self) {
        for i in 0..self.thumbnails.len() {
            if self.thumbnails[i].visibility_state == VisibilityState::Visible {
                self.thumbnails[i].set_visibility(VisibilityState::HiddenAutomatic);
                if let Err(err) = self.render_thumbnail(i) {
                    SLOG.err(format_args!("Failed to hide thumbnail: {err}"));
                }
            }
        }
    }

    /// Hides every currently-visible thumbnail so it doesn't obscure the "Start Region Selection" overlay.
    pub fn hide_thumbnails_for_region_select(&mut self) {
        self.region_select_hidden_hwnds.clear();
        for i in 0..self.thumbnails.len() {
            if !self.thumbnails[i].is_visible() {
                continue;
            }
            self.thumbnails[i].set_visibility(VisibilityState::HiddenManual);
            // set_visibility silently refused (alerting/dragging) - nothing to restore later.
            if self.thumbnails[i].is_visible() {
                continue;
            }
            self.region_select_hidden_hwnds.push(self.thumbnails[i].hwnd);
            self.render_thumbnail_logged(i, "region select hide");
        }
    }

    /// Restores visibility for thumbnails hide_thumbnails_for_region_select hid.
    pub fn restore_thumbnails_after_region_select(&mut self) {
        for hwnd in std::mem::take(&mut self.region_select_hidden_hwnds) {
            let Some(&i) = self.thumbnail_hwnd_to_index.get(&hwnd) else { continue };
            self.thumbnails[i].set_visibility(VisibilityState::Visible);
            self.render_thumbnail_logged(i, "region select restore");
        }
    }

    /// Toggle auto-minimize mode temporarily, without persisting to config (hotkey action).
    pub fn toggle_auto_minimize(&self) {
        let auto = &mut globals::config().auto_minimize;
        auto.enabled = !auto.enabled;
        SLOG.info(format_args!("Auto-minimize toggled: {}", if auto.enabled { "enabled" } else { "disabled" }));
    }

    /// Sole writer of active_source_hwnd, the single source of truth for who's focused; call instead of setting it directly.
    pub fn reconcile_thumbnail_states(&mut self, should_be_active: Option<HWND>) {
        let old_active = self.active_source_hwnd;
        self.active_source_hwnd = should_be_active;
        let active_changed = old_active != should_be_active;
        let any_eve_has_focus = should_be_active.is_some_and(|h| self.hwnd_to_thumbnail_index.contains_key(&h));
        if any_eve_has_focus {
            self.record_last_focused(should_be_active.unwrap());
        }

        for t in &mut self.thumbnails {
            // Unhide automatically-hidden thumbnails when EVE gains focus; manual hiding persists until the user toggles visibility.
            if t.visibility_state == VisibilityState::HiddenAutomatic && any_eve_has_focus {
                t.set_visibility(VisibilityState::Visible);
                t.needs_render = true;
            }
            if active_changed && (Some(t.source_hwnd) == old_active || Some(t.source_hwnd) == should_be_active) {
                t.needs_render = true;
            }
        }
    }

    /// Updates the system name for a character (O(1) lookup); see ThumbnailWindow::system_name_event_ts and last_jump for `event_ts`/`is_jump`.
    pub fn update_system_name_by_hwnd(&mut self, source_hwnd: HWND, system_name: &str, event_ts: u64, is_jump: bool) {
        let index = match self.index_by_source(source_hwnd) {
            Some(i) => i,
            None => {
                // Window not found on first attempt - defensively rebuild HWND index and retry
                SLOG.debug(format_args!("Window {source_hwnd:?} not found for system update, rebuilding HWND index..."));
                self.rebuild_hwnd_index(true);
                match self.index_by_source(source_hwnd) {
                    Some(i) => {
                        SLOG.info(format_args!("Successfully found window {source_hwnd:?} after index rebuild for {}", self.thumbnails[i].character_name));
                        i
                    }
                    None => {
                        SLOG.warn(format_args!("Window {source_hwnd:?} not found even after index rebuild (thumbnail may not exist)"));
                        SLOG.debug(format_args!("Currently tracking {} thumbnails:", self.thumbnails.len()));
                        for t in &self.thumbnails {
                            SLOG.debug(format_args!("  - {}: source_hwnd={:?}", t.character_name, t.source_hwnd));
                        }
                        return;
                    }
                }
            }
        };

        let t = &mut self.thumbnails[index];
        if event_ts != 0 && event_ts < t.system_name_event_ts {
            SLOG.debug(format_args!("Ignoring stale system update for {}: {system_name} (event_ts={event_ts} < current={})", t.character_name, t.system_name_event_ts));
            return;
        }
        t.system_name = system_name.to_owned();
        t.system_name_event_ts = event_ts;
        t.cached_system_color = globals::config().system_name_color(system_name);
        t.cached_sys_dims = None;
        SLOG.debug(format_args!("System '{system_name}' color resolved to: 0x{:06X}", t.cached_system_color & 0xFF_FFFF));
        if is_jump {
            t.last_jump = Ticks::now();
            t.travel_alert_fired = false;
        }
        t.needs_render = true;
        SLOG.debug(format_args!("Updated system for {}: {system_name}", t.character_name));
    }

    pub fn show_notification(&mut self, source_hwnd: HWND, notification_text: &str, notification_type: NotificationType) {
        let Some(index) = self.index_by_source(source_hwnd) else {
            // This can happen if the thumbnail hasn't been created yet
            SLOG.debug(format_args!("Window {source_hwnd:?} not found for notification update (thumbnail may not exist yet)"));
            return;
        };
        let config = globals::config();
        let notifications = &config.thumbnail.notifications;
        if !notifications.enabled || config.is_notification_muted(&self.thumbnails[index].character_name) {
            return;
        }

        let type_config = notifications.type_config(notification_type).clone();
        if !type_config.enabled {
            return;
        }
        let t = &mut self.thumbnails[index];
        if type_config.suppress_when_focused && t.is_focused(self.active_source_hwnd) {
            return;
        }
        let now = Ticks::now();
        if type_config.suppress_when_clicked && now.elapsed_since(t.last_click_time) < notifications.suppress_click_duration_ms as u64 {
            return;
        }
        if type_config.throttle_ms > 0 {
            let last = t.last_notification_time_by_type[notification_type.index()];
            if !last.is_zero() && now.elapsed_since(last) < type_config.throttle_ms as u64 {
                return;
            }
        }
        t.last_notification_time_by_type[notification_type.index()] = now;

        self.push_notification(index, Self::notification_from(&type_config, notification_text, notification_type, now, type_config.duration_ms));

        let name = self.thumbnails[index].character_name.clone();
        self.track_notified_character(&name);
        self.push_notification_history(source_hwnd, &name, notification_text, notification_type);

        let spoken_name = (notifications.tts_speak_character_name && !name.is_empty())
            .then(|| if notifications.tts_use_display_name { self.thumbnails[index].cached_display_name.clone() } else { name.clone() });
        Self::play_alert_effects(&type_config, notification_text, spoken_name.as_deref());

        SLOG.debug(format_args!(
            "Queued notification for {name}: [{}] {notification_text} (border_color_override: {:?})",
            notification_type.as_str(),
            type_config.border_color
        ));
    }

    fn notification_from(type_config: &NotificationTypeConfig, text: &str, notification_type: NotificationType, now: Ticks, duration_ms: u32) -> ActiveNotification {
        ActiveNotification {
            text: text.to_owned(),
            notification_type,
            start_time: now,
            duration_ms,
            suppress_when_focused: type_config.suppress_when_focused,
            suppress_when_clicked: type_config.suppress_when_clicked,
            border_color_override: type_config.border_color,
            text_color_override: type_config.text_color,
            show_border: type_config.show_border,
            flash_border: type_config.flash_border,
        }
    }

    /// Speaks the same phrase the visual notification shows and plays its sound; both are self-contained, with no global master switch or shared volume.
    fn play_alert_effects(type_config: &NotificationTypeConfig, text: &str, spoken_name: Option<&str>) {
        if type_config.tts_enabled {
            let notifications = &globals::config().thumbnail.notifications;
            tts::set_voice_settings(notifications.tts_volume, notifications.tts_rate);
            match spoken_name {
                Some(name) => tts::speak_alert(&format!("{name}, {text}")),
                None => tts::speak_alert(text),
            }
        }
        if type_config.sound_enabled {
            if let Some(path) = &type_config.sound_path {
                sound::play_alert(path, type_config.sound_volume);
            }
        }
    }

    /// Config dialog's "Test Notification": bypasses every suppression, force-shows hidden thumbnails for its duration, and skips history/cycle tracking; alerts play once rather than per thumbnail.
    pub fn show_test_notification(&mut self, notification_type: NotificationType, text: &str, type_config: &NotificationTypeConfig) {
        let now = Ticks::now();
        // A permanent (0) duration would never clear a test.
        let duration_ms = if type_config.duration_ms == 0 { TEST_NOTIFICATION_PERMANENT_FALLBACK_MS } else { type_config.duration_ms };
        for i in 0..self.thumbnails.len() {
            self.push_notification(i, Self::notification_from(type_config, text, notification_type, now, duration_ms));
            // The alert blocks re-hiding, so the thumbnail stays up until update_notifications() restores it.
            let t = &mut self.thumbnails[i];
            if !t.is_visible() {
                if t.test_restore_visibility.is_none() {
                    t.test_restore_visibility = Some(t.visibility_state);
                }
                t.set_visibility(VisibilityState::Visible);
                self.render_thumbnail_logged(i, "test notification show");
            }
        }
        Self::play_alert_effects(type_config, text, None);
    }

    /// Puts a thumbnail that a Test Notification force-showed back to its prior visibility.
    fn restore_visibility_after_test(&mut self, index: usize) {
        let Some(prior) = self.thumbnails[index].test_restore_visibility.take() else { return };
        // Focus or the setting may have changed during the test, in which case auto-hiding no longer applies.
        let restored = match prior {
            VisibilityState::HiddenAutomatic if globals::config().thumbnail.hide_when_no_eve_focus && !self.is_eve_window_foreground() => VisibilityState::HiddenAutomatic,
            VisibilityState::HiddenAutomatic => VisibilityState::Visible,
            other => other,
        };
        self.thumbnails[index].set_visibility(restored);
        self.render_thumbnail_logged(index, "test notification restore");
    }

    fn is_eve_window_foreground(&self) -> bool {
        let foreground = unsafe { GetForegroundWindow() };
        !foreground.is_null() && self.hwnd_to_thumbnail_index.contains_key(&foreground)
    }

    /// Flags characters behind the group's current system by more than config.travel.window_seconds.
    pub fn check_travel_left_behind(&mut self, now: Ticks) {
        let cfg = globals::config().travel.clone();
        if !cfg.enabled {
            return;
        }
        let eligible = |t: &ThumbnailWindow| !t.last_jump.is_zero() && !is_character_travel_excluded(&t.character_name);
        let eligible_count = self.thumbnails.iter().filter(|t| eligible(t)).count();
        if eligible_count < 2 {
            return;
        }

        let mut group_system = String::new();
        let mut group_count = 0usize;
        let mut group_arrival = Ticks::default();
        for candidate in self.thumbnails.iter().filter(|t| eligible(t)) {
            let mut count = 0;
            let mut arrival = Ticks::default();
            for other in self.thumbnails.iter().filter(|t| eligible(t) && t.system_name == candidate.system_name) {
                count += 1;
                if other.last_jump.ms > arrival.ms {
                    arrival = other.last_jump;
                }
            }
            if count > group_count {
                group_count = count;
                group_system = candidate.system_name.clone();
                group_arrival = arrival;
            }
        }
        if group_count == 0 {
            return;
        }

        let required = match cfg.threshold_mode {
            eve_maj_core::config::TravelThresholdMode::Percent => (cfg.threshold_percent / 100.0 * eligible_count as f32).ceil() as usize,
            eve_maj_core::config::TravelThresholdMode::Count => cfg.threshold_count as usize,
        };
        if group_count < required {
            return;
        }
        if now.elapsed_since(group_arrival) < cfg.window_seconds as u64 * 1000 {
            return;
        }

        let behind: Vec<(HWND, String)> = self
            .thumbnails
            .iter_mut()
            .filter(|t| !t.last_jump.is_zero() && !is_character_travel_excluded(&t.character_name) && t.system_name != group_system && !t.travel_alert_fired)
            .map(|t| {
                t.travel_alert_fired = true;
                (t.source_hwnd, t.system_name.clone())
            })
            .collect();
        for (source_hwnd, system) in behind {
            self.show_notification(source_hwnd, &format!("Left behind in {system}"), NotificationType::TravelLeftBehind);
        }
    }

    /// Inserts `entry` at the front of the thumbnail's notification stack (newest first). Replaces any existing entry of the
    /// same type in place (bump-to-top) and evicts the oldest entry once the stack is at MAX_STACKED_NOTIFICATIONS.
    pub fn push_notification(&mut self, index: usize, entry: ActiveNotification) {
        let stack = &mut self.thumbnails[index].active_notifications;
        if let Some(i) = stack.iter().position(|n| n.notification_type == entry.notification_type) {
            stack.remove(i);
        }
        stack.truncate(MAX_STACKED_NOTIFICATIONS - 1);
        stack.insert(0, entry);
        self.thumbnails[index].needs_render = true;
    }

    /// Removes every stacked notification with suppress_when_clicked set (used by input's click handler). Returns whether anything was removed.
    pub fn dismiss_click_suppressed_notifications(&mut self, index: usize) -> bool {
        let t = &mut self.thumbnails[index];
        let before = t.active_notifications.len();
        t.active_notifications.retain(|n| !n.suppress_when_clicked);
        let removed_any = t.active_notifications.len() != before;
        if removed_any {
            t.needs_render = true;
        }
        removed_any
    }

    /// Pushes/bumps character_name into the "recently notified" FIFO used by the cycle-to-notified-character hotkey; re-notifying bumps to the back instead of duplicating.
    fn track_notified_character(&mut self, character_name: &str) {
        let now = Ticks::now();
        if let Some(i) = self.notified_queue.iter().position(|e| e.character_name == character_name) {
            self.notified_queue.remove(i);
        }
        self.notified_queue.push(NotifiedCharacterEntry { character_name: character_name.to_owned(), notified_at: now });
    }

    /// "Recently notified" names within retention_ms, oldest first.
    pub fn notified_character_names(&self, retention_ms: u64) -> Vec<String> {
        let now = Ticks::now();
        self.notified_queue.iter().filter(|e| now.elapsed_since(e.notified_at) <= retention_ms).map(|e| e.character_name.clone()).collect()
    }

    /// Re-applies opacity and forces a redraw (and resize if needed) of every thumbnail from the current config, unconditionally (ignoring needs_render), for the config dialog's live preview.
    pub fn refresh_all_thumbnail_visuals(&mut self) {
        // Re-evaluate focus against the current foreground window here so a live-preview toggle of hideWhenNoEveFocus reacts immediately instead of waiting for the next focus-change WinEvent.
        let any_eve_has_focus = self.is_eve_window_foreground();
        let config = globals::config();
        let cfg = &config.display;
        // region/grid are invariant across every thumbnail this pass; compute once instead of per-thumbnail.
        let region_fit_grid = is_region_fit_active(cfg).then(|| {
            let region = region_rect_from_config(cfg).expect("checked by is_region_fit_active");
            calculate_region_fit_grid(region, self.region_fit_grid_count(), cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region))
        });

        for i in 0..self.thumbnails.len() {
            // Must run for every thumbnail, not just win32_enabled ones: the list view reads these cache fields directly.
            let system_color = if self.thumbnails[i].system_name.is_empty() {
                config.thumbnail.system_name_color
            } else {
                config.system_name_color(&self.thumbnails[i].system_name.clone())
            };
            self.thumbnails[i].cached_system_color = system_color;
            self.refresh_character_caches(i);
            let t = &mut self.thumbnails[i];
            // RenderSettings' equality check can miss a change to one of the resolved fields above; force the full-render path since this only runs on debounced (~120ms) preview edits.
            t.cached_render_settings = None;
            // Force re-measurement: a display-name-only edit changes the string without touching the font, which is otherwise the only re-measure trigger.
            t.cached_char_dims = None;
            t.cached_sys_dims = None;
            t.cached_badge_dims = None;
            t.cached_font = no_font();
            t.cached_sys_font = no_font();
            t.cached_badge_font = no_font();

            if !t.win32_enabled {
                continue;
            }
            if config.thumbnail.hide_when_no_eve_focus && !any_eve_has_focus {
                if t.visibility_state == VisibilityState::Visible {
                    t.set_visibility(VisibilityState::HiddenAutomatic);
                }
            } else if t.visibility_state == VisibilityState::HiddenAutomatic {
                t.set_visibility(VisibilityState::Visible);
            }

            // thumbnailOpacity is otherwise only applied once, at window creation time.
            unsafe { SetLayeredWindowAttributes(t.hwnd, 0, t.cached_opacity, LWA_ALPHA) };
            set_click_through_style(t.hwnd, config.interaction.click_through);
            set_click_through_style(t.text_hwnd, config.interaction.click_through);
            self.resize_thumbnail_if_needed(i, region_fit_grid);
            self.render_thumbnail_logged(i, "visuals refresh");
        }
    }

    /// Re-applies every thumbnail's on-screen position from the current display config, for config-dialog live preview and layout reflows; never touches startX/startY since those can be live-dragged in the running app.
    pub fn reposition_all_thumbnails(&mut self) {
        let window_count = self.thumbnails.iter().filter(|t| t.win32_enabled).count() as i32 * 2;
        if window_count == 0 {
            return;
        }
        let mut hdwp = unsafe { BeginDeferWindowPos(window_count) };
        if hdwp.is_null() {
            return;
        }

        let config = globals::config();
        let cfg = &config.display;
        let placement = resolve_monitor_placement(cfg);
        let monitor_bounds = placement.map(|p| p.bounds);
        let scale = dpi_to_scale(placement.map_or_else(default_dpi, |p| eve_maj_win::geometry::monitor_dpi(p.monitor)));
        let region_fit_active = is_region_fit_active(cfg);
        let grid_active = is_display_regions_active(cfg);
        let layout_managed = region_fit_active || grid_active;
        let not_logged_in_space = not_logged_in_space_rect_from_config(cfg);
        let total_count = self.thumbnails.len();

        // RegionFit fills in configured-order rank, not raw array position; the not-logged-in space carves its placeholders out of that rank and count entirely.
        let display_order = layout_managed.then(|| self.compute_region_fit_display_order(not_logged_in_space.is_some()));

        // Display Regions replace the single whole-region grid with one grid per cell.
        if grid_active {
            self.reload_account_membership_if_needed();
        }
        let grid_layout = grid_active.then(|| self.compute_grid_layout(display_order.as_ref()));

        // region/grid are invariant across every thumbnail this pass, so compute them once.
        let region_fit = (region_fit_active && !grid_active).then(|| {
            let region = region_rect_from_config(cfg).expect("checked by is_region_fit_active");
            let count = display_order.as_ref().map_or(total_count, |o| o.count);
            (region, calculate_region_fit_grid(region, count, cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region)))
        });
        let not_logged_in = not_logged_in_space.map(|space| (space, self.not_logged_in_space_grid(space, self.not_logged_in_space_count())));

        let defer = |hdwp: &mut _, t: &ThumbnailWindow, pos: Position, size: Option<(i32, i32)>| -> bool {
            let (w, h, flags) = match size {
                Some((w, h)) => (w, h, 0),
                None => (0, 0, SWP_NOSIZE),
            };
            unsafe {
                *hdwp = DeferWindowPos(*hdwp, t.hwnd, HWND_NOTOPMOST, pos.x, pos.y, w, h, flags | SWP_NOZORDER | SWP_NOACTIVATE);
                if hdwp.is_null() {
                    return false;
                }
                *hdwp = DeferWindowPos(*hdwp, t.text_hwnd, HWND_TOPMOST, pos.x, pos.y, w, h, flags | SWP_NOACTIVATE);
                if hdwp.is_null() {
                    return false;
                }
                if let Some((w, h)) = size {
                    // DeferWindowPos alone won't update the DWM thumbnail's own destination rect.
                    let props = make_thumbnail_props(w, h, DWM_TNP_RECTDESTINATION);
                    DwmUpdateThumbnailProperties(t.thumbnail_id, &props);
                }
            }
            true
        };

        for (index, t) in self.thumbnails.iter().enumerate() {
            if !t.win32_enabled {
                continue;
            }
            let carved_out = not_logged_in_space.is_some() && is_generic_character_name(&t.character_name);

            if let Some(gl) = &grid_layout {
                if let (false, Some(slot)) = (carved_out, gl.slot_of[index]) {
                    let grid = gl.grids[slot];
                    let pos = region_fit_position_for_grid(gl.cells[slot], grid, gl.rank_in_slot[index], self.grid_slot_direction(slot), cfg.spacing);
                    if !defer(&mut hdwp, t, pos, Some((grid.cell_width, grid.cell_height))) {
                        return;
                    }
                    continue;
                }
            }

            if let Some((region, grid)) = region_fit {
                if !carved_out {
                    let position_index = display_order.as_ref().map_or(index, |o| o.ranks[index]);
                    let pos = region_fit_position_for_grid(region, grid, position_index, cfg.region_fit_direction, cfg.spacing);
                    if !defer(&mut hdwp, t, pos, Some((grid.cell_width, grid.cell_height))) {
                        return;
                    }
                    continue;
                }
            }

            if carved_out {
                let (space, grid) = not_logged_in.expect("carved out implies a space");
                let pos = region_fit_position_for_grid(space, grid, self.not_logged_in_index(index), cfg.region_fit_direction, cfg.not_logged_in_space_spacing);
                // May still be sized from a previous RegionFit grid cell, so resize explicitly.
                if !defer(&mut hdwp, t, pos, Some((grid.cell_width, grid.cell_height))) {
                    return;
                }
                continue;
            }

            // Plain Custom mode: RegionFit is off and this thumbnail isn't a carved-out placeholder either.
            let (w, h) = self.thumbnail_size(&t.character_name, total_count, None);
            let (w, h) = (scale_pixels(w, scale), scale_pixels(h, scale));
            let pos = self.calculate_thumbnail_position(&t.character_name, w, h, index, total_count, monitor_bounds, scale);
            if !defer(&mut hdwp, t, pos, None) {
                return;
            }
        }
        unsafe { EndDeferWindowPos(hdwp) };

        // Unclaimed thumbnails were only moved above (SWP_NOSIZE); they may still carry a cell size from before the grid changed.
        if let Some(gl) = &grid_layout {
            for i in 0..self.thumbnails.len() {
                let t = &self.thumbnails[i];
                if !t.win32_enabled || gl.slot_of[i].is_some() || (not_logged_in_space.is_some() && is_generic_character_name(&t.character_name)) {
                    continue;
                }
                self.resize_thumbnail_if_needed(i, None);
            }
        }

        if layout_managed || not_logged_in.is_some() {
            // Avoids a one-tick delay before the border catches up to the new cell size.
            for i in 0..self.thumbnails.len() {
                if !self.thumbnails[i].win32_enabled {
                    continue;
                }
                self.thumbnails[i].cached_render_settings = None;
                self.render_thumbnail_logged(i, "region fit resize");
            }
        }
    }

    /// Resizes a thumbnail's window and DWM rect to the DPI-scaled configured size if changed; text_hwnd resizes separately via UpdateLayeredWindow.
    /// Pass precomputed_grid when called for every thumbnail in a batch (see thumbnail_size).
    pub fn resize_thumbnail_if_needed(&mut self, index: usize, precomputed_grid: Option<RegionFitGrid>) {
        let t = &self.thumbnails[index];
        let (w, h) = self.thumbnail_size(&t.character_name, self.thumbnails.len(), precomputed_grid);
        // Grid-fit sizes are already absolute physical pixels; only the plain default/per-character size needs DPI scaling.
        let (target_w, target_h) = if self.uses_grid_size(&t.character_name) {
            (w, h)
        } else {
            let scale = dpi_to_scale(window_dpi(t.hwnd));
            (scale_pixels(w, scale), scale_pixels(h, scale))
        };

        let mut current = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { GetClientRect(t.hwnd, &mut current) } == 0 {
            return;
        }
        if current.right == target_w && current.bottom == target_h {
            return;
        }
        unsafe {
            SetWindowPos(t.hwnd, HWND_NOTOPMOST, 0, 0, target_w, target_h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            let props = make_thumbnail_props(target_w, target_h, DWM_TNP_RECTDESTINATION);
            DwmUpdateThumbnailProperties(t.thumbnail_id, &props);
        }
    }

    fn process_dirty_thumbnails(&mut self) {
        self.render_dirty_thumbnails(None);
    }

    /// Renders every thumbnail with needs_render set. If max_immediate is given and more thumbnails than that are dirty, renders only up to the cap now and leaves the rest dirty for the timer.
    pub fn render_dirty_thumbnails(&mut self, max_immediate: Option<usize>) {
        let mut rendered = 0;
        for i in 0..self.thumbnails.len() {
            if !self.thumbnails[i].needs_render || max_immediate.is_some_and(|cap| rendered >= cap) {
                continue;
            }
            self.render_thumbnail_logged(i, "dirty thumbnail");
            self.thumbnails[i].needs_render = false;
            rendered += 1;
        }
    }

    /// Re-asserts HWND_TOPMOST z-order for all thumbnail/text windows when another app's topmost window steals it from us.
    fn reassert_topmost(&self) {
        // Batched so DWM applies the whole z-order change atomically, instead of compositing each intermediate SetWindowPos and flashing thumbnails.
        let window_count = self.thumbnails.iter().filter(|t| t.win32_enabled).count() as i32 * 2;
        if window_count == 0 {
            return;
        }
        unsafe {
            let mut hdwp = BeginDeferWindowPos(window_count);
            for t in self.thumbnails.iter().filter(|t| t.win32_enabled) {
                for h in [t.hwnd, t.text_hwnd] {
                    if hdwp.is_null() {
                        return;
                    }
                    hdwp = DeferWindowPos(hdwp, h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
            if !hdwp.is_null() {
                EndDeferWindowPos(hdwp);
            }
        }
    }

    /// Updates DPS values for a character's overlay.
    pub fn update_dps_for_character(&mut self, source_hwnd: HWND, incoming: Option<f32>, outgoing: Option<f32>) {
        let Some(i) = self.index_by_source(source_hwnd) else { return };
        let t = &mut self.thumbnails[i];
        let first_update = !t.has_dps_data;
        t.has_dps_data = true;
        if first_update || t.last_incoming_dps != incoming || t.last_outgoing_dps != outgoing {
            t.last_incoming_dps = incoming;
            t.last_outgoing_dps = outgoing;
            t.needs_render = true;
        }
    }

    /// Re-render thumbnails with updated tracker values (called from the main timer loop)
    pub fn process_dirty_dps_overlays(&mut self) {
        self.process_dirty_thumbnails();
    }

    /// Updates mining rate (and its ISK/sec twin) for a character's overlay.
    pub fn update_mining_for_character(&mut self, source_hwnd: HWND, rate: Option<f32>, isk_rate: Option<f32>) {
        let Some(i) = self.index_by_source(source_hwnd) else { return };
        let t = &mut self.thumbnails[i];
        let first_update = !t.has_mining_data;
        t.has_mining_data = true;
        if first_update || t.last_mining_rate != rate || t.last_mining_isk_rate != isk_rate {
            t.last_mining_rate = rate;
            t.last_mining_isk_rate = isk_rate;
            t.needs_render = true;
        }
    }

    /// Updates the bounty ISK/sec rate for a character's overlay.
    pub fn update_bounty_for_character(&mut self, source_hwnd: HWND, isk_rate: Option<f32>) {
        let Some(i) = self.index_by_source(source_hwnd) else { return };
        let t = &mut self.thumbnails[i];
        let first_update = !t.has_bounty_data;
        t.has_bounty_data = true;
        if first_update || t.last_bounty_isk_rate != isk_rate {
            t.last_bounty_isk_rate = isk_rate;
            t.needs_render = true;
        }
    }

    /// Updates per-process CPU%/RAM/VRAM for a character's overlay; `has_vram` is false when VRAM sampling isn't available.
    pub fn update_resource_stats_for_character(&mut self, source_hwnd: HWND, cpu_percent: f32, ram_mb: f32, vram_mb: f32, has_vram: bool) {
        let Some(i) = self.index_by_source(source_hwnd) else { return };
        let t = &mut self.thumbnails[i];
        let first_update = !t.has_resource_data;
        t.has_resource_data = true;
        if first_update || t.last_cpu_percent != cpu_percent || t.last_ram_mb != ram_mb || t.last_vram_mb != vram_mb || t.has_vram_data != has_vram {
            t.last_cpu_percent = cpu_percent;
            t.last_ram_mb = ram_mb;
            t.last_vram_mb = vram_mb;
            t.has_vram_data = has_vram;
            t.needs_render = true;
        }
    }

    /// Clear expired notifications (call from update loop)
    pub fn update_notifications(&mut self) {
        let now = Ticks::now();
        for i in 0..self.thumbnails.len() {
            let t = &mut self.thumbnails[i];
            let before = t.active_notifications.len();
            // duration_ms == 0 means the notification is permanent.
            t.active_notifications.retain(|n| n.duration_ms == 0 || now.elapsed_since(n.start_time) < n.duration_ms as u64);
            if t.active_notifications.len() != before {
                t.needs_render = true;
            }
            if t.test_restore_visibility.is_some() && t.active_notifications.is_empty() {
                self.restore_visibility_after_test(i);
            }
            // Force a render each tick so the newest entry's alternating on/off flash phases actually paint.
            let t = &mut self.thumbnails[i];
            if t.active_notifications.first().is_some_and(|n| n.flash_border && now.elapsed_since(n.start_time) < NOTIFICATION_FLASH_TOTAL_MS) {
                t.needs_render = true;
            }
        }
    }

    /// Reacts to Scout's name-change events: syncs the affected thumbnail's name/title and runs the associated side effects (position restore, system-name clear, exclusion restore). Returns whether a layout reflow is needed.
    pub fn apply_name_changes(&mut self, name_changes: &[NameChange], eve_windows: &[EveWindow]) -> bool {
        let config = globals::config();
        let mut any_login_rank_change = false;
        let mut any_logout_rank_change = false;
        for change in name_changes {
            let Some(i) = self.index_by_source(change.hwnd) else { continue };

            let was_generic = is_generic_character_name(&change.old_name);
            let now_generic = is_generic_character_name(&change.new_name);
            // An unconfigured "EVE" placeholder always sorts last, so either direction can change a thumbnail's RegionFit rank.
            if was_generic && !now_generic {
                any_login_rank_change = true;
            }
            if now_generic && !was_generic {
                any_logout_rank_change = true;
            }

            let new_title = eve_windows.iter().find(|w| w.hwnd == change.hwnd).map_or(change.new_name.as_str(), |w| w.title.as_str());
            self.thumbnails[i].title = new_title.to_owned();
            self.thumbnails[i].character_name = change.new_name.clone();
            self.thumbnails[i].cached_char_dims = None;
            self.refresh_character_caches(i);

            // If character logged in (changed from "EVE" to actual name), move the thumbnail box to its remembered spot
            if was_generic && !now_generic {
                // RegionFit ignores the saved spot; the reflow below places it correctly instead.
                if self.thumbnails[i].win32_enabled && !is_layout_managed(&config.display) {
                    if let Some(saved) = config.character_position(&change.new_name) {
                        let t = &self.thumbnails[i];
                        unsafe {
                            SetWindowPos(t.hwnd, HWND_NOTOPMOST, saved.x, saved.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                            SetWindowPos(t.text_hwnd, HWND_TOPMOST, saved.x, saved.y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
                        }
                        self.resize_thumbnail_if_needed(i, None);
                        SLOG.info(format_args!("Moved {} thumbnail to saved position: ({}, {})", change.new_name, saved.x, saved.y));
                    } else {
                        SLOG.debug(format_args!("No saved thumbnail position for {}, keeping current location", change.new_name));
                    }
                }

                // Auto-move-on-login setting: same action as the hotkey, but for the real EVE client window
                if config.auto_move_position.enabled && !config.is_excluded_from_auto_move(&change.new_name) {
                    if let Some(window_pos) = config.character_window_position(&change.new_name) {
                        let source = self.thumbnails[i].source_hwnd;
                        manager::move_client_to_position(source, window_pos);
                        self.queue_auto_move_verification(source, window_pos);
                        SLOG.info(format_args!("Auto-moved {} client window to saved position: ({}, {})", change.new_name, window_pos.x, window_pos.y));
                    } else {
                        SLOG.debug(format_args!("No saved window position for {}, auto-move-on-login skipped", change.new_name));
                    }
                }
            }

            // If character logged out (title is just "EVE"), clear system name
            if now_generic {
                let t = &mut self.thumbnails[i];
                t.system_name.clear();
                t.cached_system_color = config.thumbnail.system_name_color;
                t.cached_sys_dims = None;
                SLOG.debug(format_args!("Cleared system name for logged out client"));
                // Unconditionally clears the entire notification stack, same as a full natural expiry.
                if !t.active_notifications.is_empty() {
                    t.active_notifications.clear();
                    t.needs_render = true;
                }
                // Moving this placeholder into the not-logged-in space happens via the forced reflow below, which also resizes everyone else already there.
            }

            // Update exclusion state when character name becomes known (e.g., "EVE" -> "Probe Enthusiast")
            if was_generic && !now_generic {
                if let Some(manager) = globals::hotkey_manager() {
                    let is_excluded = manager.is_character_excluded(&change.new_name);
                    if is_excluded != self.thumbnails[i].is_excluded_from_cycle {
                        self.thumbnails[i].is_excluded_from_cycle = is_excluded;
                        if is_excluded {
                            SLOG.info(format_args!("Restored exclusion state for {}", change.new_name));
                        }
                    }
                }
            }

            self.render_thumbnail_logged(i, "name change");
            SLOG.info(format_args!("Updated thumbnail for {}", self.thumbnails[i].character_name));
        }

        let region_fit_active = is_layout_managed(&config.display);
        let not_logged_in_space_active = not_logged_in_space_rect_from_config(&config.display).is_some();
        // The not-logged-in space is count-dependent, so crossing its boundary must reflow it regardless of regionFitReorderLoggedOut.
        if not_logged_in_space_active && (any_login_rank_change || any_logout_rank_change) {
            return true;
        }
        region_fit_active && (any_login_rank_change || (any_logout_rank_change && config.display.region_fit_reorder_logged_out))
    }

    /// Callers gate on their own setting; only the per-character exclusion is checked here.
    pub fn move_client_to_saved_position(&mut self, hwnd: HWND, character_name: &str) {
        let config = globals::config();
        if config.is_excluded_from_auto_move(character_name) {
            return;
        }
        let Some(pos) = config.character_window_position(character_name) else { return };
        manager::move_client_to_position(hwnd, pos);
        self.queue_auto_move_verification(hwnd, pos);
        SLOG.info(format_args!("Moved {character_name} client window to saved position: ({}, {})", pos.x, pos.y));
    }

    fn queue_auto_move_verification(&mut self, hwnd: HWND, pos: Position) {
        let auto_move = &globals::config().auto_move_position;
        if auto_move.verify_count == 0 {
            return;
        }
        let entry = PendingAutoMove { hwnd, target: manager::clamp_to_virtual_screen(pos), last_move: Ticks::now(), checks_left: auto_move.verify_count, last_seen: None };
        match self.pending_auto_moves.iter_mut().find(|e| e.hwnd == hwnd) {
            Some(existing) => *existing = entry,
            None => self.pending_auto_moves.push(entry),
        }
    }

    /// Polls without touching the window until its position stops changing between two consecutive polls (i.e. EVE is done repositioning it), then corrects it exactly once. Re-applying on every poll while EVE is still mid-move would re-grab focus each time, making clients visibly jump.
    fn verify_pending_auto_moves(&mut self) {
        let now = Ticks::now();
        let interval = globals::config().auto_move_position.verify_interval_ms as u64;
        let mut i = 0;
        while i < self.pending_auto_moves.len() {
            let entry = &mut self.pending_auto_moves[i];
            if !is_window(entry.hwnd) {
                self.pending_auto_moves.swap_remove(i);
                continue;
            }
            if now.elapsed_since(entry.last_move) < interval {
                i += 1;
                continue;
            }
            entry.last_move = now;

            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            if unsafe { GetWindowRect(entry.hwnd, &mut rect) } == 0 {
                SLOG.warn(format_args!("Auto-move verification: GetWindowRect failed"));
                self.pending_auto_moves.swap_remove(i);
                continue;
            }
            if rect.left == entry.target.x && rect.top == entry.target.y {
                self.pending_auto_moves.swap_remove(i);
                continue;
            }

            entry.checks_left = entry.checks_left.saturating_sub(1);
            let settled = entry.last_seen.is_some_and(|p| p.x == rect.left && p.y == rect.top);
            if settled || entry.checks_left == 0 {
                SLOG.info(format_args!("Client drifted to ({}, {}) after auto-move, re-applying ({}, {})", rect.left, rect.top, entry.target.x, entry.target.y));
                manager::move_client_to_position(entry.hwnd, entry.target);
                self.pending_auto_moves.swap_remove(i);
                continue;
            }
            entry.last_seen = Some(POINT { x: rect.left, y: rect.top });
            i += 1;
        }
    }

    /// Syncs thumbnail title text against Scout's latest scan, independent of character-name changes.
    fn sync_thumbnail_titles(&mut self, eve_windows: &[EveWindow]) {
        for w in eve_windows {
            if let Some(i) = self.index_by_source(w.hwnd) {
                if self.thumbnails[i].title != w.title {
                    self.thumbnails[i].title = w.title.clone();
                }
            }
        }
    }

    /// Synchronizes thumbnails with Scout's window list, creating thumbnails for new windows; returns true if any were created.
    fn sync_thumbnails_with_windows(&mut self, eve_windows: &[EveWindow]) -> bool {
        let mut created_new = false;
        for w in eve_windows {
            if self.has_thumbnail(w.hwnd) {
                continue;
            }
            if let Err(err) = self.create_thumbnail(w, "") {
                SLOG.err(format_args!("Failed to create thumbnail for {}: {err}", w.character_name));
                continue;
            }
            created_new = true;
            if globals::config().auto_move_position.enabled {
                self.move_client_to_saved_position(w.hwnd, &w.character_name);
            }
        }
        created_new
    }

    /// Main update cycle - performs all Painter operations for a single tick
    pub fn update(&mut self, eve_windows: &[EveWindow], closed_windows: &[ClosedWindow], name_changes: &[NameChange]) {
        let mut needs_region_reflow = self.cleanup_closed_thumbnails(closed_windows);
        self.update_thumbnail_states();
        self.check_auto_minimize();
        needs_region_reflow = self.apply_name_changes(name_changes, eve_windows) || needs_region_reflow;
        self.verify_pending_auto_moves();
        self.sync_thumbnail_titles(eve_windows);

        // create_thumbnail seeds title/character_name from the window, so new thumbnails need no re-sync.
        let created_new = self.sync_thumbnails_with_windows(eve_windows);
        let display = &globals::config().display;
        // A new arrival changes RegionFit's and/or the not-logged-in space's grid count, so every member must reflow to the recomputed cell size.
        needs_region_reflow = (created_new && (is_layout_managed(display) || not_logged_in_space_rect_from_config(display).is_some())) || needs_region_reflow;

        // Coalesced into one reflow, since any combination of the three triggers above can fire in the same tick.
        if needs_region_reflow {
            self.reposition_all_thumbnails();
        }
        self.process_dirty_thumbnails();
    }

    /// True when at least one tracked EVE client currently has a real (non-generic) character name, i.e. is logged in.
    pub fn any_character_logged_in(&self) -> bool {
        self.thumbnails.iter().any(|t| !is_generic_character_name(&t.character_name))
    }

    /// Whether the History Panel is on-screen; it isn't ported yet, so never.
    pub fn is_notif_info_panel_visible(&self) -> bool {
        false
    }

    /// Resets the notification-history ring buffer; used by the tray menu's "Clear Notification History" action.
    pub fn clear_notification_history(&mut self) {
        self.notification_history.clear();
    }

    /// The notification history, newest first.
    pub fn notification_history(&self) -> impl Iterator<Item = &NotificationHistoryEntry> {
        self.notification_history.iter().rev()
    }

    /// Appends a notification to the history ring buffer (overwrites the oldest entry once full); called from show_notification for every notification actually shown.
    fn push_notification_history(&mut self, source_hwnd: HWND, character_name: &str, text: &str, notification_type: NotificationType) {
        // Same caps as the Zig build's fixed 64/96-byte entry buffers.
        let clip = |s: &str, max: usize| {
            let mut end = s.len().min(max);
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s[..end].to_owned()
        };
        let character_name = clip(character_name, 64);
        let character_color = globals::config().character_name_color(&character_name);
        if self.notification_history.len() == NOTIF_HISTORY_CAPACITY {
            self.notification_history.pop_front();
        }
        self.notification_history.push_back(NotificationHistoryEntry {
            source_hwnd,
            notification_type,
            character_name,
            text: clip(text, 96),
            timestamp: Ticks::now(),
            character_color,
        });
    }

    /// Reflows every thumbnail's RegionFit grid slot after a rank/count change (bulk create loops, group membership); no-op outside RegionFit.
    pub fn reflow_if_region_fit_active(&mut self) {
        if is_layout_managed(&globals::config().display) {
            self.reposition_all_thumbnails();
        }
    }

    fn determine_initial_visibility(&self, source_hwnd: HWND) -> VisibilityState {
        let foreground = unsafe { GetForegroundWindow() };
        let any_eve_has_focus = source_hwnd == foreground || self.thumbnails.iter().any(|t| t.source_hwnd == foreground);
        if globals::config().thumbnail.hide_when_no_eve_focus && !any_eve_has_focus {
            VisibilityState::HiddenAutomatic
        } else {
            VisibilityState::Visible
        }
    }

    /// A fresh ThumbnailWindow record for `eve_window` with every config-derived cache field resolved.
    fn new_thumbnail_record(&self, eve_window: &EveWindow, system_name: &str, hwnd: HWND, text_hwnd: HWND, thumbnail_id: isize, win32_enabled: bool) -> ThumbnailWindow {
        let config = globals::config();
        let name = &eve_window.character_name;
        let is_excluded = globals::hotkey_manager().is_some_and(|m| m.is_character_excluded(name));
        if win32_enabled {
            if globals::hotkey_manager().is_none() {
                SLOG.debug(format_args!("Hotkey manager not available during thumbnail creation for {name}"));
            } else if is_excluded {
                SLOG.info(format_args!("Character {name} is excluded from cycling, setting visual indicator"));
            }
        }
        ThumbnailWindow {
            hwnd,
            text_hwnd,
            thumbnail_id,
            source_hwnd: eve_window.hwnd,
            title: eve_window.title.clone(),
            character_name: name.clone(),
            system_name: system_name.to_owned(),
            system_name_event_ts: 0,
            last_jump: Ticks::default(),
            travel_alert_fired: false,
            active_notifications: Vec::new(),
            last_click_time: Ticks::default(),
            last_notification_time_by_type: vec![Ticks::default(); NotificationType::COUNT],
            is_excluded_from_cycle: is_excluded,
            needs_render: false,
            win32_enabled,
            last_incoming_dps: None,
            last_outgoing_dps: None,
            last_mining_rate: None,
            last_mining_isk_rate: None,
            last_bounty_isk_rate: None,
            last_cpu_percent: 0.0,
            last_ram_mb: 0.0,
            last_vram_mb: 0.0,
            has_dps_data: false,
            has_mining_data: false,
            has_bounty_data: false,
            has_resource_data: false,
            has_vram_data: false,
            cached_overlay: None,
            visibility_state: self.determine_initial_visibility(eve_window.hwnd),
            test_restore_visibility: None,
            inactive_since: Ticks::now(),
            was_minimized: false,
            cached_render_settings: None,
            cached_char_dims: None,
            cached_sys_dims: None,
            cached_badge_dims: None,
            cached_font: no_font(),
            cached_sys_font: no_font(),
            cached_badge_font: no_font(),
            cached_system_color: if system_name.is_empty() { config.thumbnail.system_name_color } else { config.system_name_color(system_name) },
            cached_character_color: config.character_name_color(name),
            cached_display_name: config.display_name(name).to_owned(),
            cached_border_colors: config.character_border_colors(name),
            cached_excluded_from_minimize: config.is_excluded_from_minimize(name),
            cached_hide_thumbnail: config.is_thumbnail_hidden(name),
            cached_thumbnail_size: config.character_size(name),
            cached_opacity: config.character_opacity(name),
            cached_group_badge_label: Self::build_group_badge_label(name),
        }
    }

    /// After a new thumbnail is indexed: this window may already be the real foreground window, so reconcile now instead of waiting for the next tick.
    fn reconcile_new_thumbnail(&mut self, eve_window: &EveWindow) {
        let foreground = unsafe { GetForegroundWindow() };
        self.reconcile_thumbnail_states((!foreground.is_null()).then_some(foreground));
        if foreground == eve_window.hwnd {
            if let Some(manager) = globals::hotkey_manager() {
                manager.update_focused_character(&eve_window.character_name, eve_window.hwnd);
            }
        }
    }

    /// ClientList and Nothing modes only need a data record, not real Win32 windows.
    fn create_tracking_only_entry(&mut self, eve_window: &EveWindow, initial_system_name: &str) {
        // Sentinel HWNDs, never passed to Win32 APIs since win32_enabled is false.
        let sentinel = 1usize as HWND;
        let thumbnail = self.new_thumbnail_record(eve_window, initial_system_name, sentinel, sentinel, 1, false);
        self.thumbnails.push(thumbnail);
        // Only register the source HWND, since thumbnail/text HWNDs are sentinels.
        self.hwnd_to_thumbnail_index.insert(eve_window.hwnd, self.thumbnails.len() - 1);
        self.reconcile_new_thumbnail(eve_window);
        SLOG.info(format_args!("Created tracking entry for {} ({:?} mode)", eve_window.character_name, globals::config().display.view_mode));
    }

    pub fn create_thumbnail(&mut self, eve_window: &EveWindow, initial_system_name: &str) -> Result<(), CreateError> {
        let config = globals::config();
        if config.display.view_mode != ViewMode::Thumbnails {
            self.create_tracking_only_entry(eve_window, initial_system_name);
            return Ok(());
        }

        // Thumbnail mode: full Win32/DWM path.
        let name = &eve_window.character_name;
        let total_count = self.thumbnails.len() + 1;
        let (w, h) = self.thumbnail_size(name, total_count, None);
        let placement = resolve_monitor_placement(&config.display);
        let monitor_bounds = placement.map(|p| p.bounds);
        let scale = dpi_to_scale(placement.map_or_else(default_dpi, |p| eve_maj_win::geometry::monitor_dpi(p.monitor)));
        // Grid-fit sizes are already absolute physical pixels; only the plain default/per-character size needs DPI scaling.
        let (thumb_w, thumb_h) = if self.uses_grid_size(name) { (w, h) } else { (scale_pixels(w, scale), scale_pixels(h, scale)) };
        let pos = self.calculate_thumbnail_position(name, thumb_w, thumb_h, self.thumbnails.len(), total_count, monitor_bounds, scale);

        // Needed on both windows since text_hwnd, being topmost, is the one that actually receives mouse messages.
        let click_through_ex = if config.interaction.click_through { WS_EX_TRANSPARENT } else { 0 };
        let name_w = wide(name);
        let class_w = wide(WINDOW_CLASS_NAME);
        let text_class_w = wide(TEXT_WINDOW_CLASS_NAME);

        // Create thumbnail window (borderless, layered for transparency)
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | click_through_ex,
                class_w.as_ptr(), name_w.as_ptr(), WS_POPUP | WS_VISIBLE, pos.x, pos.y, thumb_w, thumb_h,
                std::ptr::null_mut(), std::ptr::null_mut(), self.instance, std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(CreateError::CreateWindowFailed);
        }
        let destroy = |h: HWND| unsafe {
            DestroyWindow(h);
        };

        unsafe { SetLayeredWindowAttributes(hwnd, 0, config.character_opacity(name), LWA_ALPHA) };

        let mut thumbnail_id: isize = 0;
        if unsafe { DwmRegisterThumbnail(hwnd, eve_window.hwnd, &mut thumbnail_id) } != 0 {
            destroy(hwnd);
            return Err(CreateError::DwmRegisterThumbnailFailed);
        }

        // Fill the entire window
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetClientRect(hwnd, &mut client) };
        let props = make_thumbnail_props(client.right, client.bottom, DWM_TNP_VISIBLE | DWM_TNP_RECTDESTINATION | DWM_TNP_SOURCECLIENTAREAONLY);
        if unsafe { DwmUpdateThumbnailProperties(thumbnail_id, &props) } != 0 {
            unsafe { DwmUnregisterThumbnail(thumbnail_id) };
            destroy(hwnd);
            return Err(CreateError::DwmUpdateThumbnailPropertiesFailed);
        }
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
        }

        // Covers the full thumbnail, not just a top bar.
        let text_hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | click_through_ex,
                text_class_w.as_ptr(), name_w.as_ptr(), WS_POPUP, pos.x, pos.y, thumb_w, thumb_h,
                std::ptr::null_mut(), std::ptr::null_mut(), self.instance, std::ptr::null(),
            )
        };
        if text_hwnd.is_null() {
            unsafe { DwmUnregisterThumbnail(thumbnail_id) };
            destroy(hwnd);
            return Err(CreateError::CreateTextWindowFailed);
        }

        let thumbnail = self.new_thumbnail_record(eve_window, initial_system_name, hwnd, text_hwnd, thumbnail_id, true);
        self.thumbnails.push(thumbnail);
        let new_index = self.thumbnails.len() - 1;
        if let Err(err) = self.render_thumbnail(new_index) {
            let mut t = self.thumbnails.pop().expect("just pushed");
            Self::destroy_thumbnail_resources(&mut t);
            return Err(CreateError::Render(err));
        }

        unsafe {
            // Store source window handle for click-to-focus
            SetPropW(hwnd, SOURCE_HWND_PROP.as_ptr(), eve_window.hwnd as _);
            SetPropW(text_hwnd, SOURCE_HWND_PROP.as_ptr(), eve_window.hwnd as _);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, text_hwnd as isize);
            // For reverse lookup during drag.
            SetWindowLongPtrW(text_hwnd, GWLP_USERDATA, hwnd as isize);
            SetWindowPos(text_hwnd, HWND_TOPMOST, pos.x, pos.y, thumb_w, thumb_h, SWP_NOACTIVATE);
            ShowWindow(text_hwnd, SW_SHOW);
            UpdateWindow(text_hwnd);
        }

        // Add to HWND indices for O(1) lookups by all window handles
        self.hwnd_to_thumbnail_index.insert(eve_window.hwnd, new_index);
        self.thumbnail_hwnd_to_index.insert(hwnd, new_index);
        self.text_hwnd_to_index.insert(text_hwnd, new_index);
        self.reconcile_new_thumbnail(eve_window);
        SLOG.info(format_args!("Created thumbnail for {name}"));
        Ok(())
    }

    pub fn save_thumbnail_position(&mut self, hwnd: HWND) {
        if !is_window(hwnd) {
            return;
        }
        let Some(i) = self.index_by_overlay(hwnd) else { return };
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetWindowRect(hwnd, &mut rect) };
        let name = self.thumbnails[i].character_name.clone();
        if let Err(err) = globals::config().save_character_position(&name, Position { x: rect.left, y: rect.top }) {
            SLOG.err(format_args!("Failed to save position for {name}: {err}"));
        }
    }

    /// Region selection (the config dialog's "Start Region Selection") isn't ported yet; logs and ignores the request.
    pub fn start_region_select(&mut self, _request: &eve_maj_core::protocol::RegionSelectRequest) {
        SLOG.warn(format_args!("Region selection is not available in this build yet"));
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        for entry in self.cached_fonts.values() {
            unsafe { DeleteObject(entry.font) };
        }
        unsafe {
            if !self.focus_event_hook.is_null() {
                UnhookWinEvent(self.focus_event_hook);
            }
            if !self.destroy_event_hook.is_null() {
                UnhookWinEvent(self.destroy_event_hook);
            }
        }
        self.current_drag_ghost_groups = None;
        self.ghost_overlay_bitmap = None;
        if let Some(hwnd) = self.ghost_overlay_hwnd {
            unsafe { DestroyWindow(hwnd) };
        }
        self.drag_hint_bitmap = None;
        if let Some(hwnd) = self.drag_hint_hwnd {
            unsafe { DestroyWindow(hwnd) };
        }
        for t in &mut self.thumbnails {
            Self::destroy_thumbnail_resources(t);
        }
    }
}

/// OBJID_WINDOW; anything else is a child object of the window.
const OBJID_WINDOW: i32 = 0;

unsafe extern "system" fn window_destroy_proc(_: HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    let Some(painter) = globals::painter() else { return };
    let Some(index) = painter.resolve_thumbnail_index_for_destroy(hwnd) else { return };
    SLOG.info(format_args!("Window closed (event), removing thumbnail for {}", painter.thumbnails[index].character_name));
    painter.remove_thumbnail_at(index);
    painter.rebuild_hwnd_index(false);
    if painter.thumbnails.is_empty() {
        globals::config().flush_auto_colors();
    }
}

/// True if hwnd belongs to this process, so its own dialogs/panels never get recorded as a "last non-EVE app".
fn is_own_process_window(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// True if hwnd is the desktop shell (Progman or a WorkerW), which isn't a real "app" to return focus to.
fn is_desktop_shell_window(hwnd: HWND) -> bool {
    matches!(class_name(hwnd).as_deref(), Some("Progman" | "WorkerW"))
}

/// True if hwnd belongs to explorer.exe (taskbar, tray, Start menu, etc.), which should always be able to sit above our thumbnails.
fn is_explorer_owned(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return false;
    }
    const SUFFIX: &str = "\\explorer.exe";
    process_exe_path(pid).is_some_and(|p| p.len() >= SUFFIX.len() && p.as_bytes()[p.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX.as_bytes()))
}

unsafe extern "system" fn win_event_proc(_: HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
    let Some(painter) = globals::painter() else { return };
    let config = globals::config();

    // O(1) lookup: Check if it's one of our thumbnail windows (early exit - most common case)
    if painter.thumbnail_hwnd_to_index.contains_key(&hwnd) || painter.text_hwnd_to_index.contains_key(&hwnd) {
        SLOG.debug(format_args!("Thumbnail window got focus (ignoring): {hwnd:?}"));
        return;
    }

    // A newly-foregrounded topmost window gets inserted above ours in the z-order band; push back unless it's shell UI allowed to stay on top.
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if ex_style & WS_EX_TOPMOST as isize != 0 && !is_explorer_owned(hwnd) {
        painter.reassert_topmost();
    }

    if !painter.hwnd_to_thumbnail_index.contains_key(&hwnd) {
        if !is_own_process_window(hwnd) && !is_desktop_shell_window(hwnd) {
            painter.last_non_eve_foreground = Some(hwnd);
        }
        if config.thumbnail.hide_when_no_eve_focus {
            SLOG.debug(format_args!("Untracked window focused (hwnd={hwnd:?}), starting {}ms debounce timer (hideWhenNoEveFocus=true)", config.thumbnail.hide_debounce_ms));
            // Only use thumbnail HWNDs in thumbnail mode (list mode has no valid thumbnail HWNDs)
            if let Some(first) = painter.thumbnails.first().filter(|t| t.win32_enabled) {
                let timer_hwnd = first.hwnd;
                if SetTimer(timer_hwnd, HIDE_DEBOUNCE_TIMER_ID, config.thumbnail.hide_debounce_ms, None) != 0 {
                    painter.hide_debounce_timer_hwnd = Some(timer_hwnd);
                } else {
                    SLOG.err(format_args!("Failed to start hide debounce timer"));
                }
            }
        } else {
            SLOG.debug(format_args!("Untracked window focused (hwnd={hwnd:?}), ignoring (hideWhenNoEveFocus=false)"));
        }
        return;
    }

    // Cancel any pending hide timer since an EVE window now has focus
    if let Some(timer_hwnd) = painter.hide_debounce_timer_hwnd.take() {
        KillTimer(timer_hwnd, HIDE_DEBOUNCE_TIMER_ID);
        SLOG.debug(format_args!("Cancelled hide debounce timer (tracked window focused)"));
    }

    // WINEVENT_OUTOFCONTEXT delivery can lag well behind the actual focus change; during rapid cycling a stale event can
    // arrive after focus has already moved on again, so drop it rather than reconciling the active border back to a target that's no longer current.
    let current_foreground = GetForegroundWindow();
    if current_foreground != hwnd {
        SLOG.debug(format_args!("Ignoring stale focus event (event hwnd={hwnd:?}, current foreground={current_foreground:?})"));
        return;
    }

    painter.reconcile_thumbnail_states(Some(hwnd));
    if let Some(i) = painter.index_by_source(hwnd) {
        let name = painter.thumbnails[i].character_name.clone();
        SLOG.debug(format_args!("Tracked window focused: {name}"));
        if let Some(manager) = globals::hotkey_manager() {
            manager.update_focused_character(&name, hwnd);
        }
    }
}

#[allow(dead_code)]
const _: i32 = OBJID_WINDOW;
