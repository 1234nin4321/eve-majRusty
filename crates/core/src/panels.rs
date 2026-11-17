//! What the Client List view and the notification History Panel show, as plain data a front end draws: row order,
//! texts and colours, the list's column maths, the history ring buffer with its category filter, and when the
//! History Panel is on screen. Mirrors the Zig build's list_view.zig and notif_info_view.zig (their GDI drawing aside).

use std::collections::VecDeque;

use crate::config::{build_character_order_map, order_map_less_than, Config, DisplayConfig};
use crate::render::{format_isk_abbrev, ActiveNotification, Metrics};
use crate::state::ThumbnailState;
use crate::config::IskRateUnit;
use crate::types::{ListViewOrder, NotificationCategory, NotificationType};

// ------------------------------------------------------------------------------------------------- client list --

pub const LIST_WIDTH: i32 = 230;
pub const LIST_HEADER_HEIGHT: i32 = 24;
pub const LIST_ROW_HEIGHT: i32 = 28;
pub const LIST_BADGE_LEFT: i32 = 8;
pub const LIST_BADGE_RADIUS: i32 = 5;
pub const LIST_TEXT_LEFT: i32 = LIST_BADGE_LEFT + LIST_BADGE_RADIUS * 2 + 7;
pub const LIST_TEXT_PAD_Y: i32 = 6;
pub const LIST_RIGHT_MARGIN: i32 = 8;
/// Width kept free for the right-hand text when a long name is cut short.
pub const LIST_NAME_RESERVE: i32 = 70;
/// The list never lays out more columns than this, whatever listViewColumns says.
pub const LIST_MAX_COLUMNS: u32 = 6;

// Colours, 0xRRGGBB unless noted.
pub const LIST_HEADER_BG: u32 = 0x1A1A1A;
pub const LIST_ROW_BG: u32 = 0x0F0F0F;
pub const LIST_ROW_ALERT_BG: u32 = 0x2A1A1A;
pub const PANEL_SEPARATOR: u32 = 0x888888;
pub const PANEL_FRAME: u32 = 0x888888;
pub const PANEL_HEADER_TEXT: u32 = 0xFFFFFF;
pub const LIST_NAME_TEXT: u32 = 0xFFFFFF;
pub const LIST_NOTIFICATION_TEXT: u32 = 0xFFAA00;
pub const LIST_SYSTEM_TEXT: u32 = 0xCCCCCC;
/// The default active border (ARGB); a list badge shows green instead of this grey.
const DEFAULT_ACTIVE_BORDER: u32 = 0xFF60_6060;
pub const BADGE_ACTIVE: u32 = 0x44CC44;
pub const BADGE_ALERT: u32 = 0xFF8833;
pub const BADGE_INACTIVE: u32 = 0x505050;
pub const BADGE_MINIMIZED: u32 = 0x303030;
pub const BADGE_DISABLED_BG: u32 = 0x3E3E3E;
pub const BADGE_DISABLED_X: u32 = 0xCC4444;

/// Columns used for `n` rows: listViewColumns clamped to 1..=6, never more than there are rows.
pub fn effective_columns(configured: u32, n: usize) -> i32 {
    let clamped = configured.clamp(1, LIST_MAX_COLUMNS) as i32;
    if n == 0 {
        clamped
    } else {
        clamped.min(n as i32)
    }
}

/// Rows in column `col` when `n` rows fill `columns` columns row by row; earlier columns take the remainder.
pub fn items_in_column(n: i32, columns: i32, col: i32) -> i32 {
    let base = n / columns;
    if col < n % columns {
        base + 1
    } else {
        base
    }
}

/// The list window's size for `n` rows.
pub fn list_size(configured_columns: u32, n: usize) -> (i32, i32) {
    let columns = effective_columns(configured_columns, n);
    let rows = (n as i32 + columns - 1) / columns;
    (columns * LIST_WIDTH, LIST_HEADER_HEIGHT + rows * LIST_ROW_HEIGHT)
}

