//! Thumbnail interaction: click-to-activate (with shift-click exclusion), right-drag moving with screen-edge,
//! thumbnail-edge and ghost-position snapping, and the window procedures of the thumbnail and text-overlay windows.

use std::cell::Cell;

use eve_maj_core::log::Scope;
use eve_maj_core::types::{AnimationStyle, ClickTrigger, HoverCursor, NotificationType};
use eve_maj_win::input::{is_ctrl_pressed, is_shift_pressed};
use eve_maj_win::time::Ticks;
use eve_maj_win::window::{is_window, isize_to_hwnd};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::globals;
use crate::painter::{ActiveNotification, GhostGroup, Painter, ThumbnailWindow};

const SLOG: Scope = Scope::new("input");

/// Property name both overlay windows carry, pointing back at their EVE client window.
pub const SOURCE_HWND_PROP: &[u16] = &[b'S' as u16, b'O' as u16, b'U' as u16, b'R' as u16, b'C' as u16, b'E' as u16, b'_' as u16, b'H' as u16, b'W' as u16, b'N' as u16, b'D' as u16, 0];

/// Click state for mouse-up triggered clicks (left-click only; right-click uses DragState)
#[derive(Clone, Copy)]
struct ClickState {
    pending: bool,
    hwnd: HWND,
    source_hwnd: HWND,
    shift_pressed: bool,
}

const NO_CLICK: ClickState = ClickState { pending: false, hwnd: std::ptr::null_mut(), source_hwnd: std::ptr::null_mut(), shift_pressed: false };

#[derive(Clone, Copy)]
struct DragState {
    is_dragging: bool,
    hwnd: HWND,
    offset_x: i32,
    offset_y: i32,
}

const NO_DRAG: DragState = DragState { is_dragging: false, hwnd: std::ptr::null_mut(), offset_x: 0, offset_y: 0 };

thread_local! {
    static CLICK_STATE: Cell<ClickState> = const { Cell::new(NO_CLICK) };
    static DRAG_STATE: Cell<DragState> = const { Cell::new(NO_DRAG) };
    static ORIGINAL_ANIMATION_SETTING: Cell<Option<i32>> = const { Cell::new(None) };
}

