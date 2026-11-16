//! eve-maj-config: the settings dialog (config.exe in the Windows/Zig build). The page is the Zig build's own
//! HTML/CSS/JS, unchanged; this binary serves it to a native web view and answers its `webui.call(...)` requests.

#[cfg(target_os = "linux")]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(e) = linux::run() {
        eprintln!("eve-maj-config: {e}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("eve-maj-config: only the Linux build exists so far");
        std::process::exit(1);
    }
}
