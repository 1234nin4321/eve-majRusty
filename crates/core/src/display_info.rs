//! Platform-neutral half of the config dialog's Display Config tab (the Windows side lives in eve_maj_app::displays): the
//! per-display record and its JSON shape, the GDI-name and mode-list maths, and the desktop bounding box.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// From a Win32 RECT's edges.
    pub fn from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { x: left, y: top, width: right - left, height: bottom - top }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Display {
    /// The N in Windows' \\.\DISPLAYN, used as the label here and on the Identify overlay.
    pub number: u32,
    /// Stable across reboots and re-plugging (the monitor's device path), so later per-display assignments can key on it; falls back to the GDI name.
    pub id: String,
    pub gdi_name: String,
    /// Monitor model from its EDID (e.g. "DELL U2720Q"), else Windows' generic description.
    pub name: String,
    pub gpu: Option<String>,
    pub primary: bool,
    /// Desktop position/size in physical pixels (the dialog is per-monitor DPI aware).
    pub bounds: Rect,
    /// Bounds minus the taskbar and other app bars.
    pub work_area: Rect,
    pub mode_width: u32,
    pub mode_height: u32,
    pub refresh_hz: u32,
    pub bits_per_pixel: u32,
    /// 0, 90, 180 or 270.
    pub orientation: u32,
    pub scale_percent: u32,
    /// Largest mode the display reports (usually its native resolution).
    pub max_width: u32,
    pub max_height: u32,
    /// Highest refresh rate it supports at the current resolution.
    pub max_refresh_hz: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplaysResponse<'a> {
    pub displays: &'a [Display],
    /// Bounding box of every display, for scaling the arrangement diagram.
    pub desktop: Rect,
}

/// The getDisplays response when enumeration or serialization fails.
pub const EMPTY_DISPLAYS_JSON: &str = "{\"displays\":[],\"desktop\":{\"x\":0,\"y\":0,\"width\":0,\"height\":0}}";

/// UTF-16 buffer up to its first NUL, as UTF-8; empty on bad input.
pub fn wide_to_utf8(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16(&wide[..len]).unwrap_or_default()
}

/// The trailing number of a GDI device name (\\.\DISPLAY12 -> 12), or 0 if there is none.
pub fn gdi_number(gdi_name: &str) -> u32 {
    let digits = gdi_name.len() - gdi_name.bytes().rev().take_while(u8::is_ascii_digit).count();
    gdi_name[digits..].parse().unwrap_or(0)
}

/// Display orientation in degrees from DEVMODE's dmDisplayOrientation (DMDO_DEFAULT..DMDO_270).
pub fn orientation_degrees(dm_display_orientation: u32) -> u32 {
    dm_display_orientation.min(3) * 90
}

/// Windows' display scaling percentage for an effective DPI.
pub fn scale_percent(dpi: u32) -> u32 {
    dpi * 100 / 96
}

/// Running maximum over a display's mode list: the largest resolution and the highest refresh rate at the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeScan {
    mode_width: u32,
    mode_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub max_refresh_hz: u32,
}

impl ModeScan {
    /// Starts from the current mode (`refresh_hz` is 0 when it is unknown).
    pub fn new(mode_width: u32, mode_height: u32, refresh_hz: u32) -> Self {
        Self { mode_width, mode_height, max_width: mode_width, max_height: mode_height, max_refresh_hz: refresh_hz }
    }

    pub fn consider(&mut self, width: u32, height: u32, refresh_hz: u32) {
        let portrait = self.mode_height > self.mode_width;
        // The mode list may be reported in the panel's native orientation, so turn each mode to match the current one before comparing.
        let (w, h) = if portrait == (height > width) { (width, height) } else { (height, width) };
        if u64::from(w) * u64::from(h) > u64::from(self.max_width) * u64::from(self.max_height) {
            self.max_width = w;
            self.max_height = h;
        }
        if w == self.mode_width && h == self.mode_height && refresh_hz > self.max_refresh_hz {
            self.max_refresh_hz = refresh_hz;
        }
    }
}

/// Bounding box of every display's bounds; all zero when there are none.
pub fn desktop_bounds(displays: &[Display]) -> Rect {
    if displays.is_empty() {
        return Rect::default();
    }
    let (mut left, mut top, mut right, mut bottom) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for d in displays {
        left = left.min(d.bounds.x);
        top = top.min(d.bounds.y);
        right = right.max(d.bounds.x + d.bounds.width);
        bottom = bottom.max(d.bounds.y + d.bounds.height);
    }
    Rect { x: left, y: top, width: right - left, height: bottom - top }
}

