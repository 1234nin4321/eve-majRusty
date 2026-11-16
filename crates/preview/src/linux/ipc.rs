//! Single instance and `evemajpreview://` links on Linux.
//!
//! The running app listens on a Unix socket in $XDG_RUNTIME_DIR; a second invocation with a link (`--protocol URL`,
//! or the bare URL as xdg-open passes it) writes the URL there and exits, like the Windows build's WM_COPYDATA
//! forwarding. The links themselves reach us through a .desktop file registered for x-scheme-handler/evemajpreview.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use eve_maj_core::log::Scope;

const SLOG: Scope = Scope::new("ipc");
const DESKTOP_FILE: &str = "eve-maj-preview.desktop";
const SCHEME: &str = "evemajpreview";

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("eve-maj-preview.sock")
}

/// The link this process was started with, if any: `--protocol URL` or a bare `evemajpreview://...` argument.
pub fn url_from_args() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    eve_maj_core::protocol::protocol_url_from_args(&args).or_else(|| args.iter().skip(1).find(|a| a.starts_with(&format!("{SCHEME}://"))).cloned())
}

/// Hands `url` to the running instance; false if none is listening.
pub fn forward(url: &str) -> bool {
    match UnixStream::connect(socket_path()) {
        Ok(mut s) => s.write_all(url.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

/// The settings dialog's previews carry a whole thumbnail config; links are tiny.
const MAX_MESSAGE: u64 = 1024 * 1024;
const COMMAND_PREFIX: &[u8] = b"evemaj-cmd:";

/// Something sent to the running instance.
pub enum Incoming {
    /// An evemajpreview:// link from a second invocation.
    Url(String),
    /// A settings-dialog command: "evemaj-cmd:<id>\n<payload>", the id/payload pair the Windows build sends with WM_COPYDATA.
    Command(eve_maj_core::protocol::Command),
}

fn parse_message(buf: &[u8]) -> Option<Incoming> {
    if let Some(rest) = buf.strip_prefix(COMMAND_PREFIX) {
        let nl = rest.iter().position(|&b| b == b'\n')?;
        let id: usize = std::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
        let payload = &rest[nl + 1..];
        return eve_maj_core::protocol::Command::from_copy_data(id, (!payload.is_empty()).then_some(payload)).map(Incoming::Command);
    }
    let text = std::str::from_utf8(buf).ok()?.trim();
    (!text.is_empty()).then(|| Incoming::Url(text.to_owned()))
}

pub enum BindError {
    AlreadyRunning,
    Io(std::io::Error),
}

impl Listener {
    /// Claims the instance socket; AlreadyRunning if another instance answers on it.
    pub fn bind() -> Result<Listener, BindError> {
        let path = socket_path();
        if UnixStream::connect(&path).is_ok() {
            return Err(BindError::AlreadyRunning);
        }
        // Nobody answered, so any socket file left there is stale.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(BindError::Io)?;
        listener.set_nonblocking(true).map_err(BindError::Io)?;
        Ok(Listener { listener, path })
    }

    pub fn fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Messages from every pending connection.
    pub fn accept_all(&self) -> Vec<Incoming> {
        let mut out = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let mut buf = Vec::new();
            if stream.take(MAX_MESSAGE).read_to_end(&mut buf).is_err() || buf.is_empty() {
                continue;
            }
            match parse_message(&buf) {
                Some(m) => out.push(m),
                None => SLOG.warn(format_args!("Ignoring an unrecognized instance-socket message ({} bytes)", buf.len())),
            }
        }
        out
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn applications_dir() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("applications"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/applications"),
    }
}

/// Registers this executable as the handler for evemajpreview:// links (the Linux side of autoRegisterProtocol).
/// Only rewrites the .desktop file when its contents would change.
pub fn register_protocol_handler() {
    let Ok(exe) = std::env::current_exe() else { return };
    // Quoted only when needed: xdg-open's generic fallback (no desktop environment) can't run a quoted Exec path.
    let exe_str = exe.display().to_string();
    let exec = if exe_str.contains(char::is_whitespace) { format!("\"{exe_str}\"") } else { exe_str };
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=EVE-Maj Preview\nComment=Live thumbnails of EVE Online clients\nExec={exec} --protocol %u\nIcon=eve-maj-preview\nTerminal=false\nNoDisplay=true\nMimeType=x-scheme-handler/{SCHEME};\n"
    );
    let dir = applications_dir();
    let path = dir.join(DESKTOP_FILE);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(contents.as_str()) {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &contents)) {
        SLOG.warn(format_args!("Couldn't write {}: {e}", path.display()));
        return;
    }
    let mime = std::process::Command::new("xdg-mime").args(["default", DESKTOP_FILE, &format!("x-scheme-handler/{SCHEME}")]).status();
    let _ = std::process::Command::new("update-desktop-database").arg(&dir).status();
    match mime {
        Ok(s) if s.success() => SLOG.info(format_args!("Registered {SCHEME}:// links -> {}", exe.display())),
        _ => SLOG.warn(format_args!("xdg-mime couldn't register {SCHEME}:// links (is xdg-utils installed?)")),
    }
}