/// Whether this thumbnail (by either its overlay or text-overlay hwnd) is the one currently being dragged.
pub fn is_thumbnail_dragging(thumbnail: &ThumbnailWindow) -> bool {
    let drag = DRAG_STATE.with(|d| d.get());
    drag.is_dragging && (drag.hwnd == thumbnail.hwnd || drag.hwnd == thumbnail.text_hwnd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapPosition {
    pub x: i32,
    pub y: i32,
}

fn animation_info() -> ANIMATIONINFO {
    ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: 0 }
}

/// Temporarily disable Windows minimize/restore animations
fn turn_off_animation() {
    let mut info = animation_info();
    unsafe {
        if SystemParametersInfoW(SPI_GETANIMATION, info.cbSize, (&mut info as *mut ANIMATIONINFO).cast(), 0) != 0 {
            if ORIGINAL_ANIMATION_SETTING.with(|o| o.get()).is_none() {
                ORIGINAL_ANIMATION_SETTING.with(|o| o.set(Some(info.iMinAnimate)));
            }
            if info.iMinAnimate != 0 {
                info.iMinAnimate = 0;
                SystemParametersInfoW(SPI_SETANIMATION, info.cbSize, (&mut info as *mut ANIMATIONINFO).cast(), 0);
            }
        }
    }
}

/// Restore Windows minimize/restore animations to original setting
fn restore_animation() {
    let Some(original) = ORIGINAL_ANIMATION_SETTING.with(|o| o.get()) else { return };
    let mut info = animation_info();
    unsafe {
        if SystemParametersInfoW(SPI_GETANIMATION, info.cbSize, (&mut info as *mut ANIMATIONINFO).cast(), 0) != 0
            && info.iMinAnimate != original
        {
            info.iMinAnimate = original;
            SystemParametersInfoW(SPI_SETANIMATION, info.cbSize, (&mut info as *mut ANIMATIONINFO).cast(), 0);
        }
    }
}

pub fn force_set_foreground_window(target_hwnd: HWND) {
    unsafe {
        SetForegroundWindow(target_hwnd);
        SetFocus(target_hwnd);
    }
}

/// Activates and focuses the EVE client window when its thumbnail is clicked, handling minimized/maximized states.
pub fn handle_thumbnail_click(source_hwnd: HWND) {
    handle_thumbnail_click_with_animation(source_hwnd, globals::config().interaction.animation_style);
}

fn handle_thumbnail_click_with_animation(source_hwnd: HWND, animation_style: AnimationStyle) {
    if !is_window(source_hwnd) {
        return;
    }

    // Get the current window placement to preserve maximized state
    let mut placement: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    if unsafe { GetWindowPlacement(source_hwnd, &mut placement) } == 0 {
        SLOG.err(format_args!("Failed to get window placement"));
        return;
    }
    let was_minimized = placement.showCmd == SW_SHOWMINIMIZED as u32;

    force_set_foreground_window(source_hwnd);

    // SW_RESTORE returns a maximized window to maximized, so no need to track was_maximized separately.
    if was_minimized {
        match animation_style {
            AnimationStyle::NoAnimation => {
                turn_off_animation();
                unsafe { ShowWindowAsync(source_hwnd, SW_RESTORE) };
                restore_animation();
            }
            AnimationStyle::OriginalAnimation => unsafe {
                ShowWindowAsync(source_hwnd, SW_RESTORE);
            },
        }
    }

    // Update thumbnail states immediately so the active border shows without waiting for the event hook.
    update_thumbnail_states_after_focus(source_hwnd);

    // Dismiss any active notification with suppress_when_clicked set; must run after state is reconciled above.
    if let Some(painter) = globals::painter() {
        if let Some(i) = painter.index_by_source(source_hwnd) {
            painter.thumbnails[i].last_click_time = Ticks::now();
            if painter.dismiss_click_suppressed_notifications(i) {
                painter.render_thumbnail_logged(i, "click-suppress clear");
                painter.thumbnails[i].needs_render = false;
            }
        }
    }

    update_hotkey_cycle_position(source_hwnd);
}

/// The source HWND an overlay window points back to, from its SOURCE_HWND property.
pub fn source_hwnd_of(hwnd: HWND) -> Option<HWND> {
    let h: HANDLE = unsafe { GetPropW(hwnd, SOURCE_HWND_PROP.as_ptr()) };
    (!h.is_null()).then_some(h as HWND)
}

/// Resolves the thumbnail under the cursor (its index in Painter::thumbnails), polled on demand since hotkey presses carry no SOURCE_HWND message.
pub fn resolve_thumbnail_under_cursor() -> Option<usize> {
    let painter = globals::painter()?;
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return None;
    }
    let hwnd_at_cursor = unsafe { WindowFromPoint(pt) };
    if hwnd_at_cursor.is_null() {
        return None;
    }
    painter.index_by_source(source_hwnd_of(hwnd_at_cursor)?)
}

/// Toggles character exclusion from hotkey cycling on Shift+Click, with visual feedback via a semi-transparent overlay.
pub fn handle_thumbnail_shift_click(source_hwnd: HWND) {
    let Some(painter) = globals::painter() else { return };
    let Some(hotkey_manager) = globals::hotkey_manager() else { return };
    let config = globals::config();

    if !config.exclusion.enable_shift_click_exclude {
        // Exclusion disabled: fall back to a plain click instead of swallowing the input
        handle_thumbnail_click(source_hwnd);
        return;
    }

    let Some(i) = painter.index_by_source(source_hwnd) else { return };
    let char_name = painter.thumbnails[i].character_name.clone();

    // Toggles exclusion in every group containing this character, or a group-independent list if it's in none.
    hotkey_manager.toggle_character_exclusion(&char_name);
    let excluded = hotkey_manager.is_character_excluded(&char_name);
    painter.thumbnails[i].is_excluded_from_cycle = excluded;

    if excluded && config.exclusion.auto_minimize_excluded {
        unsafe { ShowWindowAsync(source_hwnd, SW_FORCEMINIMIZE) };
    }

    let notification_text = if excluded { "Excluded" } else { "Included" };
    painter.push_notification(
        i,
        ActiveNotification {
            text: notification_text.to_owned(),
            notification_type: NotificationType::Generic,
            start_time: Ticks::now(),
            duration_ms: 3000,
            suppress_when_focused: false,
            suppress_when_clicked: false,
            border_color_override: None,
            text_color_override: None,
            show_border: false,
            flash_border: false,
        },
    );
    painter.render_thumbnail_logged(i, "exclusion toggle");
    SLOG.info(format_args!("Toggled cycle exclusion for {char_name}: {notification_text}"));
}

