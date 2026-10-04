# EVE-Maj Rusty

A Rust port of [EVE-Maj Preview](https://github.com/1234nin4321/EveMajImproved), the lightweight Windows tool that
shows live DWM thumbnail previews of EVE Online client windows. Profiles (`profiles/*.json`) use the same format as
the Zig build, so an existing install's settings carry over.

> Work in progress. Milestone 1 (thumbnails, clicking, dragging/snapping, layouts, notifications, tray, profile
> switching) is ported; hotkeys, chatlog monitoring and its overlays, the client list and history panels, region
> selection and `config.exe` are not yet. Unported settings still load and save, and log a warning when enabled.

## Layout

| Crate | What it is |
|---|---|
| `crates/core` | Platform-independent model: config, logging, protocol URL parsing, trackers, accounts, update checks. Unit-tested on any host. |
| `crates/win` | Thin helpers over the Win32 API (windows, monitors, clipboard, shell, file pickers). |
| `crates/app` | Windows subsystems shared by both binaries: IPC, sound, TTS, updater, displays, mouse hook. |
| `crates/preview` | The `eve-maj-preview.exe` binary. |

`assets/` holds the embedded Cascadia fonts and the app icon.

## Building

Windows binaries cross-compile from Linux with the MinGW toolchain:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64   # provides the linker and windres
cargo build --release                   # default target is x86_64-pc-windows-gnu (.cargo/config.toml)
cargo t                                 # unit tests on the Linux host
```

On Windows, `cargo build --release --target x86_64-pc-windows-gnu` (or `-msvc`) works the same way.

## License

GPL-3.0, like the project it is ported from. The bundled Cascadia fonts are under the SIL Open Font License
(`CascadiaCode-LICENSE.txt`).