/// Which row (in display order) a click at (x, y) inside the list hits; None for the header or an empty cell.
pub fn list_row_at(configured_columns: u32, n: usize, x: i32, y: i32) -> Option<usize> {
    if y < LIST_HEADER_HEIGHT || x < 0 {
        return None;
    }
    let columns = effective_columns(configured_columns, n);
    let row = ((y - LIST_HEADER_HEIGHT) / LIST_ROW_HEIGHT) as usize;
    let col = (x / LIST_WIDTH).clamp(0, columns - 1) as usize;
    let index = row * columns as usize + col;
    (index < n).then_some(index)
}

/// One tracked client, as the list needs it.
pub struct ListItem<'a> {
    pub character_name: &'a str,
    pub display_name: &'a str,
    pub state: ThumbnailState,
    pub excluded: bool,
    pub newest_notification: Option<&'a ActiveNotification>,
    pub system_name: &'a str,
    /// The resolved system name colour (ARGB or RGB).
    pub system_color: u32,
    pub character_color: Option<u32>,
    /// The character's own active border colour, if it has one.
    pub active_border_override: Option<u32>,
    pub metrics: &'a Metrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Badge {
    Dot(u32),
    /// Excluded from hotkey cycling: a grey dot with a red cross.
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    /// Index into the items the rows were built from.
    pub source: usize,
    pub background: u32,
    pub badge: Badge,
    pub name: String,
    pub name_color: u32,
    /// The newest notification, else live stats, else the system name (when shown); may be empty.
    pub right_text: String,
    pub right_color: u32,
}

fn active_badge_color(cfg: &Config, item: &ListItem<'_>) -> u32 {
    let color = item.active_border_override.unwrap_or_else(|| cfg.thumbnail.active.border_color_or(cfg.thumbnail.border_color));
    if color == DEFAULT_ACTIVE_BORDER {
        BADGE_ACTIVE
    } else {
        color & 0xFF_FFFF
    }
}

fn shows(value: Option<f32>) -> bool {
    value.is_none_or(|v| v > 0.0)
}

/// The compact "IN:120 OUT:80 M:12.5 ISK:2.5m" stat text for a row; empty when no stat is live.
pub fn list_stat_text(cfg: &Config, m: &Metrics) -> String {
    let mut parts: Vec<String> = Vec::new();
    let combat = &cfg.combat;
    if combat.enabled && m.has_dps_data {
        let value = |dps: Option<f32>| dps.map_or_else(|| "??".to_owned(), |d| format!("{d:.0}"));
        if combat.show_incoming && shows(m.incoming_dps) {
            parts.push(format!("{}{}", if combat.incoming_show_prefix { "IN:" } else { "" }, value(m.incoming_dps)));
        }
        if combat.show_outgoing && shows(m.outgoing_dps) {
            parts.push(format!("{}{}", if combat.outgoing_show_prefix { "OUT:" } else { "" }, value(m.outgoing_dps)));
        }
    }
    let mining = &cfg.mining;
    if mining.enabled && m.has_mining_data && shows(m.mining_rate) {
        let prefix = if mining.show_prefix { "M:" } else { "" };
        parts.push(match m.mining_rate {
            Some(rate) if rate * 60.0 < 10.0 => format!("{prefix}{:.1}", rate * 60.0),
            Some(rate) => format!("{prefix}{:.0}", rate * 60.0),
            None => format!("{prefix}??"),
        });
    }
    let bounty = &cfg.bounty;
    if bounty.enabled && m.has_bounty_data && shows(m.bounty_isk_rate) {
        let prefix = if bounty.show_prefix { "ISK:" } else { "" };
        let period = if bounty.isk_rate_unit == IskRateUnit::Hour { 3600.0 } else { 60.0 };
        parts.push(match m.bounty_isk_rate {
            Some(rate) => format!("{prefix}{}", format_isk_abbrev(rate * period)),
            None => format!("{prefix}??"),
        });
    }
    parts.join(" ")
}

