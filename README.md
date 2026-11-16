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

## Linux (Hyprland)

`eve-maj-preview` also runs natively on Linux under [Hyprland](https://hyprland.org). Each thumbnail is a
wlr-layer-shell overlay (so it stays above fullscreen clients); Hyprland renders the EVE window straight into a GPU
buffer (`hyprland-toplevel-export-v1` + dmabuf) that is drawn into the thumbnail without passing through the CPU, with
the same text/border overlay as on Windows. Layout and styling come from the same `core` code, so profiles behave the same.

```sh
sudo pacman -S --needed rust wayland mesa libdrm cairo pango   # Arch; any distro with these dev libs works
cargo linux                                                    # = build --release for x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/eve-maj-preview      # --profile NAME.json, --fps N (default 30)
```

- Profiles, `accounts.json` and `eve-maj.log` live in `~/.config/eve-maj/` (the Windows build keeps them next to the
  .exe). Edits to the active profile's JSON are picked up while it runs.
- EVE clients are found by the default window filter: the Wine/Proton process image `exefile.exe` (or a window class
  of that name). A filter's `class_names` also match Hyprland window classes, e.g. `steam_app_8500`.
- Left-click focuses a client, right-drag moves a thumbnail, Ctrl+right-drag moves them all. `display.monitorIndex`
  counts Hyprland monitors by id; with none set, `startX`/`startY` are relative to monitor 0.
- Clients on a special workspace that isn't shown count as minimized.
- Optional `hyprland.conf` rule: `layerrule = noanim, eve-maj-preview`.
- Thumbnail refresh rate is `--fps` (default 30): unlike DWM, every refresh is a GPU copy done by Hyprland, so lower it
  if you run many clients.

Not on Linux yet: hotkeys and groups, tray, `evemajpreview://` links, chatlog-driven notifications and overlays, the
list/history panels, region selection and the config dialog.

## License

GPL-3.0, like the project it is ported from. The bundled Cascadia fonts are under the SIL Open Font License
(`CascadiaCode-LICENSE.txt`).
