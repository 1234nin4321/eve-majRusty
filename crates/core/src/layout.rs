//! Where thumbnails go and how big they are, independent of any windowing system: plain saved/spawn positions,
//! RegionFit's auto-fit grid, the not-logged-in space and Display Regions.
//!
//! This is the platform-neutral twin of the Windows painter's `layout.rs` + `reposition_all_thumbnails`, written so a
//! front end only has to supply the tracked character names (in creation order), the monitor geometry, and a DPI
//! scale lookup. The Linux/Hyprland front end uses it today; the Windows painter can switch over to it later.

use std::collections::HashMap;

use crate::accounts_store::{self, Membership};
use crate::config::{build_character_order_map, order_map_less_than, Config, DisplayConfig, Position};
use crate::display_grid::{self, LayoutView, Rect};
use crate::types::{LayoutMode, RegionFitDirection, RegionFitOrder};

/// character_name before login or after logout while the client window stays open.
pub const GENERIC_CHARACTER_NAME: &str = "EVE";

pub fn is_generic_character_name(name: &str) -> bool {
    name == GENERIC_CHARACTER_NAME
}

/// Scales a logical (96-DPI) pixel value to the given monitor scale factor, rounding to nearest.
pub fn scale_pixels(value: i32, scale: f32) -> i32 {
    if scale == 1.0 {
        return value;
    }
    (value as f32 * scale).round() as i32
}

pub fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect { left, top, right, bottom }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionFitCap {
    pub width: i32,
    pub height: i32,
}

/// thumbnails-array index -> display rank, plus how many thumbnails were ranked.
pub struct RegionFitDisplayOrder {
    pub ranks: Vec<usize>,
    pub count: usize,
}

pub struct GridLayout {
    /// Per thumbnails-array index: claiming global cell, or None (unclaimed or carved out).
    pub slot_of: Vec<Option<usize>>,
    /// Position within its cell's grid, in configured display order.
    pub rank_in_slot: Vec<usize>,
    /// Per global cell.
    pub cells: Vec<Rect>,
    pub grids: Vec<RegionFitGrid>,
}

/// One thumbnail's resolved on-screen rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn region_rect_from_config(cfg: &DisplayConfig) -> Option<Rect> {
    let (x, y, w, h) = (cfg.region_x?, cfg.region_y?, cfg.region_width?, cfg.region_height?);
    Some(rect(x, y, x + w, y + h))
}

pub fn is_region_fit_active(cfg: &DisplayConfig) -> bool {
    cfg.layout_mode == LayoutMode::RegionFit && region_rect_from_config(cfg).is_some()
}

pub fn not_logged_in_space_rect_from_config(cfg: &DisplayConfig) -> Option<Rect> {
    if !cfg.not_logged_in_space_enabled {
        return None;
    }
    let (x, y, w, h) = (cfg.not_logged_in_space_x?, cfg.not_logged_in_space_y?, cfg.not_logged_in_space_width?, cfg.not_logged_in_space_height?);
    Some(rect(x, y, x + w, y + h))
}

/// True when the not-logged-in space pulls this placeholder out of the RegionFit grid entirely, even while RegionFit is active.
pub fn is_carved_out_of_region_fit(cfg: &DisplayConfig, character_name: &str) -> bool {
    is_generic_character_name(character_name) && not_logged_in_space_rect_from_config(cfg).is_some()
}

/// Every active display grid, in config order (empty unless RegionFit is on): the per-display layouts when any are set, else a v1.3.0-style single grid over the RegionFit region.
pub fn layout_views(cfg: &DisplayConfig) -> Vec<LayoutView<'_>> {
    if cfg.layout_mode != LayoutMode::RegionFit {
        return Vec::new();
    }
    let mut views: Vec<LayoutView<'_>> =
        cfg.display_layouts.iter().filter(|l| l.is_active()).take(display_grid::MAX_LAYOUTS).map(|l| LayoutView { rect: l.rect(), grid: &l.grid }).collect();
    if views.is_empty() && cfg.display_grid.is_active() {
        if let Some(r) = region_rect_from_config(cfg) {
            views.push(LayoutView { rect: r, grid: &cfg.display_grid });
        }
    }
    views
}

pub fn is_display_regions_active(cfg: &DisplayConfig) -> bool {
    !layout_views(cfg).is_empty()
}

