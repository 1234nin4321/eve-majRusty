//! Ultra Potato Mode: finds EVE's core_public__.yaml graphics settings files and forces their quality keys to the lowest setting.

use std::io::Read;
use std::path::Path;

use crate::config::{atomic_write_file, ConfigError};
use crate::log::Scope;

const SLOG: Scope = Scope::new("ultra_potato");

/// Keys in EVE's core_public__.yaml `device:` section that Ultra Potato Mode forces to -300 (the client's lowest-quality sentinel value).
pub const TARGET_KEYS: [&str; 9] = [
    "aoQuality",
    "charClothSimulation",
    "charTextureQuality",
    "postProcessingQuality",
    "reflectionQuality",
    "shaderQuality",
    "shadowQuality",
    "textureQuality",
    "volumetricQuality",
];

const MAX_YAML_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Serialized as `{"path": ..., "label": ...}`, the shape the config dialog's profile picker expects.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Profile {
    pub path: String,
    pub label: String,
}

/// Scans %LOCALAPPDATA%\CCP\EVE\*\settings*\ for core_public__.yaml files - EVE's shared graphics settings, one per client install / settings profile (multiboxers keep several, e.g. settings_Default, settings_<CharName>).
pub fn scan_profiles() -> Vec<Profile> {
    let local_app_data = std::env::var_os("LOCALAPPDATA");
    scan_profiles_in(local_app_data.as_deref().map(Path::new))
}

/// [`scan_profiles`] with the LOCALAPPDATA value passed in (None when the variable is unset).
pub fn scan_profiles_in(local_app_data: Option<&Path>) -> Vec<Profile> {
    let mut profiles = Vec::new();

    let Some(local_app_data) = local_app_data else {
        SLOG.warn(format_args!("LOCALAPPDATA environment variable not found"));
        return profiles;
    };

    let eve_root = local_app_data.join("CCP").join("EVE");

    let eve_dir = match std::fs::read_dir(&eve_root) {
        Ok(d) => d,
        Err(err) => {
            SLOG.info(format_args!("No EVE settings directory found at '{}': {err}", eve_root.display()));
            return profiles;
        }
    };

    for install_entry in eve_dir {
        let install_entry = match install_entry {
            Ok(e) => e,
            Err(err) => {
                SLOG.warn(format_args!("Failed to enumerate EVE install directory: {err}"));
                break;
            }
        };
        if !install_entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let install_name = install_entry.file_name().to_string_lossy().into_owned();
        let install_path = eve_root.join(&install_name);

        let install_dir = match std::fs::read_dir(&install_path) {
            Ok(d) => d,
            Err(err) => {
                SLOG.warn(format_args!("Failed to open EVE install directory '{}': {err}", install_path.display()));
                continue;
            }
        };

        for settings_entry in install_dir {
            let settings_entry = match settings_entry {
                Ok(e) => e,
                Err(err) => {
                    SLOG.warn(format_args!("Failed to enumerate EVE settings directory: {err}"));
                    break;
                }
            };
            if !settings_entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let settings_name = settings_entry.file_name().to_string_lossy().into_owned();
            if !settings_name.starts_with("settings") {
                continue;
            }

            let yaml_path = install_path.join(&settings_name).join("core_public__.yaml");
            if std::fs::File::open(&yaml_path).is_err() {
                continue;
            }

            profiles.push(Profile {
                path: yaml_path.to_string_lossy().into_owned(),
                label: format!("{install_name} / {settings_name}"),
            });
        }
    }

    profiles
}

/// Serialized as `{"path": ..., "ok": ..., "changed": ..., "error": ...}`, the per-file shape of the config dialog's apply response.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ApplyResult {
    pub path: String,
    #[serde(rename = "ok")]
    pub success: bool,
    pub changed: bool,
    #[serde(rename = "error")]
    pub error_message: Option<String>,
}

pub fn apply_to_files<P: AsRef<str>>(paths: &[P]) -> Vec<ApplyResult> {
    paths.iter().map(|p| apply_to_one_file(p.as_ref())).collect()
}

fn apply_to_one_file(path: &str) -> ApplyResult {
    let failed = |message: String| ApplyResult { path: path.to_owned(), success: false, changed: false, error_message: Some(message) };

    let original = match read_limited(path) {
        Ok(c) => c,
        Err(err) => return failed(err),
    };

    if !backup_exists(path) && !make_backup(path) {
        return failed("Failed to write backup".to_owned());
    }

    let outcome = patch_yaml_text(&original);

    if !outcome.changed {
        return ApplyResult { path: path.to_owned(), success: true, changed: false, error_message: None };
    }

    if let Err(err) = atomic_write_file(path, &outcome.text) {
        return failed(match err {
            ConfigError::Io(e) => io_error_name(&e),
            other => other.to_string(),
        });
    }

    ApplyResult { path: path.to_owned(), success: true, changed: true, error_message: None }
}

