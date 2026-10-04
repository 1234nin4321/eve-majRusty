//! Which strings are acceptable profile names. Names arrive from the evemajpreview:// protocol (any web page can trigger it), window-message IPC, global.settings.json and the config dialog, and get joined onto the profiles directory, so anything that could address a different file is rejected here. Platform-neutral so it unit-tests anywhere.

pub const MAX_LEN: usize = 64;

/// Files that share the profiles directory but aren't profiles.
const RESERVED: [&str; 2] = ["global.settings.json", "accounts.json"];

/// Windows device names, which resolve to devices instead of files even with an extension.
const DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A plain "<name>.json" file name: no directory parts, drive or stream colons, characters Windows forbids in file names, leading/trailing dots or spaces, device names, or the app's own files.
pub fn is_safe(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_LEN {
        return false;
    }
    const EXT: &[u8] = b".json";
    if bytes.len() <= EXT.len() || !bytes[bytes.len() - EXT.len()..].eq_ignore_ascii_case(EXT) {
        return false;
    }
    for &c in bytes {
        if c < 0x20 || c == 0x7F {
            return false;
        }
        if matches!(c, b'/' | b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|') {
            return false;
        }
    }
    if bytes[0] == b'.' || bytes[0] == b' ' || bytes[bytes.len() - 1] == b' ' {
        return false;
    }
    if RESERVED.iter().any(|r| name.eq_ignore_ascii_case(r)) {
        return false;
    }
    let stem_end = bytes.iter().position(|&c| c == b'.').unwrap_or(bytes.len());
    let stem = &bytes[..stem_end];
    !DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_profile_names() {
        for name in ["default.json", "pvp fleet.json", "Mining-2.JSON", "a..b.json"] {
            assert!(is_safe(name), "{name}");
        }
    }

    #[test]
    fn rejects_anything_that_could_address_another_file() {
        let long = format!("{}.json", "a".repeat(61));
        let bad = [
            "", "..\\..\\x.json", "../x.json", "C:\\x.json", "C:x.json", "\\\\host\\share.json", "x.json:stream",
            "global.settings.json", "ACCOUNTS.json", "CON.json", "nul.json", "com1.cfg.json", "..json", ".json",
            ".hidden.json", "x.txt", "x.json ", " x.json", "x\0.json", "x\n.json", &long,
        ];
        for name in bad {
            assert!(!is_safe(name), "accepted: {name:?}");
        }
    }
}