/// Either flavor of managed layout (single RegionFit region or Display Regions): both reflow on rank changes, size thumbnails in absolute pixels, and skip the jump to a saved spot on login.
pub fn is_layout_managed(cfg: &DisplayConfig) -> bool {
    is_region_fit_active(cfg) || is_display_regions_active(cfg)
}

/// Floors at 1px.
pub fn fit_aspect(box_width: i32, box_height: i32, aspect_ratio: f32) -> (i32, i32) {
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

/// How many cap-sized cells fit along one axis; used only to break area ties under a size cap.
fn region_fit_axis_capacity(dimension: i32, spacing: i32, cell_dimension: i32) -> u32 {
    ((dimension + spacing) / (cell_dimension + spacing).max(1)).max(1) as u32
}

/// Grows one column or row at a time from a single full-region cell, whichever yields the bigger cell.
pub fn calculate_region_fit_grid(region: Rect, count: usize, spacing_x: i32, spacing_y: i32, aspect_ratio: f32, max_cell: Option<RegionFitCap>) -> RegionFitGrid {
    let n = count.max(1) as u32;
    let region_width = region.right - region.left;
    let region_height = region.bottom - region.top;

    // Under a size cap, growing either axis often yields the same capped cell size; break that tie toward whichever axis has more natural room.
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
pub fn region_fit_col_row(direction: RegionFitDirection, index: usize, columns: u32, rows: u32) -> (i32, i32) {
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
pub fn region_fit_position_for_grid(region: Rect, grid: RegionFitGrid, index: usize, direction: RegionFitDirection, spacing: i32) -> Position {
    let (col, row) = region_fit_col_row(direction, index, grid.columns, grid.rows);
    // Stride by cell size, not the wider box, so slack collects at the region's far edge instead of as gaps between thumbnails.
    Position { x: region.left + col * (grid.cell_width + spacing), y: region.top + row * (grid.cell_height + spacing) }
}

/// Monitor geometry the layout needs from the front end, in the same coordinate space as saved positions.
pub struct Screens<'a> {
    /// The configured `display.monitorIndex` monitor's bounds (work area if `useMonitorWorkArea`), if one is configured and exists.
    pub monitor_bounds: Option<Rect>,
    /// Bounding box of every monitor.
    pub virtual_screen: Rect,
    /// DPI scale for spawn positions and the configured thumbnail size (the configured monitor's, else the default).
    pub scale: f32,
    /// DPI scale of the monitor nearest a point, for a space's thumbnail-size cap.
    pub scale_at: &'a dyn Fn(i32, i32) -> f32,
}

/// The layout of one set of tracked thumbnails (`names`, in creation order) under one config.
pub struct Layout<'a> {
    pub config: &'a Config,
    pub names: &'a [String],
    pub membership: &'a Membership,
    pub screens: Screens<'a>,
}

impl<'a> Layout<'a> {
    fn cfg(&self) -> &'a DisplayConfig {
        &self.config.display
    }

    /// RegionFit always keeps the configured thumbnail's shape; regionFitLimitToThumbnailSize additionally caps its absolute size.
    pub fn region_fit_aspect_ratio(&self) -> f32 {
        let t = &self.config.thumbnail;
        t.width as f32 / t.height as f32
    }

    /// Pixel cap on a space's cell size, DPI-scaled for the space's own monitor; None when `limit_enabled` is off.
    fn thumbnail_size_cap(&self, region: Rect, limit_enabled: bool) -> Option<RegionFitCap> {
        if !limit_enabled {
            return None;
        }
        let scale = (self.screens.scale_at)((region.left + region.right) / 2, (region.top + region.bottom) / 2);
        let t = &self.config.thumbnail;
        Some(RegionFitCap { width: scale_pixels(t.width, scale), height: scale_pixels(t.height, scale) })
    }

    pub fn region_fit_max_cell_size(&self, region: Rect) -> Option<RegionFitCap> {
        self.thumbnail_size_cap(region, self.cfg().region_fit_limit_to_thumbnail_size)
    }

    fn has_saved_position(&self, character_name: &str) -> bool {
        !is_generic_character_name(character_name) && self.cfg().honor_saved_positions && self.config.character_position(character_name).is_some()
    }

    fn unpositioned_index(&self, up_to: usize) -> usize {
        self.names.iter().take(up_to).filter(|n| !self.has_saved_position(n)).count()
    }

    pub fn not_logged_in_index(&self, up_to: usize) -> usize {
        self.names.iter().take(up_to).filter(|n| is_generic_character_name(n)).count()
    }

    pub fn region_fit_grid_count(&self) -> usize {
        if not_logged_in_space_rect_from_config(self.cfg()).is_none() {
            return self.names.len();
        }
        self.names.iter().filter(|n| !is_generic_character_name(n)).count()
    }

    pub fn not_logged_in_space_count(&self) -> usize {
        self.names.iter().filter(|n| is_generic_character_name(n)).count()
    }

    pub fn not_logged_in_space_grid(&self, space: Rect, count: usize) -> RegionFitGrid {
        let cfg = self.cfg();
        calculate_region_fit_grid(
            space, count, cfg.not_logged_in_space_spacing, cfg.not_logged_in_space_spacing, self.region_fit_aspect_ratio(),
            self.thumbnail_size_cap(space, cfg.not_logged_in_space_limit_to_thumbnail_size),
        )
    }

    fn has_thumbnail_named(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// The global cell (across every display's grid) that claims `name`, or None if none does.
    pub fn grid_slot_for(&self, name: &str) -> Option<usize> {
        display_grid::assign_slot_multi(&layout_views(self.cfg()), name, accounts_store::account_of(self.membership, name))
    }

    /// Whether `name`'s size comes from an auto-fit grid (absolute pixels) rather than the DPI-scaled configured size.
    pub fn uses_grid_size(&self, name: &str) -> bool {
        let cfg = self.cfg();
        if is_carved_out_of_region_fit(cfg, name) {
            return true;
        }
        if is_display_regions_active(cfg) {
            return self.grid_slot_for(name).is_some();
        }
        is_region_fit_active(cfg)
    }

    fn grid_cell_rect(&self, slot: usize) -> Rect {
        let cfg = self.cfg();
        display_grid::cell_rect_multi(&layout_views(cfg), slot, cfg.spacing).unwrap_or_default()
    }

    /// A leftover-taking Empty region's own fill order, else the global Region Fit direction.
    pub fn grid_slot_direction(&self, slot: usize) -> RegionFitDirection {
        let cfg = self.cfg();
        display_grid::slot_fill_order(&layout_views(cfg), slot).and_then(crate::config::serde_helpers::enum_from_str).unwrap_or(cfg.region_fit_direction)
    }

    fn grid_slot_count(&self, slot: usize) -> usize {
        let cfg = self.cfg();
        self.names.iter().filter(|n| !is_carved_out_of_region_fit(cfg, n) && self.grid_slot_for(n) == Some(slot)).count()
    }

    fn grid_slot_grid(&self, slot: usize, count: usize) -> RegionFitGrid {
        let cfg = self.cfg();
        let views = layout_views(cfg);
        let fit_to_grid = display_grid::locate(&views, slot).is_some_and(|r| views[r.view].grid.fit_to_grid);
        let cell = self.grid_cell_rect(slot);
        let spacing = cfg.spacing;
        if fit_to_grid {
            let fill = display_grid::fill_grid(cell, count, spacing, self.region_fit_aspect_ratio());
            return RegionFitGrid { columns: fill.columns, rows: fill.rows, box_width: fill.tile_width, box_height: fill.tile_height, cell_width: fill.tile_width, cell_height: fill.tile_height };
        }
        calculate_region_fit_grid(cell, count, spacing, spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(cell))
    }

    /// Unscaled (logical) size for `character_name`, except grid sizes which are already absolute. `total_count` only matters for RegionFit.
    pub fn thumbnail_size(&self, character_name: &str, total_count: usize, precomputed_grid: Option<RegionFitGrid>) -> (i32, i32) {
        let cfg = self.cfg();
        if is_carved_out_of_region_fit(cfg, character_name) {
            let space = not_logged_in_space_rect_from_config(cfg).expect("carved out implies a space");
            let grid = self.not_logged_in_space_grid(space, self.not_logged_in_space_count());
            return (grid.cell_width, grid.cell_height);
        }
        if is_display_regions_active(cfg) {
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
        let t = &self.config.thumbnail;
        match self.config.character_size(character_name) {
            Some(size) => (size.width.unwrap_or(t.width), size.height.unwrap_or(t.height)),
            None => (t.width, t.height),
        }
    }

    /// thumbnail_size, DPI-scaled unless it already came from a grid.
    pub fn scaled_thumbnail_size(&self, character_name: &str, total_count: usize, scale: f32) -> (i32, i32) {
        let (w, h) = self.thumbnail_size(character_name, total_count, None);
        if self.uses_grid_size(character_name) { (w, h) } else { (scale_pixels(w, scale), scale_pixels(h, scale)) }
    }

    /// Position of thumbnail `index` (which may be one past the end, for one about to be created).
    pub fn calculate_thumbnail_position(&self, character_name: &str, thumb_width: i32, thumb_height: i32, index: usize, total_count: usize) -> Position {
        let cfg = self.cfg();
        let scale = self.screens.scale;
        let monitor_bounds = self.screens.monitor_bounds;

        if is_generic_character_name(character_name) {
            if let Some(space) = not_logged_in_space_rect_from_config(cfg) {
                let grid = self.not_logged_in_space_grid(space, self.not_logged_in_space_count());
                return region_fit_position_for_grid(space, grid, self.not_logged_in_index(index), cfg.region_fit_direction, cfg.not_logged_in_space_spacing);
            }
        }

        if is_display_regions_active(cfg) {
            if let Some(slot) = self.grid_slot_for(character_name) {
                let rank = self.names.iter().take(index).filter(|n| !is_carved_out_of_region_fit(cfg, n) && self.grid_slot_for(n) == Some(slot)).count();
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
            return self.config.character_position(character_name).expect("checked by has_saved_position");
        }

        // No saved position: flow left-to-right from startX/startY, wrapping to a new row once a row runs out of width.
        let slot = self.unpositioned_index(index) as i32;
        let step_x = thumb_width + scale_pixels(cfg.new_thumbnail_spacing, scale);
        let step_y = thumb_height + scale_pixels(cfg.new_thumbnail_spacing, scale);
        let bounds = monitor_bounds.unwrap_or(self.screens.virtual_screen);

        // startX/startY are monitor-relative when a monitor is configured, absolute otherwise.
        let start_x = scale_pixels(cfg.start_x, scale) + if monitor_bounds.is_some() { bounds.left } else { 0 };
        let start_y = scale_pixels(cfg.start_y, scale) + if monitor_bounds.is_some() { bounds.top } else { 0 };

        let columns_per_row = ((bounds.right - start_x) / step_x.max(1)).max(1);
        let row = slot / columns_per_row;
        let col = slot.rem_euclid(columns_per_row);
        let mut pos = Position { x: start_x + col * step_x, y: start_y + row * step_y };

        // Keep thumbnails from spawning fully off-screen, while still allowing edge placement.
        const CLAMP_MARGIN: i32 = 50;
        pos.x = pos.x.min(bounds.right - thumb_width + CLAMP_MARGIN).max(bounds.left - CLAMP_MARGIN);
        pos.y = pos.y.min(bounds.bottom - thumb_height + CLAMP_MARGIN).max(bounds.top - CLAMP_MARGIN);
        pos
    }

    /// Ranks each tracked thumbnail per display.regionFitOrder; unranked characters sort last.
    pub fn compute_region_fit_display_order(&self, carve_out: bool) -> RegionFitDisplayOrder {
        let config = self.config;
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

        let mut used: Vec<usize> = (0..self.names.len()).filter(|&i| !(carve_out && is_generic_character_name(&self.names[i]))).collect();
        // Generic placeholders sort after everyone.
        used.sort_by(|&a, &b| {
            let (an, bn) = (&self.names[a], &self.names[b]);
            let (ag, bg) = (is_generic_character_name(an), is_generic_character_name(bn));
            let less = if ag != bg { !ag } else { order_map_less_than(&order_map, an, bn, a, b) };
            let greater = if ag != bg { ag } else { order_map_less_than(&order_map, bn, an, b, a) };
            match (less, greater) {
                (true, _) => std::cmp::Ordering::Less,
                (_, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });

        let mut ranks = vec![0usize; self.names.len()];
        for (display_index, &thumb_index) in used.iter().enumerate() {
            ranks[thumb_index] = display_index;
        }
        RegionFitDisplayOrder { ranks, count: used.len() }
    }

    /// One pass over the thumbnails: which cell claims each, its rank there, and each cell's grid sized for its member count.
    pub fn compute_grid_layout(&self, display_order: Option<&RegionFitDisplayOrder>) -> GridLayout {
        let cfg = self.cfg();
        let total_cells = display_grid::total_cells(&layout_views(cfg));
        let n = self.names.len();
        let mut slot_of = vec![None; n];
        let mut rank_in_slot = vec![0usize; n];
        let mut counts = vec![0usize; total_cells];

        let ordered: Vec<usize> = match display_order {
            Some(order) => {
                let mut ordered = vec![usize::MAX; order.count];
                for (i, name) in self.names.iter().enumerate() {
                    if is_carved_out_of_region_fit(cfg, name) {
                        continue;
                    }
                    if order.ranks[i] < order.count {
                        ordered[order.ranks[i]] = i;
                    }
                }
                ordered.retain(|&i| i != usize::MAX);
                ordered
            }
            None => (0..n).filter(|&i| !is_carved_out_of_region_fit(cfg, &self.names[i])).collect(),
        };

        for i in ordered {
            let Some(slot) = self.grid_slot_for(&self.names[i]) else { continue };
            if slot >= total_cells {
                continue;
            }
            slot_of[i] = Some(slot);
            rank_in_slot[i] = counts[slot];
            counts[slot] += 1;
        }

        let cells: Vec<Rect> = (0..total_cells).map(|slot| self.grid_cell_rect(slot)).collect();
        let grids = (0..total_cells).map(|slot| self.grid_slot_grid(slot, counts[slot].max(1))).collect();
        GridLayout { slot_of, rank_in_slot, cells, grids }
    }

    /// Every thumbnail's rectangle under the current config, in `names` order: the same decisions as the Windows painter's reposition_all_thumbnails, plus the size its resize pass would settle on.
    pub fn place_all(&self) -> Vec<Placement> {
        let cfg = self.cfg();
        let scale = self.screens.scale;
        let region_fit_active = is_region_fit_active(cfg);
        let grid_active = is_display_regions_active(cfg);
        let layout_managed = region_fit_active || grid_active;
        let not_logged_in_space = not_logged_in_space_rect_from_config(cfg);
        let total_count = self.names.len();

        let display_order = layout_managed.then(|| self.compute_region_fit_display_order(not_logged_in_space.is_some()));
        let grid_layout = grid_active.then(|| self.compute_grid_layout(display_order.as_ref()));
        let region_fit = (region_fit_active && !grid_active).then(|| {
            let region = region_rect_from_config(cfg).expect("checked by is_region_fit_active");
            let count = display_order.as_ref().map_or(total_count, |o| o.count);
            (region, calculate_region_fit_grid(region, count, cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region)))
        });
        let not_logged_in = not_logged_in_space.map(|space| (space, self.not_logged_in_space_grid(space, self.not_logged_in_space_count())));

        let mut out = Vec::with_capacity(total_count);
        for (index, name) in self.names.iter().enumerate() {
            let carved_out = not_logged_in_space.is_some() && is_generic_character_name(name);
            let placed = |pos: Position, grid: RegionFitGrid| Placement { x: pos.x, y: pos.y, width: grid.cell_width, height: grid.cell_height };

            if let Some(gl) = &grid_layout {
                if let (false, Some(slot)) = (carved_out, gl.slot_of[index]) {
                    let grid = gl.grids[slot];
                    let pos = region_fit_position_for_grid(gl.cells[slot], grid, gl.rank_in_slot[index], self.grid_slot_direction(slot), cfg.spacing);
                    out.push(placed(pos, grid));
                    continue;
                }
            }
            if let Some((region, grid)) = region_fit {
                if !carved_out {
                    let position_index = display_order.as_ref().map_or(index, |o| o.ranks[index]);
                    out.push(placed(region_fit_position_for_grid(region, grid, position_index, cfg.region_fit_direction, cfg.spacing), grid));
                    continue;
                }
            }
            if carved_out {
                let (space, grid) = not_logged_in.expect("carved out implies a space");
                out.push(placed(region_fit_position_for_grid(space, grid, self.not_logged_in_index(index), cfg.region_fit_direction, cfg.not_logged_in_space_spacing), grid));
                continue;
            }
            // Plain Custom mode.
            let (w, h) = self.thumbnail_size(name, total_count, None);
            let (w, h) = (scale_pixels(w, scale), scale_pixels(h, scale));
            let pos = self.calculate_thumbnail_position(name, w, h, index, total_count);
            out.push(Placement { x: pos.x, y: pos.y, width: w, height: h });
        }
        out
    }
}

/// A saved position that other thumbnails can snap to while one is dragged; identical rects are grouped ("stacked").
#[derive(Debug, Clone, PartialEq)]
pub struct GhostGroup {
    pub rect: Rect,
    pub names: String,
}

impl Layout<'_> {
    /// Saved positions for every other character in the profile, grouped by exact rect match.
    pub fn collect_ghost_groups(&self, exclude_character: &str) -> Vec<GhostGroup> {
        let cfg = self.cfg();
        let region_fit_grid = is_region_fit_active(cfg).then(|| {
            let region = region_rect_from_config(cfg).expect("checked by is_region_fit_active");
            calculate_region_fit_grid(region, self.region_fit_grid_count(), cfg.spacing, cfg.spacing, self.region_fit_aspect_ratio(), self.region_fit_max_cell_size(region))
        });
        let raw: Vec<(&str, Rect)> = self
            .config
            .characters
            .iter()
            .filter(|c| c.name != exclude_character)
            .filter_map(|c| {
                let pos = c.position?;
                let (w, h) = self.thumbnail_size(&c.name, self.names.len(), region_fit_grid);
                Some((c.name.as_str(), rect(pos.x, pos.y, pos.x + w, pos.y + h)))
            })
            .collect();

        let mut used = vec![false; raw.len()];
        let mut groups = Vec::new();
        for (i, &(name, r)) in raw.iter().enumerate() {
            if used[i] {
                continue;
            }
            used[i] = true;
            let mut names = name.to_owned();
            for (j, &(other_name, other_rect)) in raw.iter().enumerate().skip(i + 1) {
                if used[j] || r != other_rect {
                    continue;
                }
                used[j] = true;
                names.push_str(", ");
                names.push_str(other_name);
            }
            groups.push(GhostGroup { rect: r, names });
        }
        groups
    }
}

// ---------------------------------------------------------------------------------------------------- snapping --

/// Moves `snapped_x`/`snapped_y` toward the nearest edge of `other` if closer than the current best (`min_x_dist`/`min_y_dist`, seeded with the threshold).
#[allow(clippy::too_many_arguments)]
pub fn snap_axes_to_rect(snapped_x: &mut i32, snapped_y: &mut i32, width: i32, height: i32, other: Rect, min_x_dist: &mut i32, min_y_dist: &mut i32) {
    let snapped_right = *snapped_x + width;
    let snapped_bottom = *snapped_y + height;
    let orig_x = *snapped_x;
    let orig_y = *snapped_y;

    let v_overlap = !(snapped_bottom < other.top || *snapped_y > other.bottom);
    if v_overlap {
        for (dist, target) in [
            ((orig_x - other.left).abs(), other.left),
            ((orig_x - other.right).abs(), other.right),
            ((snapped_right - other.left).abs(), other.left - width),
            ((snapped_right - other.right).abs(), other.right - width),
        ] {
            if dist <= *min_x_dist {
                *min_x_dist = dist;
                *snapped_x = target;
            }
        }
    }

    let snapped_right_now = *snapped_x + width;
    let h_overlap = !(snapped_right_now < other.left || *snapped_x > other.right);
    if h_overlap {
        for (dist, target) in [
            ((orig_y - other.top).abs(), other.top),
            ((orig_y - other.bottom).abs(), other.bottom),
            ((snapped_bottom - other.top).abs(), other.top - height),
            ((snapped_bottom - other.bottom).abs(), other.bottom - height),
        ] {
            if dist <= *min_y_dist {
                *min_y_dist = dist;
                *snapped_y = target;
            }
        }
    }
}

pub fn screen_edge_snap(x: i32, y: i32, width: i32, height: i32, threshold: i32, bounds: Rect) -> (i32, i32) {
    let (mut sx, mut sy) = (x, y);
    if (x - bounds.left).abs() < threshold {
        sx = bounds.left;
    }
    if (x + width - bounds.right).abs() < threshold {
        sx = bounds.right - width;
    }
    if (y - bounds.top).abs() < threshold {
        sy = bounds.top;
    }
    if (y + height - bounds.bottom).abs() < threshold {
        sy = bounds.bottom - height;
    }
    (sx, sy)
}

pub fn thumbnail_edge_snap(x: i32, y: i32, width: i32, height: i32, threshold: i32, others: &[Rect]) -> (i32, i32) {
    let (mut sx, mut sy) = (x, y);
    let (mut mx, mut my) = (threshold, threshold);
    for &o in others {
        snap_axes_to_rect(&mut sx, &mut sy, width, height, o, &mut mx, &mut my);
    }
    (sx, sy)
}

/// Docks onto a ghost's exact saved position within `threshold` (Chebyshev distance), then edge-aligns against ghost rects.
pub fn ghost_snap(x: i32, y: i32, width: i32, height: i32, threshold: i32, groups: &[GhostGroup]) -> (i32, i32) {
    let (mut dock_x, mut dock_y) = (x, y);
    let mut best = threshold;
    for g in groups {
        let dist = (x - g.rect.left).abs().max((y - g.rect.top).abs());
        if dist <= best {
            best = dist;
            dock_x = g.rect.left;
            dock_y = g.rect.top;
        }
    }
    let (mut sx, mut sy) = (dock_x, dock_y);
    let (mut mx, mut my) = (threshold, threshold);
    for g in groups {
        snap_axes_to_rect(&mut sx, &mut sy, width, height, g.rect, &mut mx, &mut my);
    }
    (sx, sy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_with<'a>(config: &'a Config, names: &'a [String], membership: &'a Membership, scale_at: &'a dyn Fn(i32, i32) -> f32) -> Layout<'a> {
        Layout { config, names, membership, screens: Screens { monitor_bounds: None, virtual_screen: rect(0, 0, 1920, 1080), scale: 1.0, scale_at } }
    }

    #[test]
    fn region_fit_grid_grows_toward_bigger_cells() {
        let region = rect(0, 0, 1000, 500);
        let g = calculate_region_fit_grid(region, 4, 0, 0, 16.0 / 9.0, None);
        assert_eq!(g.columns * g.rows >= 4, true);
        assert!(g.cell_width > 0 && g.cell_height > 0);
        assert_eq!(calculate_region_fit_grid(region, 1, 0, 0, 2.0, None).cell_width, 1000);
    }

    #[test]
    fn directions_flip_inside_grid() {
        assert_eq!(region_fit_col_row(RegionFitDirection::RowFirst_RTL_TTB, 0, 3, 2), (2, 0));
        assert_eq!(region_fit_col_row(RegionFitDirection::ColumnFirst_BTT_LTR, 1, 3, 2), (0, 0));
    }

    #[test]
    fn unpositioned_thumbnails_flow_left_to_right() {
        let config = Config::defaults_with_profile("default.json").unwrap();
        let names = vec!["A".to_owned(), "B".to_owned()];
        let membership = Membership::new();
        let scale_at = |_, _| 1.0;
        let layout = layout_with(&config, &names, &membership, &scale_at);
        let placed = layout.place_all();
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].y, placed[1].y);
        assert!(placed[1].x > placed[0].x);
        assert_eq!(placed[0].width, config.thumbnail.width);
    }

    #[test]
    fn saved_position_wins_in_custom_mode() {
        let mut config = Config::defaults_with_profile("default.json").unwrap();
        config.get_or_create_character("A").position = Some(Position { x: 700, y: 300 });
        let names = vec!["A".to_owned()];
        let membership = Membership::new();
        let scale_at = |_, _| 1.0;
        let placed = layout_with(&config, &names, &membership, &scale_at).place_all();
        assert_eq!((placed[0].x, placed[0].y), (700, 300));
    }

    #[test]
    fn snapping_helpers() {
        assert_eq!(screen_edge_snap(5, 5, 100, 50, 10, rect(0, 0, 1000, 1000)), (0, 0));
        assert_eq!(thumbnail_edge_snap(205, 0, 100, 50, 10, &[rect(0, 0, 200, 50)]), (200, 0));
        let g = GhostGroup { rect: rect(400, 400, 500, 450), names: "X".into() };
        assert_eq!(ghost_snap(404, 397, 100, 50, 10, &[g]), (400, 400));
    }
}
