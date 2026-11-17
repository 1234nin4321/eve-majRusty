//! Just enough of the seat's XKB keymap (the text wl_keyboard hands over) to know which keys are Enter and Escape,
//! so the region selection overlay follows remaps (e.g. Caps Lock as Escape) and virtual keyboards with their own
//! keycodes, instead of assuming the standard evdev codes.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};

/// Keycodes as wl_keyboard reports them (evdev numbering: XKB keycode - 8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfirmKeys {
    pub enter: Vec<u32>,
    pub escape: Vec<u32>,
}

/// Reads a keymap handed over by wl_keyboard::keymap (XKB v1 text in a shared, read-only mapping).
pub fn read_keymap(fd: &OwnedFd, size: u32) -> Option<ConfirmKeys> {
    let size = size as usize;
    if size == 0 {
        return None;
    }
    unsafe {
        let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, fd.as_raw_fd(), 0);
        if ptr == libc::MAP_FAILED {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr as *const u8, size);
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(size);
        let keys = std::str::from_utf8(&bytes[..end]).ok().map(parse);
        libc::munmap(ptr, size);
        keys
    }
}

fn skip_ws(s: &str) -> &str {
    s.trim_start()
}

/// `<NAME>` at the start of `s` (after whitespace): the name and the rest.
fn key_name(s: &str) -> Option<(&str, &str)> {
    let s = skip_ws(s).strip_prefix('<')?;
    let end = s.find('>')?;
    Some((&s[..end], &s[end + 1..]))
}

/// The keysyms of a key's first group: the first `[ ... ]` that isn't an index like `symbols[1]=`.
fn first_group(body: &str) -> Vec<&str> {
    let mut rest = body;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']').map(|c| open + c) else { break };
        let after = rest[close + 1..].trim_start();
        if !after.starts_with('=') {
            return rest[open + 1..close].split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        }
        rest = &rest[close + 1..];
    }
    Vec::new()
}

pub fn parse(text: &str) -> ConfirmKeys {
    // xkb_keycodes: "<RTRN> = 36;" and "alias <ALIAS> = <REAL>;".
    let mut codes: HashMap<&str, u32> = HashMap::new();
    let mut aliases: HashMap<&str, &str> = HashMap::new();
    for statement in text.split(';') {
        // A section's first entry follows its opening brace: `xkb_keycodes "evdev" { <ESC> = 9`.
        let line = statement.rsplit('{').next().unwrap_or(statement).trim();
        if let Some(rest) = line.strip_prefix("alias") {
            if let Some((alias, rest)) = key_name(rest) {
                if let Some((real, _)) = skip_ws(rest).strip_prefix('=').and_then(key_name) {
                    aliases.insert(alias, real);
                }
            }
            continue;
        }
        let Some((name, rest)) = key_name(line) else { continue };
        if let Some(n) = skip_ws(rest).strip_prefix('=').map(str::trim).and_then(|v| v.parse::<u32>().ok()) {
            codes.insert(name, n);
        }
    }
    // xkb_symbols: "key <RTRN> { [ Return ] };" (later definitions win, as when xkb merges options).
    let mut syms: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut rest = text;
    while let Some(at) = rest.find("key") {
        let candidate = &rest[at + 3..];
        let preceded_ok = at == 0 || !rest.as_bytes()[at - 1].is_ascii_alphanumeric() && rest.as_bytes()[at - 1] != b'_';
        rest = candidate;
        if !preceded_ok {
            continue;
        }
        let Some((name, after)) = key_name(candidate) else { continue };
        let Some(body) = skip_ws(after).strip_prefix('{') else { continue };
        let Some(close) = body.find('}') else { break };
        syms.insert(name, first_group(&body[..close]));
        rest = &body[close..];
    }
    let mut keys = ConfirmKeys::default();
    let mut names: Vec<_> = syms.into_iter().collect();
    names.sort_by_key(|(n, _)| *n);
    for (name, group) in names {
        let real = aliases.get(name).copied().unwrap_or(name);
        let Some(code) = codes.get(real).or_else(|| codes.get(name)).and_then(|c| c.checked_sub(8)) else { continue };
        let Some(sym) = group.first() else { continue };
        // By name, or as the raw keysym newer xkbcommon versions write (Return 0xff0d, KP_Enter 0xff8d, ISO_Enter 0xfe34, Escape 0xff1b).
        match sym.to_ascii_lowercase().as_str() {
            "return" | "kp_enter" | "iso_enter" | "0xff0d" | "0xff8d" | "0xfe34" => keys.enter.push(code),
            "escape" | "0xff1b" => keys.escape.push(code),
            _ => {}
        }
    }
    keys.enter.sort_unstable();
    keys.escape.sort_unstable();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPILED: &str = r#"xkb_keymap {
xkb_keycodes "evdev+aliases(qwerty)" {
	minimum = 8;
	maximum = 255;
	<ESC>                = 9;
	<RTRN>               = 36;
	<CAPS>               = 66;
	<KPEN>               = 104;
	<AC01>               = 38;
	alias <LatQ>         = <AD01>;
};
xkb_types "complete" { virtual_modifiers NumLock; type "ONE_LEVEL" { modifiers= none; level_name[Level1]= "Any"; }; };
xkb_symbols "pc+us+inet(evdev)+capslock(escape)" {
	name[Group1]="English (US)";
	key <ESC>                {	[          Escape ] };
	key <RTRN>               {	[          Return ] };
	key <CAPS>               {	[          Escape ] };
	key <AC01>               {	type= "ALPHABETIC", symbols[Group1]= [ a, A ] };
	key <KPEN>               {	[        KP_Enter ] };
};
};"#;

    #[test]
    fn finds_enter_and_escape_including_remaps() {
        let k = parse(COMPILED);
        assert_eq!(k.enter, vec![28, 96]);
        assert_eq!(k.escape, vec![1, 58]);
    }

    #[test]
    fn handles_a_virtual_keyboards_tiny_keymap() {
        // wtype's style: its own keycodes, one key per keysym it needs.
        let k = parse("xkb_keymap {\nxkb_keycodes \"(unnamed)\" {\nminimum = 8;\nmaximum = 10;\n<K1> = 9;\n<K2> = 10;\n};\nxkb_types \"(unnamed)\" { include \"complete\" };\nxkb_compatibility \"(unnamed)\" { include \"complete\" };\nxkb_symbols \"(unnamed)\" {\nkey <K1> {[Return]};\nkey <K2> {[Escape]};\n};\n};\n");
        assert_eq!(k, ConfirmKeys { enter: vec![1], escape: vec![2] });
    }

    #[test]
    fn reads_hex_keysyms() {
        let k = parse("xkb_keycodes \"x\" {\n\t<ESC> = 9;\n\t<RTRN> = 36;\n\t<AE01> = 10;\n};\nxkb_symbols \"x\" {\n\tkey <ESC> {\t[ 0xff1b ] };\n\tkey <AE01> {\n\t\tsymbols[1]= [ 0x31, 0x21 ],\n\t\tsymbols[2]= [ 0x31, 0x21 ]\n\t};\n\tkey <RTRN> {\t[ 0xff0d ] };\n};");
        assert_eq!(k, ConfirmKeys { enter: vec![28], escape: vec![1] });
    }

    #[test]
    fn group_parsing_skips_indices() {
        assert_eq!(first_group(r#" type= "ALPHABETIC", symbols[Group1]= [ a, A ] "#), vec!["a", "A"]);
        assert_eq!(first_group(" [ Return ] "), vec!["Return"]);
        assert!(first_group(" vmods= NumLock ").is_empty());
    }
}
