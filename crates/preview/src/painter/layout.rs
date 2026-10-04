//! Where thumbnails go and how big they are: plain saved/spawn positions, RegionFit's auto-fit grid, the
//! not-logged-in space, Display Regions, and the monitor/DPI lookups they all lean on.

use std::collections::HashMap;

use eve_maj_core::accounts_store;
use eve_maj_core::config::{build_character_order_map, order_map_less_than, DisplayConfig, Position};
use eve_maj_core::display_grid::{self, LayoutView, SlotKind};
use eve_maj_core::types::{LayoutMode, RegionFitDirection, RegionFitOrder};
use eve_maj_win::geometry::{monitor_dpi, monitor_rect, nearest_monitor, scale_pixels};
use windows_sys::Win32::Foundation::{BOOL, FALSE, HWND, LPARAM, POINT, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::*;
use crate::scout::is_generic_character_name;

/// box_width/box_height is the per-column/row share of the region, used only to pick the column count; cell_width/cell_height is the actual aspect-corrected thumbnail size used for positioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionFitGrid {
    pub columns: u32,
    pub rows: u32,
    pub box_width: i32,
    pub box_height: i32,
    pub cell_width: i32,
    pub cell_height: i32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RegionFitCap {
    width: i32,
    height: i32,
}

#[derive(Clone, Copy)]
pub struct MonitorBounds {
    pub bounds: RECT,
    pub monitor: Option<HMONITOR>,
}

#[derive(Clone, Copy)]
pub(super) struct MonitorPlacement {
    pub bounds: RECT,
    pub monitor: HMONITOR,
}

/// thumbnails-array index -> display rank, plus how many thumbnails were ranked.
pub(super) struct RegionFitDisplayOrder {
    pub ranks: Vec<usize>,
    pub count: usize,
}

pub(super) struct GridLayout {
    /// Per thumbnails-array index: claiming global cell, or None (unclaimed or carved out).
    pub slot_of: Vec<Option<usize>>,
    /// Position within its cell's grid, in configured display order.
    pub rank_in_slot: Vec<usize>,
    /// Per global cell.
    pub cells: Vec<RECT>,
    pub grids: Vec<RegionFitGrid>,
}

pub(super) fn dpi_to_scale(dpi: u32) -> f32 {
    eve_maj_win::geometry::dpi_to_scale(dpi)
}

/// DPI of whichever monitor a window currently sits on; queried live, nothing to invalidate.
pub(super) fn window_dpi(hwnd: HWND) -> u32 {
    unsafe { GetDpiForWindow(hwnd) }
}

/// DPI for the system default monitor; used as a fallback when no target monitor is configured.
pub(super) fn default_dpi() -> u32 {
    unsafe { GetDpiForSystem() }
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT { left, top, right, bottom }
}

fn to_grid_rect(r: RECT) -> display_grid::Rect {
    display_grid::Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

fn from_grid_rect(r: display_grid::Rect) -> RECT {
    rect(r.left, r.top, r.right, r.bottom)
}

pub(super) fn region_rect_from_config(cfg: &DisplayConfig) -> Option<RECT> {
    let (x, y, w, h) = (cfg.region_x?, cfg.region_y?, cfg.region_width?, cfg.region_height?);
    Some(rect(x, y, x + w, y + h))
}

pub(super) fn is_region_fit_active(cfg: &DisplayConfig) -> bool {
    cfg.layout_mode == LayoutMode::RegionFit && region_rect_from_config(cfg).is_some()
}

pub(super) fn not_logged_in_space_rect_from_config(cfg: &DisplayConfig) -> Option<RECT> {
    if !cfg.not_logged_in_space_enabled {
        return None;
    }
    let (x, y, w, h) = (cfg.not_logged_in_space_x?, cfg.not_logged_in_space_y?, cfg.not_logged_in_space_width?, cfg.not_logged_in_space_height?);
    Some(rect(x, y, x + w, y + h))
}

/// True when the not-logged-in space pulls this placeholder out of the RegionFit grid entirely, even while RegionFit is active.
pub(super) fn is_carved_out_of_region_fit(cfg: &DisplayConfig, character_name: &str) -> bool {
    is_generic_character_name(character_name) && not_logged_in_space_rect_from_config(cfg).is_some()
}

/// Every active display grid, in config order (empty unless RegionFit is on): the per-display layouts when any are set, else a v1.3.0-style single grid over the RegionFit region.
pub(super) fn layout_views(cfg: &DisplayConfig) -> Vec<LayoutView<'_>> {
    if cfg.layout_mode != LayoutMode::RegionFit {
        return Vec::new();
    }
    let mut views: Vec<LayoutView<'_>> =
        cfg.display_layouts.iter().filter(|l| l.is_active()).take(display_grid::MAX_LAYOUTS).map(|l| LayoutView { rect: l.rect(), grid: &l.grid }).collect();
    if views.is_empty() && cfg.display_grid.is_active() {
        if let Some(r) = region_rect_from_config(cfg) {
            views.push(LayoutView { rect: to_grid_rect(r), grid: &cfg.display_grid });
        }
    }
    views
}

pub(super) fn is_display_regions_active(cfg: &DisplayConfig) -> bool {
    !layout_views(cfg).is_empty()
}

/// Either flavor of managed layout (single RegionFit region or Display Regions): both reflow on rank changes, size thumbnails in absolute pixels, and skip the jump to a saved spot on login.
pub(super) fn is_layout_managed(cfg: &DisplayConfig) -> bool {
    is_region_fit_active(cfg) || is_display_regions_active(cfg)
}

/// Floors at 1px (a Win32 API-validity floor, not a usability minimum).
fn fit_aspect(box_width: i32, box_height: i32, aspect_ratio: f32) -> (i32, i32) {
    if box_width <= 0 || box_height <= 0 {
        return (1, 1);
    }
    let mut width = box_width;
    let mut height = (width as f32 / aspect_ratio).round() as i32;
    if height > box_height {
        height = box_height;
        width = (height as f32 * aspect_ratio).round() as i32;
    }
    (width.max(1), height.max(1))
}

#[allow(clippy::too_many_arguments)]
fn region_fit_grid_for_dims(region_width: i32, region_height: i32, columns: u32, rows: u32, spacing_x: i32, spacing_y: i32, aspect_ratio: f32, max_cell: Option<RegionFitCap>) -> RegionFitGrid {
    let box_width = ((region_width - spacing_x * (columns as i32 - 1)) / columns as i32).max(1);
    let box_height = ((region_height - spacing_y * (rows as i32 - 1)) / rows as i32).max(1);
    let fit_width = max_cell.map_or(box_width, |cap| box_width.min(cap.width));
    let fit_height = max_cell.map_or(box_height, |cap| box_height.min(cap.height));
    let (cell_width, cell_height) = fit_aspect(fit_width, fit_height, aspect_ratio);
    RegionFitGrid { columns, rows, box_width, box_height, cell_width, cell_height }
}

fn region_fit_cell_area(grid: RegionFitGrid) -> i64 {
    grid.cell_width as i64 * grid.cell_height as i64
}

/// How many cap-sized cells fit along one axis; used only to break area ties under a size cap (see calculate_region_fit_grid).
fn region_fit_axis_capacity(dimension: i32, spacing: i32, cell_dimension: i32) -> u32 {
    ((dimension + spacing) / (cell_dimension + spacing).max(1)).max(1) as u32
}

/// Grows one column or row at a time from a single full-region cell, whichever yields the bigger cell, so the grid grows incrementally instead of re-optimizing from scratch per count.
pub(super) fn calculate_region_fit_grid(region: RECT, count: usize, spacing_x: i32, spacing_y: i32, aspect_ratio: f32, max_cell: Option<RegionFitCap>) -> RegionFitGrid {
    let n = count.max(1) as u32;
    let region_width = region.right - region.left;
    let region_height = region.bottom - region.top;

    // Under a size cap, growing either axis often yields the same capped cell size; break that tie toward whichever axis has more natural room, so one column/row fills up before a second one starts.
    let prefer_rows_on_tie = max_cell.is_some_and(|cap| {
        region_fit_axis_capacity(region_height, spacing_y, cap.height) >= region_fit_axis_capacity(region_width, spacing_x, cap.width)
    });

    let (mut columns, mut rows) = (1u32, 1u32);
    let mut grid = region_fit_grid_for_dims(region_width, region_height, columns, rows, spacing_x, spacing_y, aspect_ratio, max_cell);
    while columns * rows < n {
        let grow_columns = region_fit_grid_for_dims(region_width, region_height, columns + 1, rows, spacing_x, spacing_y, aspect_ratio, max_cell);
        let grow_rows = region_fit_grid_for_dims(region_width, region_height, columns, rows + 1, spacing_x, spacing_y, aspect_ratio, max_cell);
        let (area_columns, area_rows) = (region_fit_cell_area(grow_columns), region_fit_cell_area(grow_rows));
        let take_columns = if area_columns != area_rows { area_columns > area_rows } else { !prefer_rows_on_tie };
        if take_columns {
            columns += 1;
            grid = grow_columns;
        } else {
            rows += 1;
            grid = grow_rows;
        }
    }
    grid
}

/// BTT/RTL directions stay within [0, rows/columns) since the region is fixed-size.
fn region_fit_col_row(direction: RegionFitDirection, index: usize, columns: u32, rows: u32) -> (i32, i32) {
    use RegionFitDirection::*;
    let (c, r) = (columns as usize, rows as usize);
    let flip_c = |v: usize| c as i32 - 1 - v as i32;
    let flip_r = |v: usize| r as i32 - 1 - v as i32;
    match direction {
        RowFirst_LTR_TTB => ((index % c) as i32, (index / c) as i32),
        RowFirst_RTL_TTB => (flip_c(index % c), (index / c) as i32),
        RowFirst_LTR_BTT => ((index % c) as i32, flip_r(index / c)),
        RowFirst_RTL_BTT => (flip_c(index % c), flip_r(index / c)),
        ColumnFirst_TTB_LTR => ((index / r) as i32, (index % r) as i32),
        ColumnFirst_BTT_LTR => ((index / r) as i32, flip_r(index % r)),
        ColumnFirst_TTB_RTL => (flip_c(index / r), (index % r) as i32),
        ColumnFirst_BTT_RTL => (flip_c(index / r), flip_r(index % r)),
    }
}

/// Takes direction/spacing explicitly so the not-logged-in space can reuse it with its own spacing.
pub(super) fn region_fit_position_for_grid(region: RECT, grid: RegionFitGrid, index: usize, direction: RegionFitDirection, spacing: i32) -> Position {
    let (col, row) = region_fit_col_row(direction, index, grid.columns, grid.rows);
    // Stride by cell size, not the wider box, so slack collects at the region's far edge instead of as gaps between thumbnails.
    Position { x: region.left + col * (grid.cell_width + spacing), y: region.top + row * (grid.cell_height + spacing) }
}

unsafe extern "system" fn monitor_enum_proc(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let data = &mut *(data as *mut (u32, u32, HMONITOR));
    if data.1 == data.0 {
        data.2 = monitor;
        // FALSE stops EnumDisplayMonitors.
        return FALSE;
    }
    data.1 += 1;
    TRUE
}

/// Monitor bounds and handle by 0-based index; None if out of range.
fn monitor_placement(monitor_index: u32, use_work_area: bool) -> Option<MonitorPlacement> {
    let mut data: (u32, u32, HMONITOR) = (monitor_index, 0, std::ptr::null_mut());
    // The result is FALSE whenever enumeration stopped early on a match, not an error.
    unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(monitor_enum_proc), &mut data as *mut _ as LPARAM) };
    if data.2.is_null() {
        SLOG.warn(format_args!("Monitor index {monitor_index} not found (total monitors available: {})", data.1));
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(data.2, &mut info) } == 0 {
        SLOG.err(format_args!("Failed to get monitor info for monitor index {monitor_index}"));
        return None;
    }
    // Work area (excludes taskbar) or full monitor bounds
    Some(MonitorPlacement { bounds: if use_work_area { info.rcWork } else { info.rcMonitor }, monitor: data.2 })
}

