//! Display Regions: splits the Thumbnail Space region (normally a full display) into a columns x rows grid of cells, each holding only the thumbnails its rule claims. Platform-neutral (no Win32), so the routing rule and cell maths unit-test anywhere; the painter does the actual placement.

use serde::{Deserialize, Serialize};

/// Largest columns/rows count a custom split allows (8x8 = 64 cells); the dialog's presets stop at 4x4.
pub const MAX_DIM: u8 = 8;
pub const MAX_CELLS: usize = MAX_DIM as usize * MAX_DIM as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SlotKind {
    /// Holds nothing - unless it has a fill_order, which makes it take the leftovers (see Slot::fill_order).
    #[default]
    Empty,
    /// Every thumbnail no other cell claims.
    EveryoneElse,
    /// The characters listed in `characters`.
    Custom,
    /// A single character (`characters[0]`) filling the whole cell.
    Client,
    /// Every character linked to Account Config account `account`.
    Account,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Slot {
    pub kind: SlotKind,
    /// Account Config account id, for Account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Character names, for Custom (any number) and Client (the first is used).
    pub characters: Vec<String>,
    /// Empty only: a Region Fit direction name (e.g. "RowFirst_LTR_TTB"). When set, the region takes the characters no other region claims - same tier as Everyone Else - and arranges them in this order instead of the global one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_order: Option<String>,
}

impl Slot {
    /// Whether this slot catches characters nothing more specific claimed.
    pub fn takes_leftovers(&self) -> bool {
        self.kind == SlotKind::EveryoneElse || (self.kind == SlotKind::Empty && self.fill_order.is_some())
    }

    fn claims(&self, name: &str, account_of: Option<&str>) -> bool {
        match self.kind {
            SlotKind::Empty => self.fill_order.is_some(),
            SlotKind::EveryoneElse => true,
            SlotKind::Client => self.characters.first().is_some_and(|c| c.eq_ignore_ascii_case(name)),
            SlotKind::Custom => self.characters.iter().any(|c| c.eq_ignore_ascii_case(name)),
            SlotKind::Account => matches!((account_of, &self.account), (Some(a), Some(b)) if a == b),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisplayGrid {
    /// Square NxN split (0 = off). Superseded by columns/rows when both are set; kept so v1.3.0 profiles still load.
    pub size: u8,
    /// Custom columns x rows split; both must be non-zero to take effect.
    pub columns: u8,
    pub rows: u8,
    /// Stretch thumbnails to fill their cell exactly (a 2x2 grid on 2560x1440 gives 1280x720 thumbnails) instead of keeping the configured thumbnail shape inside it.
    pub fit_to_grid: bool,
    /// Row-major, one per cell; missing trailing cells count as Empty.
    pub slots: Vec<Slot>,
}

impl DisplayGrid {
    pub fn column_count(&self) -> u8 {
        if self.columns > 0 && self.rows > 0 { self.columns } else { self.size }
    }

    pub fn row_count(&self) -> u8 {
        if self.columns > 0 && self.rows > 0 { self.rows } else { self.size }
    }

    pub fn is_active(&self) -> bool {
        let (c, r) = (self.column_count(), self.row_count());
        c >= 1 && r >= 1 && c <= MAX_DIM && r <= MAX_DIM
    }

    pub fn cell_count(&self) -> usize {
        if self.is_active() { self.column_count() as usize * self.row_count() as usize } else { 0 }
    }

    /// Drops slots past MAX_CELLS; the Zig build did this while copying a grid out of its parse arena.
    pub fn truncate_slots(&mut self) {
        self.slots.truncate(MAX_CELLS);
    }

    /// For the config dialog's live-preview patch, which arrives as a loose JSON value rather than a whole profile.
    pub fn from_json_value(value: &serde_json::Value) -> serde_json::Result<Self> {
        let mut grid = Self::deserialize(value)?;
        grid.truncate_slots();
        Ok(grid)
    }

    pub fn validate(&mut self) {
        self.size = self.size.min(MAX_DIM);
        self.columns = self.columns.min(MAX_DIM);
        self.rows = self.rows.min(MAX_DIM);
    }

    fn active_slots(&self) -> &[Slot] {
        &self.slots[..self.cell_count().min(self.slots.len())]
    }
}

/// Which cell claims `name`, or None if none does (it then keeps its normal saved/spawn position). Priority: Client, then Custom, then Account, then Everyone Else, so a character listed explicitly always wins over an account-wide rule. `account_of` is the character's Account Config account id, if linked.
pub fn assign_slot(grid: &DisplayGrid, name: &str, account_of: Option<&str>) -> Option<usize> {
    let slots = grid.active_slots();
    let find = |pred: &dyn Fn(&Slot) -> bool| slots.iter().position(pred);

    find(&|s| s.kind == SlotKind::Client && s.claims(name, account_of))
        .or_else(|| find(&|s| s.kind == SlotKind::Custom && s.claims(name, account_of)))
        .or_else(|| account_of.and_then(|_| find(&|s| s.kind == SlotKind::Account && s.claims(name, account_of))))
        .or_else(|| find(&|s| s.takes_leftovers()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Most displays that can each carry their own grid.
pub const MAX_LAYOUTS: usize = 8;

/// One display's grid: the physical-pixel area it splits (the display's bounds or work area, captured when it was set up) plus its cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisplayLayout {
    /// The display's stable id (its monitor device path, see displays), so the dialog can match a layout back to its display.
    pub display_id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Whether x/y/width/height came from the work area (taskbar excluded) rather than the full bounds; only the dialog reads this.
    pub use_work_area: bool,
    pub grid: DisplayGrid,
}

impl Default for DisplayLayout {
    fn default() -> Self {
        Self { display_id: String::new(), x: 0, y: 0, width: 0, height: 0, use_work_area: true, grid: DisplayGrid::default() }
    }
}

impl DisplayLayout {
    pub fn rect(&self) -> Rect {
        Rect { left: self.x, top: self.y, right: self.x + self.width, bottom: self.y + self.height }
    }

    pub fn is_active(&self) -> bool {
        self.width > 0 && self.height > 0 && self.grid.is_active()
    }
}

/// Keeps at most MAX_LAYOUTS layouts and MAX_CELLS slots per grid, as the Zig build's clone step did.
pub fn truncate_layouts(layouts: &mut Vec<DisplayLayout>) {
    layouts.truncate(MAX_LAYOUTS);
    for layout in layouts.iter_mut() {
        layout.grid.truncate_slots();
    }
}

/// Live-preview counterpart of a profile load, from a loose JSON array.
pub fn layouts_from_json_value(value: &serde_json::Value) -> serde_json::Result<Vec<DisplayLayout>> {
    let mut layouts = Vec::<DisplayLayout>::deserialize(value)?;
    truncate_layouts(&mut layouts);
    Ok(layouts)
}

/// A grid placed on screen: what the painter routes thumbnails through. Cells of every view share one global index space, in view order.
#[derive(Debug, Clone, Copy)]
pub struct LayoutView<'a> {
    pub rect: Rect,
    pub grid: &'a DisplayGrid,
}

pub fn total_cells(views: &[LayoutView<'_>]) -> usize {
    views.iter().map(|v| v.grid.cell_count()).sum()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRef {
    pub view: usize,
    pub local: usize,
}

/// Which view (and which of its cells) global cell `global` is.
pub fn locate(views: &[LayoutView<'_>], global: usize) -> Option<CellRef> {
    let mut base = 0;
    for (i, v) in views.iter().enumerate() {
        let count = v.grid.cell_count();
        if global < base + count {
            return Some(CellRef { view: i, local: global - base });
        }
        base += count;
    }
    None
}

/// assign_slot across every view: one priority pass over all displays' cells (Client, then Custom, then Account, then Everyone Else), returning a global cell index.
pub fn assign_slot_multi(views: &[LayoutView<'_>], name: &str, account_of: Option<&str>) -> Option<usize> {
    const PASSES: [SlotKind; 4] = [SlotKind::Client, SlotKind::Custom, SlotKind::Account, SlotKind::EveryoneElse];
    for kind in PASSES {
        let mut base = 0;
        for v in views {
            for (i, slot) in v.grid.active_slots().iter().enumerate() {
                // Leftover-taking Empty regions share Everyone Else's tier, so the first catch-all in grid order wins.
                let in_pass = slot.kind == kind || (kind == SlotKind::EveryoneElse && slot.kind == SlotKind::Empty);
                if in_pass && slot.claims(name, account_of) {
                    return Some(base + i);
                }
            }
            base += v.grid.cell_count();
        }
    }
    None
}

/// The fill order a global cell arranges its characters in, when it overrides the global one (leftover-taking Empty regions only).
pub fn slot_fill_order<'a>(views: &[LayoutView<'a>], global: usize) -> Option<&'a str> {
    let r = locate(views, global)?;
    let grid = views[r.view].grid;
    let slot = grid.slots.get(r.local)?;
    if slot.kind == SlotKind::Empty { slot.fill_order.as_deref() } else { None }
}

/// Screen rect of global cell `global`.
pub fn cell_rect_multi(views: &[LayoutView<'_>], global: usize, gap: i32) -> Option<Rect> {
    let r = locate(views, global)?;
    let v = views[r.view];
    Some(cell_rect(v.rect, v.grid.column_count(), v.grid.row_count(), r.local, gap))
}

/// Cell `index` (row-major) of `region` split `columns` x `rows`, with `gap` px between cells; the last row/column absorbs rounding so cells tile the region exactly.
pub fn cell_rect(region: Rect, columns: u8, rows: u8, index: usize, gap: i32) -> Rect {
    let nc = columns.max(1) as i32;
    let nr = rows.max(1) as i32;
    let col = (index % nc as usize) as i32;
    let row = (index / nc as usize) as i32;
    let width = region.right - region.left;
    let height = region.bottom - region.top;
    let cell_w = (width - gap * (nc - 1)) / nc;
    let cell_h = (height - gap * (nr - 1)) / nr;
    let left = region.left + col * (cell_w + gap);
    let top = region.top + row * (cell_h + gap);
    Rect {
        left,
        top,
        right: if col == nc - 1 { region.right } else { left + cell_w },
        bottom: if row == nr - 1 { region.bottom } else { top + cell_h },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillGrid {
    pub columns: u32,
    pub rows: u32,
    pub tile_width: i32,
    pub tile_height: i32,
}

/// Fit to Grid: tiles `count` thumbnails across `cell` with no leftover space, choosing the columns x rows split whose tile shape is closest to `aspect` (the configured thumbnail width/height). One thumbnail fills the whole cell.
pub fn fill_grid(cell: Rect, count: usize, spacing: i32, aspect: f32) -> FillGrid {
    let width = cell.right - cell.left;
    let height = cell.bottom - cell.top;
    let n = count.max(1) as u32;
    let mut best = FillGrid { columns: 1, rows: n, tile_width: width, tile_height: height };
    let mut best_score = f32::MAX;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        // Skip splits that leave a whole empty row.
        if (rows - 1) * cols >= n {
            continue;
        }
        let tw = (width - spacing * (cols as i32 - 1)) / cols as i32;
        let th = (height - spacing * (rows as i32 - 1)) / rows as i32;
        if tw <= 0 || th <= 0 {
            continue;
        }
        let tile_aspect = tw as f32 / th as f32;
        let score = (tile_aspect / aspect.max(0.01)).ln().abs();
        if score < best_score {
            best_score = score;
            best = FillGrid { columns: cols, rows, tile_width: tw, tile_height: th };
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(kind: SlotKind) -> Slot {
        Slot { kind, ..Default::default() }
    }
    fn chars(kind: SlotKind, names: &[&str]) -> Slot {
        Slot { kind, characters: names.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }
    fn account(id: &str) -> Slot {
        Slot { kind: SlotKind::Account, account: Some(id.into()), ..Default::default() }
    }
    fn fill(order: &str) -> Slot {
        Slot { kind: SlotKind::Empty, fill_order: Some(order.into()), ..Default::default() }
    }
    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    #[test]
    fn fill_grid_one_fills_several_split() {
        let cell = rect(0, 0, 1280, 720);
        assert_eq!(fill_grid(cell, 1, 0, 16.0 / 9.0), FillGrid { columns: 1, rows: 1, tile_width: 1280, tile_height: 720 });
        assert_eq!(fill_grid(cell, 4, 0, 16.0 / 9.0), FillGrid { columns: 2, rows: 2, tile_width: 640, tile_height: 360 });
        let two = fill_grid(cell, 2, 10, 16.0 / 9.0);
        assert_eq!(two.columns * two.rows, 2);
        let tall = fill_grid(rect(0, 0, 1080, 1920), 3, 0, 16.0 / 9.0);
        assert_eq!(tall.columns, 1);
        assert_eq!(tall.tile_height, 640);
    }

    #[test]
    fn assign_slot_priority() {
        let grid = DisplayGrid {
            size: 2,
            slots: vec![
                slot(SlotKind::EveryoneElse),
                account("acc_main"),
                chars(SlotKind::Custom, &["Alt Two", "Miner Three"]),
                chars(SlotKind::Client, &["FC Zoetrope"]),
            ],
            ..Default::default()
        };
        assert_eq!(assign_slot(&grid, "fc zoetrope", Some("acc_main")), Some(3));
        assert_eq!(assign_slot(&grid, "Miner Three", Some("acc_main")), Some(2));
        assert_eq!(assign_slot(&grid, "Someone", Some("acc_main")), Some(1));
        assert_eq!(assign_slot(&grid, "Someone", None), Some(0));
        assert_eq!(assign_slot(&grid, "Someone", Some("acc_other")), Some(0));
    }

    #[test]
    fn assign_slot_ignores_slots_beyond_grid() {
        let grid = DisplayGrid { size: 1, slots: vec![chars(SlotKind::Custom, &["A"]), slot(SlotKind::EveryoneElse)], ..Default::default() };
        assert_eq!(assign_slot(&grid, "A", None), Some(0));
        assert_eq!(assign_slot(&grid, "B", None), None);
        assert_eq!(assign_slot(&DisplayGrid::default(), "A", None), None);
    }

    #[test]
    fn cell_rect_tiles_exactly() {
        let region = rect(0, 0, 2560, 1440);
        assert_eq!(cell_rect(region, 2, 2, 0, 10), rect(0, 0, 1275, 715));
        assert_eq!(cell_rect(region, 2, 2, 3, 10), rect(1285, 725, 2560, 1440));
        assert_eq!(cell_rect(rect(-1080, -240, 0, 1680), 1, 1, 0, 10), rect(-1080, -240, 0, 1680));
        assert_eq!(cell_rect(rect(0, 0, 1000, 999), 3, 3, 8, 0).right, 1000);
    }

    #[test]
    fn custom_columns_rows() {
        let grid = DisplayGrid { size: 2, columns: 3, rows: 2, ..Default::default() };
        assert_eq!(grid.cell_count(), 6);
        assert_eq!(grid.column_count(), 3);
        assert_eq!(DisplayGrid { size: 4, ..Default::default() }.cell_count(), 16);
        assert_eq!(DisplayGrid { columns: 3, ..Default::default() }.cell_count(), 0);
        assert!(!DisplayGrid { columns: 9, rows: 1, ..Default::default() }.is_active());
        let region = rect(0, 0, 3000, 1000);
        assert_eq!(cell_rect(region, 3, 2, 4, 0), rect(1000, 500, 2000, 1000));
        assert_eq!(cell_rect(region, 3, 2, 2, 0), rect(2000, 0, 3000, 500));
    }

    #[test]
    fn multi_display_priority_and_indices() {
        let d1 = DisplayGrid {
            size: 2,
            slots: vec![slot(SlotKind::EveryoneElse), slot(SlotKind::Empty), slot(SlotKind::Empty), chars(SlotKind::Custom, &["Miner Three"])],
            ..Default::default()
        };
        let d2 = DisplayGrid {
            columns: 3,
            rows: 1,
            slots: vec![account("acc_alts"), chars(SlotKind::Client, &["FC Zoetrope"]), slot(SlotKind::Empty)],
            ..Default::default()
        };
        let views = [
            LayoutView { rect: rect(0, 0, 2560, 1440), grid: &d1 },
            LayoutView { rect: rect(2560, 180, 4480, 1260), grid: &d2 },
        ];
        assert_eq!(total_cells(&views), 7);
        assert_eq!(assign_slot_multi(&views, "FC Zoetrope", Some("acc_main")), Some(5));
        assert_eq!(assign_slot_multi(&views, "Miner Three", Some("acc_alts")), Some(3));
        assert_eq!(assign_slot_multi(&views, "Alt Two", Some("acc_alts")), Some(4));
        assert_eq!(assign_slot_multi(&views, "Nobody", None), Some(0));
        assert_eq!(locate(&views, 5), Some(CellRef { view: 1, local: 1 }));
        assert_eq!(cell_rect_multi(&views, 5, 0), Some(rect(3200, 180, 3840, 1260)));
        assert_eq!(locate(&views, 7), None);
        assert_eq!(assign_slot_multi(&views[1..], "Nobody", None), None);
    }

    #[test]
    fn empty_regions_with_fill_order_take_leftovers() {
        let g1 = DisplayGrid { columns: 2, rows: 1, slots: vec![slot(SlotKind::Empty), chars(SlotKind::Custom, &["A"])], ..Default::default() };
        let v1 = [LayoutView { rect: rect(0, 0, 100, 100), grid: &g1 }];
        assert_eq!(assign_slot_multi(&v1, "B", None), None);

        let g2 = DisplayGrid {
            columns: 3,
            rows: 1,
            slots: vec![fill("ColumnFirst_TTB_LTR"), slot(SlotKind::EveryoneElse), chars(SlotKind::Custom, &["A"])],
            ..Default::default()
        };
        let v2 = [LayoutView { rect: rect(0, 0, 300, 100), grid: &g2 }];
        assert_eq!(assign_slot_multi(&v2, "B", None), Some(0));
        assert_eq!(assign_slot_multi(&v2, "A", None), Some(2));
        assert_eq!(slot_fill_order(&v2, 0), Some("ColumnFirst_TTB_LTR"));
        assert_eq!(slot_fill_order(&v2, 1), None);

        let g3 = DisplayGrid { columns: 2, rows: 1, slots: vec![slot(SlotKind::EveryoneElse), fill("RowFirst_RTL_TTB")], ..Default::default() };
        let v3 = [LayoutView { rect: rect(0, 0, 200, 100), grid: &g3 }];
        assert_eq!(assign_slot_multi(&v3, "B", None), Some(0));
    }

    #[test]
    fn layouts_parse() {
        let json = r#"[{"displayId":"\\\\?\\DISPLAY#DEL","x":0,"y":0,"width":2560,"height":1392,"grid":{"columns":2,"rows":2,"fitToGrid":true,"slots":[{"kind":"EveryoneElse"},{"kind":"Empty","fillOrder":"RowFirst_LTR_BTT"}]}},
            {"displayId":"\\\\.\\DISPLAY3","x":2560,"y":180,"width":1920,"height":1080,"useWorkArea":false,"grid":{"size":1,"slots":[{"kind":"Account","account":"acc_1"}]}}]"#;
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let layouts = layouts_from_json_value(&value).unwrap();
        assert_eq!(layouts.len(), 2);
        assert!(layouts[0].grid.fit_to_grid);
        assert!(layouts[0].use_work_area);
        assert!(!layouts[1].use_work_area);
        assert!(layouts[1].is_active());
        assert_eq!(layouts[1].grid.slots[0].account.as_deref(), Some("acc_1"));
        assert_eq!(layouts[0].grid.slots[1].fill_order.as_deref(), Some("RowFirst_LTR_BTT"));
    }

    #[test]
    fn grid_from_json_ignores_unknown_and_serializes_kind() {
        let json = r#"{"size":3,"columns":0,"rows":0,"slots":[{"kind":"Client","characters":["FC Zoetrope"]},{"kind":"Account","account":"acc_1"},{"kind":"EveryoneElse"},{"kind":"Empty","extra":1}]}"#;
        let grid = DisplayGrid::from_json_value(&serde_json::from_str(json).unwrap()).unwrap();
        assert_eq!(grid.size, 3);
        assert_eq!(grid.slots.len(), 4);
        assert_eq!(grid.slots[0].characters[0], "FC Zoetrope");
        let out = serde_json::to_string(&grid).unwrap();
        assert!(out.contains(r#""kind":"Client""#));
        assert!(!out.contains("fillOrder"), "null optionals must be omitted: {out}");
    }
}
