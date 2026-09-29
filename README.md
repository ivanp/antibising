# antibising

A permanent, noise-suppressed virtual microphone for Linux, built on PipeWire and RNNoise.
It installs a crash-resilient LADSPA filter-chain that survives device swaps and PipeWire
restarts, managed by a background daemon and controlled through a Tauri desktop app and a
system tray icon.

## Requirements

- **PipeWire ≥ 0.3** with `filter-chain.service`
- **RNNoise LADSPA plugin** — package `noise-suppression-for-voice` or equivalent;
  must provide `/usr/lib/ladspa/librnnoise_ladspa.so`
- **D-Bus session bus** (for the system tray)
- **systemd** user session (≥ 249)
- **Rust toolchain** (stable) and `cargo`

## Build

```sh
cargo build --release
```

Binaries land in `target/release/`:

| Binary | Crate | Purpose |
|---|---|---|
| `antibisingd` | `daemon` | Background daemon + install CLI |
| `app` | `app` | Desktop GUI and system tray |
| `engine-cli` | `engine` | Low-level engine debug CLI |

## Install

Run once to write the systemd user units and PipeWire filter-chain drop-in, then start the
daemon:

```sh
./target/release/antibisingd install
```

This creates:
- `~/.config/pipewire/filter-chain.conf.d/10-antibising-mic.conf`
- `~/.config/systemd/user/antibisingd.service`
- `~/.config/systemd/user/pipewire.service.d/10-antibising.conf`

The daemon starts automatically as a systemd user service from this point on.

## Arch Linux package

A `PKGBUILD` is provided at the repo root as an alternative to `antibisingd install`.
It builds both binaries with `cargo build --release --locked` and packages the daemon,
GUI app, systemd user unit, PipeWire crash-recovery drop-in, `.desktop` entry, and icon:

```sh
makepkg -si
```

Then enable the daemon and launch the GUI:

```sh
systemctl --user enable --now antibisingd.service
antibising
```

Remove with `sudo pacman -R antibising`; `~/.config/antibising/config.toml` and the
PipeWire filter-chain fragment are left in place as user data.

## Run the desktop app

```sh
./target/release/app
```

Left-click the tray icon to open the control panel. Right-click for the context menu
(device selection, denoise toggle, quit).

## Uninstall

```sh
./target/release/antibisingd uninstall
```

Stops and removes all generated systemd units and configuration files.

## Configuration

Persisted at `~/.config/antibising/config.toml`. Managed through the UI or manually
(manual edits apply on daemon restart).

| Key | Type | Description |
|---|---|---|
| `preference_order` | `[string]` | Ordered list of preferred input device names |
| `pin` | `string \| null` | Pinned input device name (overrides auto-selection) |
| `denoise_enabled` | `bool` | Whether RNNoise suppression is active |
| `vad_threshold` | `float` | Voice-activity detection threshold percentage (0.0 – 99.0) |
| `dry_mix` | `float` | Dry signal mix ratio; `1.0` bypasses RNNoise entirely |

## Architecture

```
app  (Tauri v2 + ksni tray)
 │  Unix socket IPC
 ▼
antibisingd  (systemd user service)
 │  in-process
 ▼
engine  (PipeWire session + LADSPA filter-chain)
```
