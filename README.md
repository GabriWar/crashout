# crashout

[![Built With Ratatui](https://img.shields.io/badge/Built_With_Ratatui-000?logo=ratatui&logoColor=fff)](https://ratatui.rs/)

![crashout TUI](assets/screenshot-tui.png)

A `systemd-coredump` browser, crash-notification daemon, and log viewer — all
in one terminal app. Built on top of `coredumpctl`, `journalctl`, and `gdb`.

- Browse past crashes with signal / time / size / filter / group
- Inspect each crash via tabs: `info`, `backtrace` (async `gdb` batch),
  `logs` (journal ±5min around the crash, colorcoded), `env`, `cmdline`
- Desktop notifications on every new crash (opt-out with `--no-notify`)
- Optional systray icon (StatusNotifierItem) with left-click to open the
  TUI and right-click menu to toggle notifications
- Log browser that scans `/var/log`, `/run/log`, `~/.local/share`,
  `~/.local/state`, `~/.cache`, `~/.config`, plus every
  `journalctl -F _SYSTEMD_UNIT` source (system + user), kernel ring
  buffer, and the full journal — all with level colorcoding
- Standalone `crashout log <file>` viewer for one-off log files

## Install

```sh
cargo install --path .
```

Binary lands at `~/.cargo/bin/crashout`. Make sure that's on your `$PATH`.

## Usage

```sh
crashout            # default: TUI (alias for `crashout tui`)
crashout tui        # interactive browser
crashout watch      # run the notification daemon (foreground)
crashout watch --no-notify         # stderr-only, no desktop popups
crashout watch --tray              # add a systray icon
crashout list       # print coredump list as JSON
crashout log <path> # open a log file with level colorcoding
```

## Daemon (systemd user service)

A service file is shipped in `contrib/systemd/`:

```sh
install -Dm644 contrib/systemd/crashout.service \
    ~/.config/systemd/user/crashout.service
systemctl --user daemon-reload
systemctl --user enable --now crashout.service
```

Default `ExecStart` is `crashout watch --tray`. Remove `--tray` if you don't
want the tray icon, or add `--no-notify` if you only want the stderr log.

## TUI keybinds

### Global

| Key      | Action                                 |
|----------|----------------------------------------|
| `1`      | switch to the crashes screen (default) |
| `2`      | switch to the logs browser             |
| `?`      | help overlay (any key to close)        |
| `q`      | quit                                   |

### Crashes screen

| Key            | Action                                                     |
|----------------|------------------------------------------------------------|
| `j`/`k` `↓`/`↑`| navigate list (list mode) or scroll preview (detail mode)  |
| `g` / `G`      | top / bottom                                               |
| `PgUp`/`PgDn`  | scroll preview                                             |
| `tab` / `S-tab`| cycle preview: `info` → `backtrace` → `logs` → `env` → `cmdline` |
| `enter`        | list → detail fullscreen, detail → `coredumpctl debug` (gdb) |
| `esc`          | detail → list, list → quit                                  |
| `o`            | dump core to `./core.<pid>`                                 |
| `S`            | save report to `crashout-<pid>-<ts>.txt`                    |
| `x`            | delete the corefile on disk                                 |
| `e`            | `xdg-open` the directory of the crashed binary              |
| `/`            | filter by exe name                                          |
| `s`            | cycle sort: `time↓` / `time↑` / `exe` / `sig` / `size↓`     |
| `m`            | toggle group-by-exe                                         |
| `f`            | cycle signal filter                                         |
| `t`            | cycle since filter: `all` / `1h` / `1d` / `1w` / `boot`     |
| `u`            | toggle only-failed-units                                    |
| `y` then p/e/g/i | yank pid / exe / gdb-cmd / info to clipboard              |
| `r`            | manual reload (auto-reloads every 2s)                       |

### Logs screen

| Key              | Action                                              |
|------------------|-----------------------------------------------------|
| `j`/`k` `g`/`G`  | navigate sources (browser) or lines (fullscreen)    |
| `PgUp`/`PgDn`    | scroll preview                                      |
| `enter`          | browser → fullscreen, fullscreen → open in `$EDITOR`|
| `esc`            | fullscreen → browser, browser → quit                |
| `/`              | filter sources                                      |
| `r` / `R`        | rescan all sources / refresh current preview        |

## Requirements

- `systemd` with `systemd-coredump` enabled
  (`systemctl status systemd-coredump.socket`)
- `gdb` (for the backtrace tab)
- `coredumpctl`, `journalctl` on `$PATH`
- `wl-clipboard` / `xclip` / `xsel` for yank
- A StatusNotifierItem host (Waybar, Plasma, etc.) for the tray
- A terminal on `$PATH` for tray left-click (respects `$TERMINAL`, then
  `xdg-terminal-exec`, then `kitty` / `foot` / `alacritty` / `wezterm` /
  `konsole` / `gnome-terminal` / `xterm`)

## License

MIT