/// The stat text's colour: incoming DPS first (most urgent), then outgoing, mining, bounty.
pub fn list_stat_color(cfg: &Config, m: &Metrics) -> u32 {
    let (combat, mining, bounty) = (&cfg.combat, &cfg.mining, &cfg.bounty);
    if combat.enabled && m.has_dps_data && combat.show_incoming && shows(m.incoming_dps) {
        return combat.incoming_color & 0xFF_FFFF;
    }
    if combat.enabled && m.has_dps_data && combat.show_outgoing && shows(m.outgoing_dps) {
        return combat.outgoing_color & 0xFF_FFFF;
    }
    if mining.enabled && m.has_mining_data && shows(m.mining_rate) {
        return mining.color & 0xFF_FFFF;
    }
    if bounty.enabled && m.has_bounty_data && shows(m.bounty_isk_rate) {
        return bounty.color & 0xFF_FFFF;
    }
    LIST_SYSTEM_TEXT
}

/// Display order for the rows: listViewOrder's tracked, alphabetical (by shown name, then character name) or
/// configured-character order.
pub fn list_order(cfg: &Config, items: &[ListItem<'_>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    match cfg.display.list_view_order {
        ListViewOrder::Tracked => {}
        ListViewOrder::Alphabetical => order.sort_by(|&a, &b| {
            let (x, y) = (&items[a], &items[b]);
            x.display_name
                .to_ascii_lowercase()
                .cmp(&y.display_name.to_ascii_lowercase())
                .then_with(|| x.character_name.to_ascii_lowercase().cmp(&y.character_name.to_ascii_lowercase()))
                .then(a.cmp(&b))
        }),
        ListViewOrder::ConfiguredCharacters => {
            let map = build_character_order_map(&cfg.characters);
            order.sort_by(|&a, &b| {
                if order_map_less_than(&map, items[a].character_name, items[b].character_name, a, b) {
                    std::cmp::Ordering::Less
                } else if order_map_less_than(&map, items[b].character_name, items[a].character_name, b, a) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });
        }
    }
    order
}