/// The getDisplays response JSON for `displays`.
pub fn displays_json(displays: &[Display]) -> String {
    serde_json::to_string(&DisplaysResponse { displays, desktop: desktop_bounds(displays) })
        .unwrap_or_else(|_| EMPTY_DISPLAYS_JSON.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(number: u32, bounds: Rect, gpu: Option<&str>) -> Display {
        Display {
            number,
            id: format!("\\\\?\\DISPLAY#DEL40F7#{number}"),
            gdi_name: format!("\\\\.\\DISPLAY{number}"),
            name: "DELL U2720Q".into(),
            gpu: gpu.map(Into::into),
            primary: number == 1,
            bounds,
            work_area: bounds,
            mode_width: bounds.width as u32,
            mode_height: bounds.height as u32,
            refresh_hz: 60,
            bits_per_pixel: 32,
            orientation: 0,
            scale_percent: 150,
            max_width: 3840,
            max_height: 2160,
            max_refresh_hz: 60,
        }
    }

    #[test]
    fn gdi_number_parses_the_trailing_display_number() {
        assert_eq!(gdi_number("\\\\.\\DISPLAY1"), 1);
        assert_eq!(gdi_number("\\\\.\\DISPLAY12"), 12);
        assert_eq!(gdi_number("weird"), 0);
        assert_eq!(gdi_number(""), 0);
    }

    #[test]
    fn wide_strings_stop_at_nul_and_reject_bad_utf16() {
        let mut buf = [0u16; 8];
        for (i, c) in "DISP".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wide_to_utf8(&buf), "DISP");
        assert_eq!(wide_to_utf8(&[0x41, 0x42]), "AB");
        assert_eq!(wide_to_utf8(&[0xD800, 0x41, 0]), "");
    }

    #[test]
    fn mode_scan_rotates_modes_to_the_current_orientation() {
        // Portrait 1440x2560 panel whose mode list is reported landscape.
        let mut scan = ModeScan::new(1440, 2560, 60);
        scan.consider(2560, 1440, 144);
        scan.consider(1920, 1080, 240);
        assert_eq!((scan.max_width, scan.max_height, scan.max_refresh_hz), (1440, 2560, 144));

        let mut scan = ModeScan::new(1920, 1080, 0);
        scan.consider(3840, 2160, 60);
        scan.consider(1920, 1080, 165);
        scan.consider(1920, 1080, 120);
        assert_eq!((scan.max_width, scan.max_height, scan.max_refresh_hz), (3840, 2160, 165));
    }

    #[test]
    fn orientation_and_scale() {
        assert_eq!(orientation_degrees(0), 0);
        assert_eq!(orientation_degrees(3), 270);
        assert_eq!(orientation_degrees(9), 270);
        assert_eq!(scale_percent(96), 100);
        assert_eq!(scale_percent(144), 150);
        assert_eq!(scale_percent(120), 125);
    }

    #[test]
    fn desktop_bounds_cover_every_display() {
        assert_eq!(desktop_bounds(&[]), Rect::default());
        let a = display(1, Rect { x: 0, y: 0, width: 2560, height: 1440 }, None);
        let b = display(2, Rect { x: -1920, y: 200, width: 1920, height: 1080 }, None);
        assert_eq!(desktop_bounds(&[a, b]), Rect { x: -1920, y: 0, width: 4480, height: 1440 });
    }

    #[test]
    fn json_shape_matches_the_dialog() {
        let d = display(1, Rect::from_edges(0, 0, 1920, 1080), None);
        let json: serde_json::Value = serde_json::from_str(&displays_json(&[d])).unwrap();
        let keys: Vec<&str> = json["displays"][0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "number", "id", "gdiName", "name", "gpu", "primary", "bounds", "workArea", "modeWidth", "modeHeight",
                "refreshHz", "bitsPerPixel", "orientation", "scalePercent", "maxWidth", "maxHeight", "maxRefreshHz",
            ]
        );
        assert!(json["displays"][0]["gpu"].is_null());
        assert_eq!(json["desktop"], serde_json::json!({"x": 0, "y": 0, "width": 1920, "height": 1080}));
        assert_eq!(displays_json(&[]), EMPTY_DISPLAYS_JSON);
    }
}
