//! [`WindowManager`] for Hyprland, over its IPC sockets: `.socket.sock` for queries and dispatchers, `.socket2.sock`
//! for the event stream that tells us when to rescan.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use eve_maj_core::display_grid::Rect;
use eve_maj_core::log::Scope;
use serde_json::Value;

use super::backend::{KeyBind, Monitor, Toplevel, WindowId, WindowManager, WmEvent};

const SLOG: Scope = Scope::new("hyprland");
/// Prefix of our custom events and Lua globals, so our binds never collide with the user's.
const EVENT_PREFIX: &str = "eve-maj:";
/// Where minimized clients are parked.
const MINIMIZE_WORKSPACE: &str = "special:evemaj";

pub struct HyprlandWm {
    request_path: PathBuf,
    events: Option<UnixStream>,
    event_buf: String,
    /// Hyprland 0.57+ with a Lua config takes `hl.dsp.*` dispatchers instead of the classic `name args` form; learned on first use.
    lua_dispatch: std::cell::Cell<Option<bool>>,
    /// Binds registered with the classic `keyword bind` (older Hyprland), kept so `keyword unbind` can remove exactly them.
    classic_binds: Vec<(String, String)>,
}

impl HyprlandWm {
    /// None when not running under Hyprland.
    pub fn connect() -> Option<HyprlandWm> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
        let dir = PathBuf::from(runtime).join("hypr").join(sig);
        let events = UnixStream::connect(dir.join(".socket2.sock")).ok().filter(|s| s.set_nonblocking(true).is_ok());
        if events.is_none() {
            SLOG.warn(format_args!("No Hyprland event socket; window changes are picked up by periodic rescans only"));
        }
        Some(HyprlandWm { request_path: dir.join(".socket.sock"), events, event_buf: String::new(), lua_dispatch: std::cell::Cell::new(None), classic_binds: Vec::new() })
    }

    fn request(&self, command: &str) -> Option<String> {
        let mut stream = UnixStream::connect(&self.request_path).ok()?;
        stream.write_all(command.as_bytes()).ok()?;
        let mut out = String::new();
        stream.read_to_string(&mut out).ok()?;
        Some(out)
    }

    fn request_json(&self, command: &str) -> Option<Value> {
        let text = self.request(command)?;
        serde_json::from_str(&text).map_err(|e| SLOG.warn(format_args!("Unparseable reply to '{command}': {e}"))).ok()
    }

    /// Runs a dispatcher, given in both syntaxes; tries whichever this Hyprland accepted last, then the other.
    fn dispatch(&self, classic: &str, lua: &str) {
        let order = match self.lua_dispatch.get() {
            Some(true) => [true, false],
            _ => [false, true],
        };
        let mut replies = Vec::new();
        for use_lua in order {
            let args = if use_lua { lua } else { classic };
            let reply = self.request(&format!("dispatch {args}"));
            if reply.as_deref() == Some("ok") {
                self.lua_dispatch.set(Some(use_lua));
                return;
            }
            replies.push((args, reply));
        }
        SLOG.warn(format_args!("dispatch failed in both syntaxes: {replies:?}"));
    }

    /// Runs Lua inside Hyprland (0.57+ with a Lua config); None if this Hyprland has no `eval`.
    fn eval(&self, lua: &str) -> Option<bool> {
        let reply = self.request(&format!("eval {lua}"))?;
        if reply.starts_with("unknown request") {
            return None;
        }
        if reply != "ok" {
            SLOG.warn(format_args!("eval failed: {reply}"));
        }
        Some(reply == "ok")
    }

    fn uses_lua(&self) -> bool {
        if let Some(known) = self.lua_dispatch.get() {
            return known;
        }
        let lua = self.eval("return true").is_some();
        self.lua_dispatch.set(Some(lua));
        lua
    }

    /// Workspace selector for whatever the focused monitor is showing (id for numbered workspaces, else its name).
    fn current_workspace(&self) -> Option<(String, String)> {
        let v = self.request_json("j/activeworkspace")?;
        let id = int(&v, "id");
        let name = string(&v, "name");
        Some(if id > 0 { (id.to_string(), id.to_string()) } else { (format!("name:{name}"), format!("\"{}\"", lua_escape(&name))) })
    }

    fn raw_monitors(&self) -> Vec<(Monitor, i64, String)> {
        let Some(Value::Array(list)) = self.request_json("j/monitors") else { return Vec::new() };
        let mut out: Vec<(Monitor, i64, String)> = list.iter().map(parse_monitor).collect();
        out.sort_by_key(|(_, id, _)| *id);
        out
    }

    fn parse_event(&self, line: &str) -> Option<WmEvent> {
        let (name, data) = line.split_once(">>")?;
        match name {
            "activewindowv2" => Some(WmEvent::Focus(parse_address(data).filter(|&a| a != 0))),
            "openwindow" | "closewindow" | "windowtitlev2" | "movewindowv2" | "changefloatingmode" | "monitoradded" | "monitorremoved" | "monitoraddedv2"
            | "monitorremovedv2" | "activespecial" | "activespecialv2" | "fullscreen" | "workspacev2" => Some(WmEvent::ConfigReloaded),
            "custom" => data.strip_prefix(EVENT_PREFIX).and_then(|id| id.parse().ok()).map(WmEvent::Hotkey),
            _ => None,
        }
    }
}