fn read_limited(path: &str) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| io_error_name(&e))?;
    let mut content = Vec::new();
    file.take(MAX_YAML_FILE_SIZE + 1).read_to_end(&mut content).map_err(|e| io_error_name(&e))?;
    if content.len() as u64 > MAX_YAML_FILE_SIZE {
        return Err("StreamTooLong".to_owned());
    }
    Ok(content)
}

/// Short error name for the config dialog, spelled like the Zig build's error names where one corresponds.
fn io_error_name(err: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => "FileNotFound".to_owned(),
        ErrorKind::PermissionDenied => "AccessDenied".to_owned(),
        ErrorKind::IsADirectory => "IsDir".to_owned(),
        ErrorKind::StorageFull => "NoSpaceLeft".to_owned(),
        kind => format!("{kind:?}"),
    }
}

fn backup_path(path: &str) -> String {
    format!("{path}.bak")
}

fn backup_exists(path: &str) -> bool {
    std::fs::File::open(backup_path(path)).is_ok()
}

fn make_backup(path: &str) -> bool {
    let backup_path = backup_path(path);
    if let Err(err) = std::fs::copy(path, &backup_path) {
        SLOG.err(format_args!("Failed to back up '{path}' to '{backup_path}': {err}"));
        return false;
    }
    true
}

#[derive(Debug, PartialEq, Eq)]
struct PatchOutcome {
    text: Vec<u8>,
    changed: bool,
}

/// Line-oriented text patch rather than a full YAML parse/re-serialize, so untouched keys, comments, and formatting survive byte-for-byte.
fn patch_yaml_text(original: &[u8]) -> PatchOutcome {
    let mut out = Vec::with_capacity(original.len());
    let mut any_changed = false;
    for (i, line) in original.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        match patch_line(line) {
            Some(patched) => {
                any_changed = true;
                out.extend_from_slice(&patched);
            }
            None => out.extend_from_slice(line),
        }
    }
    PatchOutcome { text: out, changed: any_changed }
}

fn trim_spaces(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| b != b' ').unwrap_or(s.len());
    let end = s.iter().rposition(|&b| b != b' ').map_or(start, |i| i + 1);
    &s[start..end]
}