/// Lets cycling resume from a manually-selected character's position
fn update_hotkey_cycle_position(focused_hwnd: HWND) {
    let Some(painter) = globals::painter() else { return };
    let Some(hotkey_manager) = globals::hotkey_manager() else { return };
    if let Some(i) = painter.index_by_source(focused_hwnd) {
        let name = painter.thumbnails[i].character_name.clone();
        hotkey_manager.update_focused_character(&name, focused_hwnd);
    }
}

/// Updates thumbnail states immediately after focus change, since the Windows event hook may fire late.
fn update_thumbnail_states_after_focus(focused_hwnd: HWND) {
    let Some(painter) = globals::painter() else { return };

    // Bail if focus already changed, to avoid races during rapid cycling
    let current_foreground = unsafe { GetForegroundWindow() };
    if current_foreground != focused_hwnd {
        SLOG.debug(format_args!(
            "Skipping update_thumbnail_states_after_focus - focus already changed (target={focused_hwnd:?}, current={current_foreground:?})"
        ));
        return;
    }

    // Ensures only one thumbnail ends up active
    painter.reconcile_thumbnail_states(Some(focused_hwnd));

    // Rendering immediately avoids hotkey lag, but defers to the timer above a threshold to avoid blocking on rare bulk updates.
    const MAX_IMMEDIATE_RENDERS: usize = 4;
    painter.render_dirty_thumbnails(Some(MAX_IMMEDIATE_RENDERS));
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    ((lparam & 0xFFFF) as u16 as i16 as i32, ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// Start dragging a window (thumbnail or text overlay)
fn start_drag(hwnd: HWND, lparam: LPARAM) {
    if !globals::config().interaction.enable_dragging {
        return;
    }
    let (x, y) = lparam_point(lparam);
    DRAG_STATE.with(|d| d.set(DragState { is_dragging: true, hwnd, offset_x: x, offset_y: y }));

    if let Some(painter) = globals::painter() {
        if let Some(i) = painter.index_by_overlay(hwnd) {
            painter.render_thumbnail_logged(i, "drag start");
            if globals::config().snapping.show_ghost_position_borders {
                let name = painter.thumbnails[i].character_name.clone();
                painter.show_ghost_overlay(&name);
            }
        }
        painter.show_drag_hint_overlay(hwnd);
    }

    unsafe { SetCapture(hwnd) };
}

/// End dragging and save the thumbnail position
fn end_drag(hwnd: HWND, thumbnail_hwnd: HWND) {
    let drag = DRAG_STATE.with(|d| d.get());
    if !(drag.is_dragging && drag.hwnd == hwnd) {
        return;
    }
    // Cleared before rendering so effective_render_state sees the drag as already over.
    DRAG_STATE.with(|d| d.set(NO_DRAG));
    unsafe { ReleaseCapture() };

    let Some(painter) = globals::painter() else { return };
    if let Some(i) = painter.index_by_overlay(hwnd) {
        painter.render_thumbnail_logged(i, "drag end");
    }
    painter.hide_ghost_overlay();
    painter.hide_drag_hint_overlay();

    // Ctrl held during drag means all thumbnails moved together
    if is_ctrl_pressed() {
        let hwnds: Vec<HWND> = painter.thumbnails.iter().map(|t| t.hwnd).collect();
        for h in hwnds {
            painter.save_thumbnail_position(h);
        }
    } else {
        painter.save_thumbnail_position(thumbnail_hwnd);
    }
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetWindowRect(hwnd, &mut r) };
    r
}

/// Handles mouse move during drag; thumbnail and text-overlay windows are linked and moved together.
fn handle_drag(hwnd: HWND, lparam: LPARAM) {
    let drag = DRAG_STATE.with(|d| d.get());
    if !drag.is_dragging || drag.hwnd != hwnd {
        return;
    }

    if !is_window(hwnd) {
        SLOG.warn(format_args!("Window {hwnd:?} became invalid during drag operation, canceling drag"));
        DRAG_STATE.with(|d| d.set(NO_DRAG));
        unsafe { ReleaseCapture() };
        if let Some(painter) = globals::painter() {
            painter.hide_ghost_overlay();
            painter.hide_drag_hint_overlay();
        }
        return;
    }

    let (cursor_x, cursor_y) = lparam_point(lparam);
    let rect = window_rect(hwnd);
    let new_x = rect.left + cursor_x - drag.offset_x;
    let new_y = rect.top + cursor_y - drag.offset_y;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    if is_ctrl_pressed() {
        // Thumbnail-edge and ghost snapping don't apply to a group move; screen edges still do.
        let mut delta_x = new_x - rect.left;
        let mut delta_y = new_y - rect.top;
        let Some(painter) = globals::painter() else { return };
        let snapping = &globals::config().snapping;
        if snapping.enabled && snapping.screen_edges {
            let snapped = apply_screen_edge_snapping(new_x, new_y, width, height, snapping.threshold, hwnd);
            delta_x = snapped.x - rect.left;
            delta_y = snapped.y - rect.top;
        }

        let movable: Vec<(HWND, HWND)> = painter
            .thumbnails
            .iter()
            .filter(|t| t.win32_enabled && is_window(t.hwnd) && is_window(t.text_hwnd))
            .map(|t| (t.hwnd, t.text_hwnd))
            .collect();

        // Batched via DeferWindowPos so the whole group moves in one atomic DWM update instead of drifting apart across N sequential SetWindowPos calls.
        if !movable.is_empty() {
            unsafe {
                let mut hdwp = BeginDeferWindowPos(movable.len() as i32 * 2);
                for (thumb, text) in movable {
                    let r = window_rect(thumb);
                    let (x, y) = (r.left + delta_x, r.top + delta_y);
                    if !hdwp.is_null() {
                        hdwp = DeferWindowPos(hdwp, thumb, HWND_NOTOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                    if !hdwp.is_null() {
                        hdwp = DeferWindowPos(hdwp, text, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                }
                if !hdwp.is_null() {
                    EndDeferWindowPos(hdwp);
                }
            }
        }
    } else {
        // Apply snapping only when dragging single thumbnail
        let snapped = apply_snapping(new_x, new_y, width, height, hwnd);

        if let Some(other_hwnd) = linked_window(hwnd) {
            // Z-order is keyed by identity (text overlay always TOPMOST above thumbnail), not by which window was grabbed, or the live thumbnail could hide the name/border until refocus.
            let dragged = globals::painter().and_then(|p| p.index_by_overlay(hwnd).map(|i| (p.thumbnails[i].hwnd, p.thumbnails[i].text_hwnd)));
            let (thumb_hwnd, text_hwnd) = dragged.unwrap_or((hwnd, other_hwnd));
            unsafe {
                SetWindowPos(thumb_hwnd, HWND_NOTOPMOST, snapped.x, snapped.y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
                SetWindowPos(text_hwnd, HWND_TOPMOST, snapped.x, snapped.y, width, height, SWP_NOACTIVATE);
            }
        }
    }
}

/// Returns the linked window stored in GWLP_USERDATA, or None if none is valid.
fn linked_window(hwnd: HWND) -> Option<HWND> {
    let linked = isize_to_hwnd(unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) })?;
    if is_window(linked) {
        return Some(linked);
    }
    SLOG.debug(format_args!("Linked window handle {linked:?} is no longer valid"));
    None
}

fn apply_screen_edge_snapping(x: i32, y: i32, width: i32, height: i32, threshold: i32, dragging_hwnd: HWND) -> SnapPosition {
    let (mut snapped_x, mut snapped_y) = (x, y);
    let right = x + width;
    let bottom = y + height;
    let bounds = Painter::nearest_monitor_bounds(dragging_hwnd).bounds;

    if (x - bounds.left).abs() < threshold {
        snapped_x = bounds.left;
    }
    if (right - bounds.right).abs() < threshold {
        snapped_x = bounds.right - width;
    }
    if (y - bounds.top).abs() < threshold {
        snapped_y = bounds.top;
    }
    if (bottom - bounds.bottom).abs() < threshold {
        snapped_y = bounds.bottom - height;
    }
    SnapPosition { x: snapped_x, y: snapped_y }
}

/// Updates `snapped_x`/`snapped_y` toward the nearest edge of `other` if closer than the current best (`min_x_dist`/`min_y_dist`), which callers seed with the threshold. Shared by live-thumbnail and ghost-position edge snapping.
#[allow(clippy::too_many_arguments)]
fn snap_axes_to_rect(snapped_x: &mut i32, snapped_y: &mut i32, width: i32, height: i32, other: RECT, min_x_dist: &mut i32, min_y_dist: &mut i32) {
    // Recalculated per call, since snapped_x/y may have changed on a prior call in the same loop.
    let snapped_right = *snapped_x + width;
    let snapped_bottom = *snapped_y + height;
    // Snapshot the pre-update position so all four candidates measure from the actual window, not one already overwritten this call.
    let orig_x = *snapped_x;
    let orig_y = *snapped_y;

    // Check vertical alignment (for horizontal snapping)
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

    // Recompute right edge for vertical-snap check: horizontal snapping above may have moved snapped_x.
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

fn apply_thumbnail_edge_snapping(x: i32, y: i32, width: i32, height: i32, threshold: i32, dragging_hwnd: HWND, painter: &Painter) -> SnapPosition {
    let (mut snapped_x, mut snapped_y) = (x, y);
    let (mut min_x_dist, mut min_y_dist) = (threshold, threshold);

    for t in &painter.thumbnails {
        if t.hwnd == dragging_hwnd || t.text_hwnd == dragging_hwnd || !is_window(t.hwnd) {
            continue;
        }
        snap_axes_to_rect(&mut snapped_x, &mut snapped_y, width, height, window_rect(t.hwnd), &mut min_x_dist, &mut min_y_dist);
    }
    SnapPosition { x: snapped_x, y: snapped_y }
}

/// Snaps to a ghost's exact saved position when within `threshold` px (Chebyshev distance), else aligns edges against ghost rects like apply_thumbnail_edge_snapping does for live thumbnails.
fn apply_ghost_snapping(x: i32, y: i32, width: i32, height: i32, threshold: i32, dragging_hwnd: HWND, painter: &mut Painter) -> SnapPosition {
    // Non-thumbnail draggers (e.g. the notification history panel) own no character, so nothing is excluded from the ghost set.
    let character_name = painter.index_by_overlay(dragging_hwnd).map(|i| painter.thumbnails[i].character_name.clone()).unwrap_or_default();

    // show_ghost_overlay (called at drag-start) already computed and cached this for the duration of the drag - reuse
    // it instead of recomputing on every mouse move. Falls back to a one-off computation for callers that snap without
    // showing the ghost overlay first (the list panel's drag never shows it); the fallback is deliberately not written
    // back into the cache, since nothing would invalidate it afterward for that flow.
    let fallback;
    let groups: &[GhostGroup] = match &painter.current_drag_ghost_groups {
        Some(groups) => groups,
        None => {
            fallback = painter.collect_ghost_groups(&character_name);
            &fallback
        }
    };

    let (mut dock_x, mut dock_y) = (x, y);
    let mut best_dist = threshold;
    for g in groups {
        let dist = (x - g.rect.left).abs().max((y - g.rect.top).abs());
        if dist <= best_dist {
            best_dist = dist;
            dock_x = g.rect.left;
            dock_y = g.rect.top;
        }
    }

    let (mut snapped_x, mut snapped_y) = (dock_x, dock_y);
    let (mut min_x_dist, mut min_y_dist) = (threshold, threshold);
    for g in groups {
        snap_axes_to_rect(&mut snapped_x, &mut snapped_y, width, height, g.rect, &mut min_x_dist, &mut min_y_dist);
    }
    SnapPosition { x: snapped_x, y: snapped_y }
}

/// Applies screen-edge, thumbnail-edge, and saved-ghost-position snapping to a dragged window's position
pub fn apply_snapping(x: i32, y: i32, width: i32, height: i32, dragging_hwnd: HWND) -> SnapPosition {
    let Some(painter) = globals::painter() else { return SnapPosition { x, y } };
    let snapping = &globals::config().snapping;
    if !snapping.enabled {
        return SnapPosition { x, y };
    }

    let threshold = snapping.threshold;
    let mut result = SnapPosition { x, y };
    if snapping.screen_edges {
        result = apply_screen_edge_snapping(result.x, result.y, width, height, threshold, dragging_hwnd);
    }
    // Chains off the screen-snapped result so both snaps compose.
    if snapping.thumbnail_edges {
        result = apply_thumbnail_edge_snapping(result.x, result.y, width, height, threshold, dragging_hwnd, painter);
    }
    if snapping.ghost_positions {
        result = apply_ghost_snapping(result.x, result.y, width, height, threshold, dragging_hwnd, painter);
    }
    result
}

pub const HIDE_DEBOUNCE_TIMER_ID: usize = 1;

/// Shared WM_LBUTTONDOWN handling for both the thumbnail and text overlay window procs.
fn handle_overlay_lbutton_down(hwnd: HWND) {
    let Some(source_hwnd) = source_hwnd_of(hwnd) else { return };
    let shift_pressed = is_shift_pressed();

    if globals::config().interaction.click_trigger == ClickTrigger::MouseDown {
        if shift_pressed {
            handle_thumbnail_shift_click(source_hwnd);
        } else {
            handle_thumbnail_click(source_hwnd);
        }
    } else {
        CLICK_STATE.with(|c| c.set(ClickState { pending: true, hwnd, source_hwnd, shift_pressed }));
    }
}

/// Shared WM_LBUTTONUP handling for both the thumbnail and text overlay window procs.
fn handle_overlay_lbutton_up(hwnd: HWND) {
    let click = CLICK_STATE.with(|c| c.replace(NO_CLICK));
    if globals::config().interaction.click_trigger == ClickTrigger::MouseUp && click.pending && click.hwnd == hwnd && !click.source_hwnd.is_null() {
        if click.shift_pressed {
            handle_thumbnail_shift_click(click.source_hwnd);
        } else {
            handle_thumbnail_click(click.source_hwnd);
        }
    }
}

/// Shared WM_SETCURSOR handling for both the thumbnail and text overlay window procs; returns whether it set the cursor.
fn apply_hover_cursor() -> bool {
    let resource = match globals::config().interaction.hover_cursor {
        HoverCursor::Default => return false,
        HoverCursor::Hand => IDC_HAND,
        HoverCursor::Crosshair => IDC_CROSS,
        HoverCursor::Move => IDC_SIZEALL,
        HoverCursor::Help => IDC_HELP,
    };
    // SetCursor(null) hides the cursor, so a failed load must fall back to the class cursor.
    let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), resource) };
    if cursor.is_null() {
        return false;
    }
    unsafe { SetCursor(cursor) };
    true
}

/// Window procedure for thumbnail windows: handles input events and the auto-hide timer when no EVE window has focus.
pub unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            if wparam == HIDE_DEBOUNCE_TIMER_ID {
                if let Some(painter) = globals::painter() {
                    KillTimer(hwnd, HIDE_DEBOUNCE_TIMER_ID);
                    painter.hide_debounce_timer_hwnd = None;
                    SLOG.debug(format_args!("Hide debounce timer fired, hiding all thumbnails"));
                    painter.hide_all_automatically();
                }
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            handle_overlay_lbutton_down(hwnd);
            0
        }
        WM_LBUTTONUP => {
            handle_overlay_lbutton_up(hwnd);
            0
        }
        WM_RBUTTONDOWN => {
            start_drag(hwnd, lparam);
            0
        }
        WM_RBUTTONUP => {
            end_drag(hwnd, hwnd);
            0
        }
        WM_MOUSEMOVE => {
            handle_drag(hwnd, lparam);
            0
        }
        WM_SETCURSOR => {
            if apply_hover_cursor() {
                return 1;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ACTIVATE => {
            if let Some(text_hwnd) = linked_window(hwnd) {
                SetWindowPos(text_hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DPICHANGED => {
            // Position only; resize_thumbnail_if_needed below re-derives size from our own scale formula.
            let suggested = &*(lparam as *const RECT);
            SetWindowPos(hwnd, HWND_NOTOPMOST, suggested.left, suggested.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);

            if let Some(painter) = globals::painter() {
                if let Some(i) = painter.index_by_overlay(hwnd) {
                    painter.resize_thumbnail_if_needed(i, None);
                    if let Some(text_hwnd) = linked_window(hwnd) {
                        let r = window_rect(hwnd);
                        SetWindowPos(text_hwnd, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE);
                    }
                    painter.render_thumbnail_logged(i, "DPI change");
                }
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Window procedure for text overlay windows: handles clicks and dragging
pub unsafe extern "system" fn text_window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            handle_overlay_lbutton_down(hwnd);
            0
        }
        WM_LBUTTONUP => {
            handle_overlay_lbutton_up(hwnd);
            0
        }
        WM_RBUTTONDOWN => {
            start_drag(hwnd, lparam);
            0
        }
        WM_RBUTTONUP => {
            if let Some(thumb_hwnd) = linked_window(hwnd) {
                end_drag(hwnd, thumb_hwnd);
            }
            0
        }
        WM_MOUSEMOVE => {
            handle_drag(hwnd, lparam);
            0
        }
        WM_SETCURSOR => {
            if apply_hover_cursor() {
                return 1;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