impl WindowManager for HyprlandWm {
    fn name(&self) -> &'static str {
        "Hyprland"
    }

    fn monitors(&self) -> Vec<Monitor> {
        self.raw_monitors().into_iter().map(|(m, _, _)| m).collect()
    }

    fn toplevels(&self) -> Vec<Toplevel> {
        let Some(Value::Array(list)) = self.request_json("j/clients") else { return Vec::new() };
        // Hyprland has no minimize; a window parked on a special workspace that no monitor is showing is the closest thing.
        let monitors = self.raw_monitors();
        let shown_specials: Vec<&str> = monitors.iter().map(|(_, _, special)| special.as_str()).filter(|s| !s.is_empty()).collect();
        list.iter()
            .filter(|v| v.get("mapped").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|v| {
                let workspace = v.get("workspace").map(|w| string(w, "name")).unwrap_or_default();
                Some(Toplevel {
                    id: parse_address(v.get("address")?.as_str()?)?,
                    title: string(v, "title"),
                    class: string(v, "class"),
                    initial_class: string(v, "initialClass"),
                    pid: int(v, "pid") as i32,
                    minimized: workspace.starts_with("special") && !shown_specials.contains(&workspace.as_str()),
                    at: pair(v, "at"),
                    floating: v.get("floating").and_then(Value::as_bool).unwrap_or(false),
                    monitor: monitors.iter().find(|(_, id, _)| *id == int(v, "monitor")).map(|(m, _, _)| m.name.clone()).unwrap_or_default(),
                })
            })
            .collect()
    }

    fn active(&self) -> Option<WindowId> {
        let v = self.request_json("j/activewindow")?;
        parse_address(v.get("address")?.as_str()?).filter(|&a| a != 0)
    }

    fn focus(&self, id: WindowId) {
        self.dispatch(&format!("focuswindow address:0x{id:x}"), &format!("hl.dsp.focus({{ window = \"address:0x{id:x}\" }})"));
    }

    fn set_hotkeys(&mut self, binds: &[KeyBind]) -> usize {
        if self.uses_lua() {
            // Our binds live in a Lua table so we can unbind exactly them (hl.unbind by key would also drop the user's own).
            self.eval("_G.evemaj = _G.evemaj or {}; for _, b in pairs(_G.evemaj) do b:unbind() end; _G.evemaj = {}");
            let mut ok = 0;
            for b in binds {
                let keys = b.modifiers.iter().copied().chain(std::iter::once(b.key.as_str())).collect::<Vec<_>>().join(" + ");
                let lua = format!(
                    "_G.evemaj[{id}] = hl.bind(\"{keys}\", hl.dsp.event(\"{EVENT_PREFIX}{id}\"), {{ repeating = {rep}, description = \"EVE-Maj: {desc}\" }})",
                    id = b.id,
                    keys = lua_escape(&keys),
                    rep = b.repeating,
                    desc = lua_escape(&b.description),
                );
                if self.eval(&lua) == Some(true) {
                    ok += 1;
                } else {
                    SLOG.err(format_args!("Couldn't bind {keys} ({})", b.description));
                }
            }
            return ok;
        }
        for (mods, key) in std::mem::take(&mut self.classic_binds) {
            self.request(&format!("keyword unbind {mods},{key}"));
        }
        let mut ok = 0;
        for b in binds {
            let mods = b.modifiers.join(" ");
            let kind = if b.repeating { "binde" } else { "bind" };
            if self.request(&format!("keyword {kind} {mods},{},event,{EVENT_PREFIX}{}", b.key, b.id)).as_deref() == Some("ok") {
                self.classic_binds.push((mods, b.key.clone()));
                ok += 1;
            } else {
                SLOG.err(format_args!("Couldn't bind {mods} {} ({})", b.key, b.description));
            }
        }
        ok
    }

    fn close(&self, id: WindowId) {
        self.dispatch(&format!("closewindow address:0x{id:x}"), &format!("hl.dsp.window.close({{ window = \"address:0x{id:x}\" }})"));
    }

    fn minimize(&self, id: WindowId) {
        self.dispatch(
            &format!("movetoworkspacesilent {MINIMIZE_WORKSPACE},address:0x{id:x}"),
            &format!("hl.dsp.window.move({{ workspace = \"{MINIMIZE_WORKSPACE}\", follow = false, window = \"address:0x{id:x}\" }})"),
        );
    }

    fn restore(&self, id: WindowId) {
        if let Some((classic_ws, lua_ws)) = self.current_workspace() {
            self.dispatch(
                &format!("movetoworkspacesilent {classic_ws},address:0x{id:x}"),
                &format!("hl.dsp.window.move({{ workspace = {lua_ws}, follow = false, window = \"address:0x{id:x}\" }})"),
            );
        }
        self.focus(id);
    }

    fn move_to(&self, id: WindowId, x: i32, y: i32) {
        let floating = self.toplevels().into_iter().find(|t| t.id == id).is_some_and(|t| t.floating);
        if !floating {
            self.dispatch(&format!("setfloating address:0x{id:x}"), &format!("hl.dsp.window.float({{ action = \"enable\", window = \"address:0x{id:x}\" }})"));
        }
        self.dispatch(&format!("movewindowpixel exact {x} {y},address:0x{id:x}"), &format!("hl.dsp.window.move({{ x = {x}, y = {y}, window = \"address:0x{id:x}\" }})"));
    }

    fn event_fd(&self) -> Option<RawFd> {
        self.events.as_ref().map(|s| s.as_raw_fd())
    }

    fn read_events(&mut self) -> Vec<WmEvent> {
        let mut out = Vec::new();
        let Some(stream) = self.events.as_mut() else { return out };
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    SLOG.warn(format_args!("Hyprland event socket closed"));
                    self.events = None;
                    break;
                }
                Ok(k) => self.event_buf.push_str(&String::from_utf8_lossy(&chunk[..k])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.events = None;
                    break;
                }
            }
        }
        while let Some(nl) = self.event_buf.find('\n') {
            let line: String = self.event_buf.drain(..=nl).collect();
            if let Some(ev) = self.parse_event(line.trim_end()) {
                out.push(ev);
            }
        }
        out
    }
}