/// Matches lines shaped like `  keyName: [connectionId, value]` for a key in TARGET_KEYS and returns the line with `value` rewritten to -300 if it isn't already (None means the line stays as is).
fn patch_line(line: &[u8]) -> Option<Vec<u8>> {
    let (body, cr): (&[u8], &[u8]) = match line.strip_suffix(b"\r") {
        Some(b) => (b, b"\r"),
        None => (line, b""),
    };

    let indent_len = body.iter().take_while(|&&b| b == b' ').count();
    let rest = &body[indent_len..];

    for key in TARGET_KEYS {
        let Some(after_key) = rest.strip_prefix(key.as_bytes()) else { continue };
        let Some(after_bracket) = after_key.strip_prefix(b": [") else { continue };

        let Some(close_idx) = after_bracket.iter().position(|&b| b == b']') else { continue };
        let content = &after_bracket[..close_idx];
        let Some(comma_idx) = content.iter().position(|&b| b == b',') else { continue };
        let id_str = trim_spaces(&content[..comma_idx]);
        let val_str = trim_spaces(&content[comma_idx + 1..]);

        if val_str == b"-300" {
            break;
        }

        let mut new_line = Vec::with_capacity(body.len() + 8);
        new_line.extend_from_slice(&body[..indent_len]);
        new_line.extend_from_slice(key.as_bytes());
        new_line.extend_from_slice(b": [");
        new_line.extend_from_slice(id_str);
        new_line.extend_from_slice(b", -300]");
        new_line.extend_from_slice(cr);
        return Some(new_line);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn patch(line: &str) -> Option<String> {
        patch_line(line.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn patch_line_rewrites_target_keys() {
        assert_eq!(patch("  shadowQuality: [1, 3]").as_deref(), Some("  shadowQuality: [1, -300]"));
        assert_eq!(patch("textureQuality: [ 7 ,2 ]\r").as_deref(), Some("textureQuality: [7, -300]\r"));
        assert_eq!(patch("    aoQuality: [12, 0] # trailing").as_deref(), Some("    aoQuality: [12, -300]"));
    }

    #[test]
    fn patch_line_leaves_other_lines_alone() {
        assert_eq!(patch("  shadowQuality: [1, -300]"), None);
        assert_eq!(patch("  shadowQuality: [1,  -300 ]"), None);
        assert_eq!(patch("  antiAliasing: [1, 3]"), None);
        assert_eq!(patch("  shadowQuality: 3"), None);
        assert_eq!(patch("  shadowQuality: [3]"), None);
        assert_eq!(patch("  shadowQuality: [1, 3"), None);
        assert_eq!(patch("\tshadowQuality: [1, 3]"), None);
        assert_eq!(patch("  shadowQualityX: [1, 3]"), None);
        assert_eq!(patch(""), None);
    }

    #[test]
    fn patch_yaml_text_preserves_untouched_bytes() {
        let original = b"device:\r\n  shaderQuality: [5, 2]\r\n  # comment\r\n  shadowQuality: [5, -300]\r\n  other: [5, 2]\r\n";
        let outcome = patch_yaml_text(original);
        assert!(outcome.changed);
        assert_eq!(
            outcome.text,
            b"device:\r\n  shaderQuality: [5, -300]\r\n  # comment\r\n  shadowQuality: [5, -300]\r\n  other: [5, 2]\r\n".to_vec()
        );

        let again = patch_yaml_text(&outcome.text);
        assert!(!again.changed);
        assert_eq!(again.text, outcome.text);

        let non_utf8 = b"\xff\xfe\n  aoQuality: [1, 1]";
        let outcome = patch_yaml_text(non_utf8);
        assert_eq!(outcome.text, b"\xff\xfe\n  aoQuality: [1, -300]".to_vec());
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eve_maj_ultra_potato_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_finds_settings_profiles() {
        let root = temp_root("scan");
        let eve = root.join("CCP").join("EVE");
        let install = eve.join("c_eve_sharedcache_tq_tranquility");
        std::fs::create_dir_all(install.join("settings_Default")).unwrap();
        std::fs::write(install.join("settings_Default").join("core_public__.yaml"), "x").unwrap();
        std::fs::create_dir_all(install.join("settings_Empty")).unwrap();
        std::fs::create_dir_all(install.join("cache")).unwrap();
        std::fs::write(install.join("cache").join("core_public__.yaml"), "x").unwrap();
        std::fs::write(eve.join("stray_file"), "x").unwrap();

        let profiles = scan_profiles_in(Some(&root));
        assert_eq!(
            profiles,
            vec![Profile {
                path: install.join("settings_Default").join("core_public__.yaml").to_string_lossy().into_owned(),
                label: "c_eve_sharedcache_tq_tranquility / settings_Default".to_owned(),
            }]
        );
        assert_eq!(
            serde_json::to_string(&profiles[0]).unwrap(),
            format!("{{\"path\":{},\"label\":\"c_eve_sharedcache_tq_tranquility / settings_Default\"}}", serde_json::to_string(&profiles[0].path).unwrap())
        );

        assert!(scan_profiles_in(None).is_empty());
        assert!(scan_profiles_in(Some(&root.join("missing"))).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn apply_patches_and_backs_up_once() {
        let root = temp_root("apply");
        let yaml = root.join("core_public__.yaml");
        let yaml_str = yaml.to_string_lossy().into_owned();
        std::fs::write(&yaml, "device:\n  shadowQuality: [1, 3]\n").unwrap();

        let results = apply_to_files(std::slice::from_ref(&yaml_str));
        assert_eq!(results, vec![ApplyResult { path: yaml_str.clone(), success: true, changed: true, error_message: None }]);
        assert_eq!(std::fs::read_to_string(&yaml).unwrap(), "device:\n  shadowQuality: [1, -300]\n");
        assert_eq!(std::fs::read_to_string(root.join("core_public__.yaml.bak")).unwrap(), "device:\n  shadowQuality: [1, 3]\n");

        // The first backup is the pristine one; later runs never overwrite it.
        std::fs::write(&yaml, "device:\n  shadowQuality: [1, 4]\n").unwrap();
        let results = apply_to_files(&[yaml_str.as_str()]);
        assert!(results[0].changed);
        assert_eq!(std::fs::read_to_string(root.join("core_public__.yaml.bak")).unwrap(), "device:\n  shadowQuality: [1, 3]\n");

        let results = apply_to_files(&[yaml_str.as_str()]);
        assert_eq!(results[0], ApplyResult { path: yaml_str.clone(), success: true, changed: false, error_message: None });

        let missing = root.join("missing.yaml").to_string_lossy().into_owned();
        let results = apply_to_files(&[missing.as_str()]);
        assert_eq!(results[0], ApplyResult { path: missing, success: false, changed: false, error_message: Some("FileNotFound".to_owned()) });
        assert_eq!(
            serde_json::to_value(&results[0]).unwrap(),
            serde_json::json!({"path": results[0].path, "ok": false, "changed": false, "error": "FileNotFound"})
        );

        std::fs::remove_dir_all(&root).unwrap();
    }
}