pub(super) fn resolve_monitor_placement(cfg: &DisplayConfig) -> Option<MonitorPlacement> {
    cfg.monitor_index.and_then(|i| monitor_placement(i, cfg.use_monitor_work_area))
}

fn monitor_bounds(nearest: Option<HMONITOR>) -> MonitorBounds {
    let mut bounds = unsafe { rect(0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let mut monitor = None;
    if let Some(m) = nearest {
        if let Some(r) = monitor_rect(m) {
            bounds = r;
            monitor = Some(m);
        }
    }
    MonitorBounds { bounds, monitor }
}

pub(super) fn cursor_monitor_bounds() -> MonitorBounds {
    let mut cursor = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut cursor) };
    monitor_bounds(nearest_monitor(cursor))
}

/// Clamps value into [min, max], warning with the axis and direction worked into the message on either bound.
fn clamp_axis_with_warn(value: &mut i32, min: i32, max: i32, axis_label: &str, low_word: &str, high_word: &str, context: &str) {
    if *value < min {
        SLOG.warn(format_args!("Thumbnail {axis_label} position {value} too far {low_word}{context}, clamping to {min}"));
        *value = min;
    } else if *value > max {
        SLOG.warn(format_args!("Thumbnail {axis_label} position {value} too far {high_word}{context}, clamping to {max}"));
        *value = max;
    }
}