fn lua_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

fn pair(v: &Value, key: &str) -> (i32, i32) {
    match v.get(key).and_then(Value::as_array) {
        Some(a) if a.len() == 2 => (a[0].as_i64().unwrap_or(0) as i32, a[1].as_i64().unwrap_or(0) as i32),
        _ => (0, 0),
    }
}

fn parse_address(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn string(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// (monitor, Hyprland id, name of the special workspace it's showing or "").
fn parse_monitor(v: &Value) -> (Monitor, i64, String) {
    let scale = v.get("scale").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let (pw, ph) = (int(v, "width") as f32, int(v, "height") as f32);
    // width/height are the mode's pixels; layout space is scaled and, for 90/270-degree transforms, rotated.
    let (w, h) = if int(v, "transform") % 2 == 1 { (ph / scale, pw / scale) } else { (pw / scale, ph / scale) };
    let (x, y) = (int(v, "x") as i32, int(v, "y") as i32);
    let bounds = Rect { left: x, top: y, right: x + w.round() as i32, bottom: y + h.round() as i32 };
    let reserved: Vec<i32> = v.get("reserved").and_then(Value::as_array).map(|a| a.iter().map(|n| n.as_i64().unwrap_or(0) as i32).collect()).unwrap_or_default();
    let r = |i: usize| reserved.get(i).copied().unwrap_or(0);
    let work_area = Rect { left: bounds.left + r(0), top: bounds.top + r(1), right: bounds.right - r(2), bottom: bounds.bottom - r(3) };
    let special = v.get("specialWorkspace").map(|w| string(w, "name")).unwrap_or_default();
    (Monitor { name: string(v, "name"), bounds, work_area, scale }, int(v, "id"), special)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_and_scaled_monitor_geometry() {
        let v: Value = serde_json::json!({"id": 1, "name": "DP-1", "x": 100, "y": 0, "width": 2560, "height": 1440, "scale": 2.0, "transform": 1, "reserved": [0, 40, 0, 0], "specialWorkspace": {"name": ""}});
        let (m, id, _) = parse_monitor(&v);
        assert_eq!(id, 1);
        assert_eq!(m.bounds, Rect { left: 100, top: 0, right: 820, bottom: 1280 });
        assert_eq!(m.work_area.top, 40);
    }

    #[test]
    fn lua_strings_are_escaped() {
        assert_eq!(lua_escape(r#"a "b" \ c"#), r#"a \"b\" \\ c"#);
    }

    #[test]
    fn addresses_parse_with_or_without_prefix() {
        assert_eq!(parse_address("0x55f882fe0500"), Some(0x55f882fe0500));
        assert_eq!(parse_address("55f882fe0500"), Some(0x55f882fe0500));
    }
}
