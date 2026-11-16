# EVE-Maj Rusty

A Rust port of [EVE-Maj Preview](https://github.com/1234nin4321/EveMajImproved), the lightweight tool that shows live
thumbnail previews of EVE Online client windows: DWM thumbnails on Windows, and a native Wayland front end for
[Hyprland](https://hyprland.org) on Linux. Profiles (`profiles/*.json`) use the same format as the Zig build, so an
existing install's settings carry over.

> Work in progress.
> - **Windows:** Milestone 1 (thumbnails, clicking, dragging/snapping, layouts, notifications, tray, profile switching)
>   is ported; hotkeys, chatlog monitoring and its overlays, the client list and history panels, region selection and
>   `config.exe` are not yet.
> - **Linux (Hyprland):** thumbnails, layouts, hotkeys and groups, minimize/close all, auto-move, tray, `evemajpreview://`
>   links, and chat-log monitoring (system names, notifications, sounds, Travel Mode) work; the activity overlays,
>   panels, region selection and the config dialog are not yet. See [Linux (Hyprland)](#linux-hyprland).
>
> Unported settings still load and save, and log a warning when enabled.

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
the same text/border overlay as on Windows. Layout, styling and hotkey logic come from the same `core` code, so profiles
behave the same.

```sh
sudo pacman -S --needed rust wayland mesa libdrm cairo pango xdg-utils wl-clipboard   # Arch; any distro with these works
cargo linux                                                    # = build --release for x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/eve-maj-preview      # --profile NAME.json, --fps N (default 30)
```

- Profiles, `accounts.json` and `eve-maj.log` live in `~/.config/eve-maj/` (the Windows build keeps them next to the
  .exe). Edits to the active profile's JSON are picked up while it runs.
- EVE clients are found by the default window filter: the Wine/Proton process image `exefile.exe` (or a window class
  of that name). A filter's `class_names` also match Hyprland window classes, e.g. `steam_app_8500`.
- Left-click focuses a client, right-drag moves a thumbnail, Ctrl+right-drag moves them all, Shift+click toggles
  cycle exclusion. `display.monitorIndex` counts Hyprland monitors by id; with none set, `startX`/`startY` are relative
  to monitor 0.
- **Hotkeys**: every hotkey from the profile and `global.settings.json` (groups, per-character, account keys, profile
  switching, cycling, minimize/close all, suspend, app and URL keys, mouse side buttons) is registered as a Hyprland
  bind while the app runs and removed when it exits. They show up in `hyprctl binds` as "EVE-Maj: ...".
- **Minimize** parks a client on the hidden `special:evemaj` workspace; clicking its thumbnail or cycling to it brings
  it back to the current workspace. Clients on a special workspace that isn't shown count as minimized.
- **Auto-move** floats a client and moves it to its saved window position. Save the current positions from the tray
  ("Save Client Window Positions"); on Windows the config dialog does this.
- **Tray icon** (any StatusNotifierItem host: Waybar, KDE, ...): profiles, dragging/auto-minimize/travel/visibility
  toggles, restore positions, suspend hotkeys, close all, exit.
- **`evemajpreview://` links** work like on Windows: the app registers itself as the handler (`autoRegisterProtocol`),
  and a second start with a link (`eve-maj-preview --protocol URL`, or `xdg-open URL`) forwards it to the running instance.
- **Chat-log monitoring** (`chatlog.enabled`): system names, notifications (fleet invites, scrams, decloaks, cargo
  full, jumps, ...) and Travel Mode's left-behind alert, from EVE's chat and game logs. When `chatlogDir`/`gamelogDir`
  don't exist, the logs are found in the running client's Wine prefix (`WINEPREFIX`, or Proton's
  `STEAM_COMPAT_DATA_PATH/pfx`) under `drive_c/users/*/Documents/EVE/logs`.
- **Notification sounds** play through `ffplay`, `mpv`, `pw-play` or `paplay` (WAV/MP3; Windows `C:\...` paths from a
  copied profile are skipped); **spoken alerts** use `spd-say` (speech-dispatcher) or `espeak-ng`.
- Optional `hyprland.conf` rule: `layerrule = noanim, eve-maj-preview`.
- Thumbnail refresh rate is `--fps` (default 30): unlike DWM, every refresh is a GPU copy done by Hyprland, so lower it
  if you run many clients.

Not on Linux yet: the activity overlays (DPS, mining, bounty), the list/history panels, region selection and the config
dialog.

## License

GPL-3.0, like the project it is ported from. The bundled Cascadia fonts are under the SIL Open Font License
(`CascadiaCode-LICENSE.txt`).