impl Painter {
    /// Bounds of the monitor nearest `hwnd`, falling back to primary-monitor metrics if the lookup fails; also returns the resolved monitor handle, if any, for a DPI lookup.
    pub fn nearest_monitor_bounds(hwnd: HWND) -> MonitorBounds {
        let m = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        monitor_bounds((!m.is_null()).then_some(m))
    }

    /// total_count only matters for RegionFit; other modes ignore it. precomputed_grid is always RegionFit's own grid, never the not-logged-in space's - that one's cheap enough to recompute fresh each call.
    pub(super) fn thumbnail_size(&self, character_name: &str, total_count: usize, precomputed_grid: Option<RegionFitGrid>) -> (i32, i32) {
        let config = globals::config();
        let cfg = &config.display;
        if is_carved_out_of_region_fit(cfg, character_name) {
            let space = not_logged_in_space_rect_from_config(cfg).expect("carved out implies a space");
            let grid = self.not_logged_in_space_grid(space, self.not_logged_in_space_count());
            return (grid.cell_width, grid.cell_height);
        }
        if is_display_regions_active(cfg) {
            // Each cell has its own grid, so RegionFit's whole-region precomputed_grid doesn't apply; unclaimed characters fall through to their configured size.
            if let Some(slot) = self.grid_slot_for(character_name) {
                let count = self.grid_slot_count(slot) + usize::from(!self.has_thumbnail_named(character_name));
                let grid = self.grid_slot_grid(slot, count.max(1));
                return (grid.cell_width, grid.cell_height);
            }
        } else if cfg.layout_mode == LayoutMode::RegionFit {
            if let Some(grid) = precomputed_grid {
                return (grid.cell_width, grid.cell_height);
            }
            if let Some(region) = region_rect_from_config(cfg) {
                let grid = calculate_region_fit_grid(region, total_count, cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region));
                return (grid.cell_width, grid.cell_height);
            }
        }
        let t = &config.thumbnail;
        match config.character_size(character_name) {
            Some(size) => (size.width.unwrap_or(t.width), size.height.unwrap_or(t.height)),
            None => (t.width, t.height),
        }
    }

    /// Always false for the generic "not logged in" name, which never has a saved position of its own.
    fn has_saved_position(&self, character_name: &str) -> bool {
        let config = globals::config();
        !is_generic_character_name(character_name) && config.display.honor_saved_positions && config.character_position(character_name).is_some()
    }

    /// Ranks this thumbnail's slot in the left-to-right unpositioned spawn row.
    fn unpositioned_index(&self, up_to: usize) -> usize {
        self.thumbnails.iter().take(up_to).filter(|t| !self.has_saved_position(&t.character_name)).count()
    }

    /// Ranks this thumbnail among not-logged-in "EVE" placeholders only, for the not-logged-in space's own grid.
    pub(super) fn not_logged_in_index(&self, up_to: usize) -> usize {
        self.thumbnails.iter().take(up_to).filter(|t| is_generic_character_name(&t.character_name)).count()
    }

    /// How many tracked thumbnails actually belong in the RegionFit grid, excluding not-logged-in placeholders when that space is configured.
    pub(super) fn region_fit_grid_count(&self) -> usize {
        if not_logged_in_space_rect_from_config(&globals::config().display).is_none() {
            return self.thumbnails.len();
        }
        self.thumbnails.iter().filter(|t| !is_generic_character_name(&t.character_name)).count()
    }

    /// How many not-logged-in "EVE" placeholders currently exist, for the not-logged-in space's own grid-fit sizing.
    pub(super) fn not_logged_in_space_count(&self) -> usize {
        self.thumbnails.iter().filter(|t| is_generic_character_name(&t.character_name)).count()
    }

    /// The not-logged-in space's own auto-fit grid - same aspect-preserving column/row search as RegionFit's Thumbnail Space, but with its own spacing and size cap.
    pub(super) fn not_logged_in_space_grid(&self, space: RECT, count: usize) -> RegionFitGrid {
        let cfg = &globals::config().display;
        calculate_region_fit_grid(
            space, count, cfg.not_logged_in_space_spacing, cfg.not_logged_in_space_spacing, self.region_fit_aspect_ratio(),
            self.thumbnail_size_cap(space, cfg.not_logged_in_space_limit_to_thumbnail_size),
        )
    }

    /// RegionFit always keeps the configured thumbnail's shape; regionFitLimitToThumbnailSize additionally caps its absolute size (see region_fit_max_cell_size).
    pub(super) fn region_fit_aspect_ratio(&self) -> f32 {
        let t = &globals::config().thumbnail;
        t.width as f32 / t.height as f32
    }

    pub(super) fn region_fit_max_cell_size(&self, region: RECT) -> Option<RegionFitCap> {
        self.thumbnail_size_cap(region, globals::config().display.region_fit_limit_to_thumbnail_size)
    }

    /// Physical-pixel cap on a space's cell size, DPI-scaled for the space's own monitor (which may differ from the app's configured monitor); None when `limit_enabled` is off.
    fn thumbnail_size_cap(&self, region: RECT, limit_enabled: bool) -> Option<RegionFitCap> {
        if !limit_enabled {
            return None;
        }
        let center = POINT { x: (region.left + region.right) / 2, y: (region.top + region.bottom) / 2 };
        let dpi = nearest_monitor(center).map_or_else(default_dpi, monitor_dpi);
        let scale = dpi_to_scale(dpi);
        let t = &globals::config().thumbnail;
        Some(RegionFitCap { width: scale_pixels(t.width, scale), height: scale_pixels(t.height, scale) })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn calculate_thumbnail_position(&self, character_name: &str, thumb_width: i32, thumb_height: i32, index: usize, total_count: usize, monitor_bounds: Option<RECT>, scale: f32) -> Position {
        let config = globals::config();
        let cfg = &config.display;

        // Checked first: the not-logged-in space takes priority over RegionFit for these placeholders.
        if is_generic_character_name(character_name) {
            if let Some(space) = not_logged_in_space_rect_from_config(cfg) {
                let grid = self.not_logged_in_space_grid(space, self.not_logged_in_space_count());
                return region_fit_position_for_grid(space, grid, self.not_logged_in_index(index), cfg.region_fit_direction, cfg.not_logged_in_space_spacing);
            }
        }

        // Checked before saved positions, which it replaces entirely while active.
        if is_display_regions_active(cfg) {
            // Display Regions: a first placement by array order within the claiming cell; the reflow that follows a new thumbnail re-sorts each cell by configured order. Unclaimed characters fall through to saved/spawn positions.
            if let Some(slot) = self.grid_slot_for(character_name) {
                let rank = self
                    .thumbnails
                    .iter()
                    .take(index)
                    .filter(|t| !is_carved_out_of_region_fit(cfg, &t.character_name) && self.grid_slot_for(&t.character_name) == Some(slot))
                    .count();
                let count = self.grid_slot_count(slot) + usize::from(!self.has_thumbnail_named(character_name));
                let grid = self.grid_slot_grid(slot, count.max(1));
                return region_fit_position_for_grid(self.grid_cell_rect(slot), grid, rank, self.grid_slot_direction(slot), cfg.spacing);
            }
        } else if cfg.layout_mode == LayoutMode::RegionFit {
            if let Some(region) = region_rect_from_config(cfg) {
                let grid = calculate_region_fit_grid(region, total_count, cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region));
                return region_fit_position_for_grid(region, grid, index, cfg.region_fit_direction, cfg.spacing);
            }
        }

        if self.has_saved_position(character_name) {
            // Saved positions are absolute physical pixels (see save_thumbnail_position), so scaling doesn't apply.
            let saved = config.character_position(character_name).expect("checked by has_saved_position");
            SLOG.debug(format_args!("Using saved position for {character_name}: ({}, {})", saved.x, saved.y));
            return saved;
        }

        // No saved position: flow left-to-right from startX/startY, wrapping to a new row instead of overlapping once a row runs out of width.
        let slot = self.unpositioned_index(index) as i32;
        let step_x = thumb_width + scale_pixels(cfg.new_thumbnail_spacing, scale);
        let step_y = thumb_height + scale_pixels(cfg.new_thumbnail_spacing, scale);

        // Monitor bounds if one's configured, otherwise the real current virtual screen.
        let bounds = monitor_bounds.unwrap_or_else(|| unsafe {
            let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            rect(x, y, x + GetSystemMetrics(SM_CXVIRTUALSCREEN), y + GetSystemMetrics(SM_CYVIRTUALSCREEN))
        });

        // startX/startY are monitor-relative when a monitor is configured, absolute otherwise.
        let start_x = scale_pixels(cfg.start_x, scale) + if monitor_bounds.is_some() { bounds.left } else { 0 };
        let start_y = scale_pixels(cfg.start_y, scale) + if monitor_bounds.is_some() { bounds.top } else { 0 };

        let columns_per_row = ((bounds.right - start_x) / step_x.max(1)).max(1);
        let row = slot / columns_per_row;
        let col = slot.rem_euclid(columns_per_row);
        SLOG.debug(format_args!("Thumbnail #{index} has no saved position, spawning at unpositioned slot {slot} (row {row}, col {col})"));

        let mut pos = Position { x: start_x + col * step_x, y: start_y + row * step_y };

        // Clamp to keep thumbnails from spawning fully off-screen, while still allowing edge placement; only bites once rows also overflow the screen's height.
        const CLAMP_MARGIN: i32 = 50;
        let suffix = if monitor_bounds.is_some() { " for monitor" } else { "" };
        clamp_axis_with_warn(&mut pos.x, bounds.left - CLAMP_MARGIN, bounds.right - thumb_width + CLAMP_MARGIN, "X", "left", "right", suffix);
        clamp_axis_with_warn(&mut pos.y, bounds.top - CLAMP_MARGIN, bounds.bottom - thumb_height + CLAMP_MARGIN, "Y", "up", "down", suffix);
        pos
    }

    /// The global cell (across every display's grid) that claims `name`, or None if none does.
    pub(super) fn grid_slot_for(&self, name: &str) -> Option<usize> {
        let views = layout_views(&globals::config().display);
        display_grid::assign_slot_multi(&views, name, accounts_store::account_of(&self.account_membership, name))
    }

    /// Whether `name`'s size comes from an auto-fit grid (absolute physical pixels) rather than the DPI-scaled configured size.
    pub(super) fn uses_grid_size(&self, name: &str) -> bool {
        let cfg = &globals::config().display;
        if is_carved_out_of_region_fit(cfg, name) {
            return true;
        }
        if is_display_regions_active(cfg) {
            return self.grid_slot_for(name).is_some();
        }
        is_region_fit_active(cfg)
    }

    fn has_thumbnail_named(&self, name: &str) -> bool {
        self.thumbnails.iter().any(|t| t.character_name == name)
    }

    fn grid_cell_rect(&self, slot: usize) -> RECT {
        let cfg = &globals::config().display;
        display_grid::cell_rect_multi(&layout_views(cfg), slot, cfg.spacing).map_or(rect(0, 0, 0, 0), from_grid_rect)
    }

    /// The order `slot` arranges its thumbnails in: a leftover-taking Empty region's own fill order, else the global Region Fit direction (an unknown name also falls back to it).
    pub(super) fn grid_slot_direction(&self, slot: usize) -> RegionFitDirection {
        let cfg = &globals::config().display;
        display_grid::slot_fill_order(&layout_views(cfg), slot)
            .and_then(eve_maj_core::config::serde_helpers::enum_from_str)
            .unwrap_or(cfg.region_fit_direction)
    }

    /// How many current thumbnails `slot` claims (not-logged-in placeholders excluded).
    fn grid_slot_count(&self, slot: usize) -> usize {
        let cfg = &globals::config().display;
        self.thumbnails.iter().filter(|t| !is_carved_out_of_region_fit(cfg, &t.character_name) && self.grid_slot_for(&t.character_name) == Some(slot)).count()
    }

    fn grid_slot_grid(&self, slot: usize, count: usize) -> RegionFitGrid {
        let cfg = &globals::config().display;
        let views = layout_views(cfg);
        let fit_to_grid = display_grid::locate(&views, slot).is_some_and(|r| views[r.view].grid.fit_to_grid);
        let cell = self.grid_cell_rect(slot);
        let spacing = cfg.spacing;
        // Fit to Grid fills the cell edge to edge (ignoring the thumbnail-size cap); otherwise auto-fit keeps the configured thumbnail shape.
        if fit_to_grid {
            let fill = display_grid::fill_grid(to_grid_rect(cell), count, spacing, self.region_fit_aspect_ratio());
            return RegionFitGrid { columns: fill.columns, rows: fill.rows, box_width: fill.tile_width, box_height: fill.tile_height, cell_width: fill.tile_width, cell_height: fill.tile_height };
        }
        calculate_region_fit_grid(cell, count, spacing, spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(cell))
    }

    /// Unconditional re-read of accounts.json, for the Key Binding tab's account hotkeys.
    pub fn refresh_account_membership(&mut self) {
        self.account_membership = accounts_store::load_membership();
    }

    /// Only account cells need membership, so skip the file read otherwise.
    pub(super) fn reload_account_membership_if_needed(&mut self) {
        let cfg = &globals::config().display;
        let needs = layout_views(cfg).iter().any(|v| v.grid.slots.iter().take(v.grid.cell_count()).any(|s| s.kind == SlotKind::Account));
        if needs {
            self.account_membership = accounts_store::load_membership();
        }
    }

    /// One pass over the thumbnails: which cell claims each, its rank there (following RegionFit's configured display order), and each cell's grid sized for its member count.
    pub(super) fn compute_grid_layout(&self, display_order: Option<&RegionFitDisplayOrder>) -> GridLayout {
        let cfg = &globals::config().display;
        let total_cells = display_grid::total_cells(&layout_views(cfg));
        let n = self.thumbnails.len();
        let mut slot_of = vec![None; n];
        let mut rank_in_slot = vec![0usize; n];
        let mut counts = vec![0usize; total_cells];

        // Thumbnail indices in display order: display_order ranks the non-carved ones; fall back to array order.
        let ordered: Vec<usize> = match display_order {
            Some(order) => {
                let mut ordered = vec![usize::MAX; order.count];
                for (i, t) in self.thumbnails.iter().enumerate() {
                    if is_carved_out_of_region_fit(cfg, &t.character_name) {
                        continue;
                    }
                    if order.ranks[i] < order.count {
                        ordered[order.ranks[i]] = i;
                    }
                }
                ordered.retain(|&i| i != usize::MAX);
                ordered
            }
            None => (0..n).filter(|&i| !is_carved_out_of_region_fit(cfg, &self.thumbnails[i].character_name)).collect(),
        };

        for i in ordered {
            let Some(slot) = self.grid_slot_for(&self.thumbnails[i].character_name) else { continue };
            if slot >= total_cells {
                continue;
            }
            slot_of[i] = Some(slot);
            rank_in_slot[i] = counts[slot];
            counts[slot] += 1;
        }

        let cells: Vec<RECT> = (0..total_cells).map(|slot| self.grid_cell_rect(slot)).collect();
        let grids = (0..total_cells).map(|slot| self.grid_slot_grid(slot, counts[slot].max(1))).collect();
        GridLayout { slot_of, rank_in_slot, cells, grids }
    }

    /// Ranks each tracked thumbnail per display.regionFitOrder; unranked characters sort last.
    /// carve_out excludes not-logged-in placeholders entirely, so `count`/ranks reflect only what belongs in the RegionFit grid.
    pub(super) fn compute_region_fit_display_order(&self, carve_out: bool) -> RegionFitDisplayOrder {
        let config = globals::config();
        let order_map: HashMap<&str, usize> = match config.display.region_fit_order {
            RegionFitOrder::Characters => build_character_order_map(&config.characters),
            RegionFitOrder::HotkeyGroups => {
                let mut map = HashMap::new();
                for name in config.hotkey_groups.iter().flat_map(|g| &g.characters) {
                    let rank = map.len();
                    map.entry(name.as_str()).or_insert(rank);
                }
                map
            }
        };

        let mut used: Vec<usize> = (0..self.thumbnails.len()).filter(|&i| !(carve_out && is_generic_character_name(&self.thumbnails[i].character_name))).collect();
        // Generic placeholders sort after everyone (checked before order_map, or an unranked logged-in character would tie with a placeholder and sort by array order instead).
        used.sort_by(|&a, &b| {
            let (an, bn) = (&self.thumbnails[a].character_name, &self.thumbnails[b].character_name);
            let (ag, bg) = (is_generic_character_name(an), is_generic_character_name(bn));
            let less = if ag != bg { !ag } else { order_map_less_than(&order_map, an, bn, a, b) };
            let greater = if ag != bg { ag } else { order_map_less_than(&order_map, bn, an, b, a) };
            match (less, greater) {
                (true, _) => std::cmp::Ordering::Less,
                (_, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });

        // Carved-out entries keep their zero rank; it's never read since they route to the not-logged-in space instead.
        let mut ranks = vec![0usize; self.thumbnails.len()];
        for (display_index, &thumb_index) in used.iter().enumerate() {
            ranks[thumb_index] = display_index;
        }
        RegionFitDisplayOrder { ranks, count: used.len() }
    }
}
