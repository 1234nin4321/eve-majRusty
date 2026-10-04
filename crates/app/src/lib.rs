//! Windows-side subsystems shared by the eve-maj-preview and config binaries.
//! Each module mirrors the Zig file of the same name under ../../src.
//! Modules without a cfg keep their platform-neutral parts testable on Linux and gate the Win32 parts inside.

pub mod sound;
pub mod tts;
pub mod update_stage;

#[cfg(windows)]
pub mod displays;
#[cfg(windows)]
pub mod mouse_hook;
#[cfg(windows)]
pub mod paste_upload;
#[cfg(windows)]
pub mod protocol;
#[cfg(windows)]
pub mod updater;