/// The list's rows in display order, everything resolved.
pub fn list_rows(cfg: &Config, items: &[ListItem<'_>]) -> Vec<ListRow> {
    list_order(cfg, items)
        .into_iter()
        .map(|source| {
            let item = &items[source];
            let active = item.state == ThumbnailState::Active;
            let alert = item.state == ThumbnailState::Alert;
            let background = match item.newest_notification.and_then(|n| n.border_color_override) {
                // Alert rows take a faint tint of the newest notification's border colour.
                Some(c) if alert && !active => {
                    let r = (((c >> 16) & 0xFF) * 35 / 255 + 0x1A) & 0xFF;
                    let g = (((c >> 8) & 0xFF) * 35 / 255 + 0x1A) & 0xFF;
                    let b = ((c & 0xFF) * 35 / 255 + 0x20) & 0xFF;
                    (r << 16) | (g << 8) | b
                }
                _ if alert => LIST_ROW_ALERT_BG,
                _ => LIST_ROW_BG,
            };
            let badge = if item.excluded {
                Badge::Excluded
            } else {
                Badge::Dot(match item.state {
                    ThumbnailState::Active => active_badge_color(cfg, item),
                    ThumbnailState::Alert => BADGE_ALERT,
                    ThumbnailState::Minimized => BADGE_MINIMIZED,
                    _ => BADGE_INACTIVE,
                })
            };
            let stat = list_stat_text(cfg, item.metrics);
            let (right_text, right_color) = if let Some(n) = item.newest_notification {
                (n.text.clone(), n.text_color_override.unwrap_or(LIST_NOTIFICATION_TEXT) & 0xFF_FFFF)
            } else if !stat.is_empty() {
                let color = list_stat_color(cfg, item.metrics);
                (stat, color)
            } else if cfg.thumbnail.show_system_name {
                let color = if item.system_name.is_empty() { LIST_SYSTEM_TEXT } else { item.system_color & 0xFF_FFFF };
                (item.system_name.to_owned(), color)
            } else {
                (String::new(), LIST_SYSTEM_TEXT)
            };
            ListRow {
                source,
                background,
                badge,
                name: item.display_name.to_owned(),
                name_color: item.character_color.unwrap_or(LIST_NAME_TEXT) & 0xFF_FFFF,
                right_text,
                right_color,
            }
        })
        .collect()
}

// ----------------------------------------------------------------------------------------------- history panel --

pub const HISTORY_CAPACITY: usize = 30;
pub const HISTORY_HEADER_HEIGHT: i32 = 18;
pub const HISTORY_FOOTER_HEIGHT: i32 = 18;
pub const HISTORY_ROW_HEIGHT: i32 = 16;
pub const HISTORY_TEXT_LEFT: i32 = 6;
pub const HISTORY_RIGHT_MARGIN: i32 = 6;
pub const HISTORY_HEADER_BG: u32 = 0x1A1A1A;
pub const HISTORY_BODY_BG: u32 = 0x0F0F0F;
pub const HISTORY_NAME_TEXT: u32 = 0xCCCCCC;
pub const HISTORY_DIM_TEXT: u32 = 0x666666;
pub const HISTORY_BUTTON_ACTIVE_BG: u32 = 0x404040;
pub const HISTORY_TITLE: &str = "Notification History";
const NAME_MAX_BYTES: usize = 64;
const TEXT_MAX_BYTES: usize = 96;

/// The footer's filter buttons, left to right.
pub const CATEGORIES: [(NotificationCategory, &str); 5] = [
    (NotificationCategory::Fleet, "FLT"),
    (NotificationCategory::Mining, "MIN"),
    (NotificationCategory::Combat, "CBT"),
    (NotificationCategory::Navigation, "NAV"),
    (NotificationCategory::General, "GEN"),
];

pub fn category_enabled(display: &DisplayConfig, category: NotificationCategory) -> bool {
    match category {
        NotificationCategory::Fleet => display.notif_info_panel_show_fleet,
        NotificationCategory::Mining => display.notif_info_panel_show_mining,
        NotificationCategory::Combat => display.notif_info_panel_show_combat,
        NotificationCategory::Navigation => display.notif_info_panel_show_navigation,
        NotificationCategory::General => display.notif_info_panel_show_general,
    }
}

pub fn set_category_enabled(display: &mut DisplayConfig, category: NotificationCategory, on: bool) {
    match category {
        NotificationCategory::Fleet => display.notif_info_panel_show_fleet = on,
        NotificationCategory::Mining => display.notif_info_panel_show_mining = on,
        NotificationCategory::Combat => display.notif_info_panel_show_combat = on,
        NotificationCategory::Navigation => display.notif_info_panel_show_navigation = on,
        NotificationCategory::General => display.notif_info_panel_show_general = on,
    }
}

/// With the filter buttons hidden, nothing is filtered out by a state the user can't see or change.
pub fn category_shown(display: &DisplayConfig, category: NotificationCategory) -> bool {
    !display.notif_info_panel_show_category_filters || category_enabled(display, category)
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry<W> {
    /// The client the notification came from (clicking the row focuses it).
    pub source: W,
    pub notification_type: NotificationType,
    pub character_name: String,
    pub text: String,
    pub timestamp_ms: u64,
    /// The character's name colour when it was recorded.
    pub character_color: Option<u32>,
}

/// The last HISTORY_CAPACITY notifications shown, across all characters.
#[derive(Debug, Clone)]
pub struct NotificationHistory<W> {
    entries: VecDeque<HistoryEntry<W>>,
}

impl<W> Default for NotificationHistory<W> {
    fn default() -> Self {
        Self { entries: VecDeque::with_capacity(HISTORY_CAPACITY) }
    }
}

fn clip(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

impl<W> NotificationHistory<W> {
    /// `character_color`: the character's resolved name colour now (a past row keeps the colour it had).
    pub fn push(&mut self, source: W, character_name: &str, character_color: Option<u32>, text: &str, notification_type: NotificationType, now_ms: u64) {
        if self.entries.len() == HISTORY_CAPACITY {
            self.entries.pop_front();
        }
        let character_name = clip(character_name, NAME_MAX_BYTES);
        self.entries.push_back(HistoryEntry { source, notification_type, character_name, text: clip(text, TEXT_MAX_BYTES), timestamp_ms: now_ms, character_color });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn newest_first(&self) -> impl Iterator<Item = &HistoryEntry<W>> {
        self.entries.iter().rev()
    }
}

/// "just now", "5m ago", "2h ago".
pub fn format_relative_time(elapsed_ms: u64) -> String {
    let s = elapsed_ms / 1000;
    if s < 60 {
        "just now".to_owned()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else {
        format!("{}h ago", s / 3600)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow<W> {
    pub source: W,
    pub name: String,
    pub name_color: u32,
    pub text: String,
    pub text_color: u32,
    pub timestamp: Option<String>,
}

/// Where the rows end: the footer's top when the filters show, else the bottom.
pub fn history_footer_top(display: &DisplayConfig, height: i32) -> i32 {
    if display.notif_info_panel_show_category_filters {
        (height - HISTORY_FOOTER_HEIGHT).max(HISTORY_HEADER_HEIGHT)
    } else {
        height
    }
}

/// The rows that fit in a panel of `height`, newest first, after the category filter and notifInfoPanelMaxRows.
pub fn history_rows<W: Copy>(cfg: &Config, history: &NotificationHistory<W>, height: i32, now_ms: u64) -> Vec<HistoryRow<W>> {
    let display = &cfg.display;
    let fit = ((history_footer_top(display, height) - HISTORY_HEADER_HEIGHT).max(0) / HISTORY_ROW_HEIGHT) as usize;
    let cap = fit.min(display.notif_info_panel_max_rows.max(1) as usize);
    history
        .newest_first()
        .filter(|e| category_shown(display, e.notification_type.category()))
        .take(cap)
        .map(|e| HistoryRow {
            source: e.source,
            name: e.character_name.clone(),
            name_color: e.character_color.unwrap_or(HISTORY_NAME_TEXT) & 0xFF_FFFF,
            text: e.text.clone(),
            text_color: cfg.thumbnail.notifications.type_config(e.notification_type).text_color.unwrap_or(cfg.thumbnail.character_name_color) & 0xFF_FFFF,
            timestamp: display.notif_info_panel_show_timestamp.then(|| format_relative_time(now_ms.saturating_sub(e.timestamp_ms))),
        })
        .collect()
}

/// What an empty panel says.
pub fn history_empty_text(history_len: usize) -> &'static str {
    if history_len == 0 {
        "No notifications yet"
    } else {
        "All notifications filtered"
    }
}

/// The footer buttons' [left, right) spans across a panel `width` wide, in CATEGORIES order.
pub fn history_button_spans(width: i32) -> [(i32, i32); 5] {
    let n = CATEGORIES.len() as i64;
    std::array::from_fn(|i| ((width as i64 * i as i64 / n) as i32, (width as i64 * (i as i64 + 1) / n) as i32))
}

/// Whether the History Panel is on screen: showNotifInfoPanel, unless hideNotifInfoPanelWhenNoCharacters hides it
/// while nobody is logged in. Turning it on from the tray shows it regardless until characters next log out.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryPanelVisibility {
    force_visible: bool,
    had_characters: bool,
}

impl HistoryPanelVisibility {
    /// Call once per update with whether any tracked client is logged in.
    pub fn update(&mut self, any_logged_in: bool) {
        if self.had_characters && !any_logged_in {
            self.force_visible = false;
        }
        self.had_characters = any_logged_in;
    }

    pub fn is_visible(&self, display: &DisplayConfig, any_logged_in: bool) -> bool {
        display.show_notif_info_panel && (!display.hide_notif_info_panel_when_no_characters || any_logged_in || self.force_visible)
    }

    /// The tray's "Show History Panel": off when it's showing, else on (and forced past the no-characters rule).
    pub fn toggle(&mut self, display: &mut DisplayConfig, any_logged_in: bool) {
        if self.is_visible(display, any_logged_in) {
            display.show_notif_info_panel = false;
            self.force_visible = false;
        } else {
            display.show_notif_info_panel = true;
            self.force_visible = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CharacterConfig;

    #[test]
    fn column_maths() {
        assert_eq!(effective_columns(3, 2), 2);
        assert_eq!(effective_columns(15, 20), 6);
        assert_eq!(effective_columns(0, 5), 1);
        assert_eq!(effective_columns(4, 0), 4);
        assert_eq!((0..3).map(|c| items_in_column(7, 3, c)).collect::<Vec<_>>(), vec![3, 2, 2]);
        assert_eq!(list_size(2, 5), (2 * LIST_WIDTH, LIST_HEADER_HEIGHT + 3 * LIST_ROW_HEIGHT));
        assert_eq!(list_size(1, 0), (LIST_WIDTH, LIST_HEADER_HEIGHT));
    }

    #[test]
    fn row_hit_testing() {
        assert_eq!(list_row_at(2, 3, 10, 5), None, "header");
        assert_eq!(list_row_at(2, 3, 10, LIST_HEADER_HEIGHT + 1), Some(0));
        assert_eq!(list_row_at(2, 3, LIST_WIDTH + 1, LIST_HEADER_HEIGHT + 1), Some(1));
        assert_eq!(list_row_at(2, 3, 10, LIST_HEADER_HEIGHT + LIST_ROW_HEIGHT + 1), Some(2));
        assert_eq!(list_row_at(2, 3, LIST_WIDTH + 1, LIST_HEADER_HEIGHT + LIST_ROW_HEIGHT + 1), None, "empty cell");
    }

    fn item<'a>(name: &'a str, state: ThumbnailState, m: &'a Metrics) -> ListItem<'a> {
        ListItem {
            character_name: name,
            display_name: name,
            state,
            excluded: false,
            newest_notification: None,
            system_name: "",
            system_color: 0,
            character_color: None,
            active_border_override: None,
            metrics: m,
        }
    }

    #[test]
    fn rows_follow_the_configured_order() {
        let m = Metrics::default();
        let items = [item("charlie", ThumbnailState::Inactive, &m), item("Alpha", ThumbnailState::Inactive, &m), item("bravo", ThumbnailState::Inactive, &m)];
        let mut cfg = Config::defaults_with_profile("test.json").unwrap();
        let names = |cfg: &Config| list_rows(cfg, &items).into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names(&cfg), ["charlie", "Alpha", "bravo"]);
        cfg.display.list_view_order = ListViewOrder::Alphabetical;
        assert_eq!(names(&cfg), ["Alpha", "bravo", "charlie"]);
        cfg.display.list_view_order = ListViewOrder::ConfiguredCharacters;
        cfg.characters = ["bravo", "charlie"].iter().map(|n| CharacterConfig { name: (*n).to_owned(), ..Default::default() }).collect();
        assert_eq!(names(&cfg), ["bravo", "charlie", "Alpha"]);
    }

    #[test]
    fn badges_and_right_hand_text() {
        let mut cfg = Config::defaults_with_profile("test.json").unwrap();
        cfg.thumbnail.show_system_name = true;
        let m = Metrics::default();
        let mut active = item("A", ThumbnailState::Active, &m);
        active.system_name = "Jita";
        active.system_color = 0xFF12_3456;
        let notif = ActiveNotification {
            text: "Scrammed".into(),
            notification_type: NotificationType::WarpScrambled,
            start_ms: 0,
            duration_ms: 0,
            suppress_when_focused: false,
            suppress_when_clicked: false,
            border_color_override: Some(0xFF0000),
            text_color_override: None,
            show_border: true,
            flash_border: false,
        };
        let mut alert = item("B", ThumbnailState::Alert, &m);
        alert.newest_notification = Some(&notif);
        let mut excluded = item("C", ThumbnailState::Minimized, &m);
        excluded.excluded = true;
        let rows = list_rows(&cfg, &[active, alert, excluded]);
        assert_eq!((rows[0].right_text.as_str(), rows[0].right_color), ("Jita", 0x123456));
        assert_eq!(rows[0].background, LIST_ROW_BG);
        assert_eq!((rows[1].right_text.as_str(), rows[1].right_color, &rows[1].badge), ("Scrammed", LIST_NOTIFICATION_TEXT, &Badge::Dot(BADGE_ALERT)));
        assert_eq!(rows[1].background, 0x3D1A20);
        assert_eq!(rows[2].badge, Badge::Excluded);
    }

    #[test]
    fn stat_text_formats_like_the_overlay() {
        let mut cfg = Config::defaults_with_profile("test.json").unwrap();
        cfg.combat.enabled = true;
        cfg.combat.show_incoming = true;
        cfg.combat.show_outgoing = true;
        cfg.mining.enabled = true;
        cfg.bounty.enabled = true;
        let m = Metrics {
            incoming_dps: Some(120.4),
            outgoing_dps: None,
            mining_rate: Some(0.1),
            bounty_isk_rate: Some(700.0),
            has_dps_data: true,
            has_mining_data: true,
            has_bounty_data: true,
            ..Default::default()
        };
        let in_prefix = if cfg.combat.incoming_show_prefix { "IN:" } else { "" };
        assert!(list_stat_text(&cfg, &m).starts_with(&format!("{in_prefix}120 ")));
        assert!(list_stat_text(&cfg, &m).contains("6.0"));
        assert_eq!(list_stat_color(&cfg, &m), cfg.combat.incoming_color & 0xFF_FFFF);
        assert_eq!(list_stat_text(&cfg, &Metrics::default()), "");
    }

    #[test]
    fn history_keeps_the_newest_and_filters() {
        let mut cfg = Config::defaults_with_profile("test.json").unwrap();
        cfg.display.notif_info_panel_max_rows = 3;
        let mut h = NotificationHistory::default();
        for i in 0..(HISTORY_CAPACITY + 5) {
            let t = if i % 2 == 0 { NotificationType::FleetInvite } else { NotificationType::WarpScrambled };
            h.push(i, "Pilot", None, &format!("n{i}"), t, i as u64 * 1000);
        }
        assert_eq!(h.len(), HISTORY_CAPACITY);
        let tall = 1000;
        let rows = history_rows(&cfg, &h, tall, 40_000);
        assert_eq!(rows.iter().map(|r| r.source).collect::<Vec<_>>(), vec![34, 33, 32]);
        set_category_enabled(&mut cfg.display, NotificationCategory::Fleet, false);
        assert_eq!(history_rows(&cfg, &h, tall, 0).iter().map(|r| r.source).collect::<Vec<_>>(), vec![33, 31, 29]);
        cfg.display.notif_info_panel_show_category_filters = false;
        assert_eq!(history_rows(&cfg, &h, tall, 0).len(), 3, "hidden filters don't filter");
        // Height limits rows too: header + two rows (no footer now).
        assert_eq!(history_rows(&cfg, &h, HISTORY_HEADER_HEIGHT + 2 * HISTORY_ROW_HEIGHT + 5, 0).len(), 2);
        cfg.display.notif_info_panel_show_timestamp = true;
        assert_eq!(history_rows(&cfg, &h, tall, 34_000 + 125_000)[0].timestamp.as_deref(), Some("2m ago"));
        let long = "x".repeat(200);
        h.push(99, &long, Some(0x123456), &long, NotificationType::Generic, 0);
        let newest = h.newest_first().next().unwrap();
        assert_eq!((newest.character_name.len(), newest.text.len()), (64, 96));
        h.clear();
        assert!(h.is_empty());
    }

    #[test]
    fn relative_times_and_buttons() {
        assert_eq!(format_relative_time(59_999), "just now");
        assert_eq!(format_relative_time(5 * 60_000), "5m ago");
        assert_eq!(format_relative_time(2 * 3_600_000 + 5), "2h ago");
        assert_eq!(history_button_spans(200), [(0, 40), (40, 80), (80, 120), (120, 160), (160, 200)]);
        assert_eq!(history_button_spans(203)[4], (162, 203));
    }

    #[test]
    fn panel_visibility_rules() {
        let mut d = Config::defaults_with_profile("test.json").unwrap().display.clone();
        let mut v = HistoryPanelVisibility::default();
        d.show_notif_info_panel = true;
        d.hide_notif_info_panel_when_no_characters = true;
        assert!(!v.is_visible(&d, false));
        assert!(v.is_visible(&d, true));
        // Tray toggle while hidden forces it on until characters log in and out again.
        v.toggle(&mut d, false);
        assert!(v.is_visible(&d, false));
        v.update(true);
        v.update(false);
        assert!(!v.is_visible(&d, false));
        // Toggle while visible turns it off.
        v.toggle(&mut d, true);
        assert!(!d.show_notif_info_panel && !v.is_visible(&d, true));
    }
}
