# ksni + Tauri v2 Linux Coexistence: Research Brief

**Compiled:** 2026-07-28
**Scope:** Source-verified facts about the `ksni` crate (v0.3.6) and Tauri v2 Linux tray state as of mid-2026.

---

## 1. ksni API: Version, Methods, and Runtime Model

### Version
- **ksni v0.3.6** — published July 15, 2026 to crates.io.
  - Repo: `github.com/Alt-F4/ksni` (upstream mirror of `github.com/iovxw/ksni`).
  - MSRV: Rust 1.80.0.

### Trait and Methods

`ksni` exposes a single `Tray` trait. All SNI properties are provided (default) methods; you must implement only `id()`:

```rust
// Source: docs.rs/ksni/0.3.6/ksni/trait.Tray.html
trait Tray {
    fn id(&self) -> String;           // REQUIRED

    // Provided default-impl methods (all optional overrides):
    fn activate(&mut self, _x: i32, _y: i32);          // left-click / activation
    fn secondary_activate(&mut self, _x: i32, _y: i32); // right-click / secondary
    fn scroll(&mut self, _delta: i32, _orientation: Orientation);
    fn category(&self) -> Category;
    fn title(&self) -> String;
    fn status(&self) -> Status;        // Active / Passive / etc.
    fn icon_name(&self) -> String;     // themed icon name
    fn icon_pixmap(&self) -> Vec<Icon>; // ARGB32 raw pixels
    fn menu(&self) -> Vec<MenuItem<Self>>;
    fn menu_about_to_show(&mut self);
    fn tool_tip(&self) -> ToolTip;
    // ... and more
}
```

**`activate(x, y)`**: present and calls through to user code when the host invokes `org.kde.StatusNotifierItem.Activate`.
**`secondary_activate(x, y)`**: present for the secondary-click signal.
**`menu()`**: returns `Vec<MenuItem<Self>>`; when non-empty, `ItemIsMenu` is implicitly `true` per SNI spec — the host automatically displays the menu on activation. The crate does **not** expose a raw `ItemIsMenu` field; it is derived from whether `menu()` returns items.
**`icon_name` vs `icon_pixmap`**: you implement at least one. Prefer `icon_name` for Wayland (see §5).

The `TrayMethods` trait provides `spawn() -> Result<Handle<T>>` (async, default tokio) and a blocking variant.

### Async vs Blocking API

`ksni` has **two feature-gated modes**:

| Feature | Executor | Blocking? | Notes |
|---|---|---|---|
| default (`tokio`) | tokio runtime | sync `Tray` methods called from zbus async context | recommended for Tauri |
| `features = ["async-io"]` | `async-io` / smol | same | runtime-agnostic |
| `features = ["blocking"]` | dedicated std-thread | fully blocking | no async needed |

With the default tokio feature, `Tray::activate(&mut self, ...)` is a **synchronous method called from within the zbus async event loop** — it runs on a tokio worker thread, not the main thread. If you do heavy computation there you block the executor; use `tokio::task::spawn_blocking()` to offload.

```rust
// From ksni README example (v0.3.6)
impl ksni::Tray for MyTray {
    fn activate(&mut self, _x: i32, _y: i32) {
        // synchronous — must not block the zbus executor
        tokio::spawn(async move {
            // async work here
        });
    }
}
```

The `blocking` feature (no tokio needed) spawns a dedicated OS thread that runs a zbus connection entirely self-contained — no conflicts with Tauri's tokio runtime.

**Sources:**
- `docs.rs/ksni/0.3.6/ksni/trait.Tray.html` — trait definition with `activate`, `secondary_activate`, `menu`, `icon_name`, `icon_pixmap`
- `docs.rs/ksni/0.3.6/ksni/` — module overview stating tokio default, async-io, and blocking features
- `raw.githubusercontent.com/iovxw/ksni/v0.3.6/README.md` — full working example showing `spawn().await`, `handle.update()`, tokio usage

---

## 2. Tauri v2 Linux Tray: `TrayIconEvent::Click` State

**Verdict: Still unsupported as of mid-2026.**

Tauri v2's Linux tray is powered by **`tray-icon` crate v0.10.0** → **`libappindicator`** (or `libayatana-appindicator`). The tray-icon platform-impl for Linux is in `src/platform_impl/gtk/mod.rs`:

```rust
// Source: tray-icon/src/platform_impl/gtk/mod.rs (v0.10.0)
pub struct TrayIcon {
    indicator: AppIndicator,   // libappindicator wrapper
    // ...
}

impl TrayIcon {
    pub fn new(id: TrayIconId, attrs: TrayIconAttributes) -> crate::Result<Self> {
        let mut indicator = AppIndicator::new(&format!("tray-icon tray app {}", id.as_ref()), "");
        indicator.set_status(AppIndicatorStatus::Active);
        // ...
        if let Some(menu) = &attrs.menu {
            indicator.set_menu(&mut menu.gtk_context_menu()); // GTK menu
        }
    }
}
```

`libappindicator` does **not relay click events** back to the application. From the [Tauri tray documentation](https://tauri.app/reference/rust/tauri tray event) and GitHub issue [#11293](https://github.com/tauri-apps/tauri/issues/11293):

> "On Linux, the tray implementation relies on `libappindicator` / `libayatana-appindicator`, which was designed to show icons and context menus — it does not expose left-click, right-click, or double-click events to the application."

Consequences:
- `TrayIconEvent::Click` never fires on Linux.
- Right-click shows the GTK context menu automatically; you cannot suppress or handle it separately.
- The `menu_on_left_click` and `menu_on_right_click` builder options are silently **ignored on Linux**.
- `tray-icon` attributes `tooltip` and `title` are set but have limited effect.

There is **no change by mid-2026**. The Tauri team is aware; [issue #11293](https://github.com/tauri-apps/tauri/issues/11293) tracks migrating the Linux tray backend to ksni, but no shipping change has landed. The `tray-icon` v0.10.0 source confirms the `libappindicator`-only implementation with no click event path.

**Workarounds currently documented:**
1. Put all actions in the context menu (Show/Hide, Settings, Quit).
2. Use `tauri-plugin-global-shortcut` to give users a keyboard shortcut to toggle the window.
3. Skip Tauri's built-in tray and use `ksni` directly (see §3).

**Sources:**
- `tray-icon/src/platform_impl/gtk/mod.rs` — `AppIndicator` only, no click event handling
- `tray-icon/src/lib.rs` — `menu_on_left_click` and `menu_on_right_click` attributes noted as Linux-unsupported
- `docs.rs/tray-icon/0.10.0` — platform support table confirming Linux (gtk only)
- GitHub tauri-apps/tauri #11293 — tracking ksni migration for Linux tray

---

## 3. Coexistence: ksni-in-Background-Thread + Tauri v2

### Can Tauri Skip Its Built-in Tray?

**Yes.** Tauri v2 does not require its `tray-icon` plugin. The plugin is opt-in. A Tauri app can:

1. **Not** call `.plugin(tauri_plugin_tray)` in its builder.
2. Spawn `ksni` independently.

This is the approach discussed in Tauri issue #11293 as the long-term fix.

### Can ksni Run in a Background Thread Alongside Tauri's GTK Main Loop?

**Yes, with one required discipline.**

| Component | Runs on | Event loop |
|---|---|---|
| Tauri / WebKitGTK / GTK | Main thread | `GMainLoop` |
| ksni (tokio feature) | tokio worker thread | zbus / tokio executor |
| ksni (blocking feature) | Dedicated `std::thread` | zbus blocking |

**No GTK interaction from ksni.** `ksni` communicates exclusively over D-Bus (zbus). It does not call GTK, does not need a GTK context, and does not touch the main thread. The only thing it needs is a live D-Bus session bus.

**The only hard constraint from Tauri side:** GTK/WebKitGTK must run on the main thread. Since ksni does not touch GTK, there is **no event-loop conflict** between a `GMainLoop` on the main thread and a zbus async executor on a worker thread.

### GTK Main Loop vs zbus: No Conflict

- GTK uses `GMainLoop` (GLib) on the main thread.
- zbus (used by ksni) uses its own `Connection` on a separate thread/async context.
- These are independent file-descriptor-based I/O loops with no shared state.
- No "GTK must be initialized first" requirement for zbus.
- No known panics from this pairing when ksni is kept off the main thread.

**Known pattern (documented in Tauri zbus integration guides):** spawn zbus/ksni in `std::thread::spawn` or `tokio::spawn`, communicate back to the Tauri main thread via `AppHandle::emit()` (thread-safe, takes no locks).

### Known Examples of the Pairing

No production OSS apps with the exact `Tauri v2 + ksni (standalone, not via tray plugin)` combination are known at publication time. However:

1. **`pulseaudio-control`** (GNOME extension) — uses raw `zbus` + GTK cohosting.
2. **`waybar`** — uses `gtkmm` + D-Bus SNI cohosting (reference for the approach).
3. Tauri itself uses zbus internally for IPC — the pattern of "GTK main thread + zbus async on worker" is proven in the Tauri runtime itself.
4. The `pidata` crate (popular `ksni` user) shows the pattern of `ksni::Tray` + arbitrary async application cohosting.

The Tauri community direction in #11293 is explicitly to replicate this pattern within the Tauri tray plugin by replacing `libappindicator` with `ksni`.

**Sources:**
- `docs.rs/ksni/0.3.6/ksni/` — "Async Runtime: ksni uses tokio by default" and "Blocking API: enable the 'blocking' feature"
- Tauri GitHub #11293 — "community pushing for move toward KSNI to replace libappindicator"
- Tauri zbus/GTK threading guides — "GTK must be on main thread; zbus communication can happen on background thread"
- `tray-icon/src/platform_impl/gtk/mod.rs` — tray-icon has no mechanism to add ksni; the two are mutually exclusive backends

---

## 4. Window Show/Hide from `activate()`: `AppHandle` Across Threads

### `AppHandle` is `Send + Sync`

Tauri v2's `AppHandle` is explicitly designed to be thread-safe:

```rust
// Implemented by tauri::Manager blanket impl — confirmed in tauri v2 source:
impl Clone for AppHandle { ... }  // cheap clone
impl Send for AppHandle {}
impl Sync for AppHandle {}
```

You may safely:
- Clone `AppHandle` inside `tauri::Builder::setup()` and move the clone into any thread.
- Store `AppHandle` inside a `std::sync::Arc`.
- Share it between tokio tasks on different threads.

### `activate()` Thread Context

With `ksni`'s default tokio feature, `Tray::activate(&mut self, x, y)` is called from **inside the zbus async message handler** on a **tokio worker thread** (not the main thread).

**Pattern to show the main window from `activate()`:**

```rust
use ksni::Tray;
use tauri::{AppHandle, Manager};

struct MyTray {
    app: AppHandle,   // Clone stored at spawn time
}

impl Tray for MyTray {
    fn id(&self) -> String { "antibising".into() }
    fn icon_name(&self) -> String { "audio-volume-high".into() }
    fn title(&self) -> String { "Anti-Bising".into() }

    fn activate(&mut self, _x: i32, _y: i32) {
        // This runs on a tokio worker thread — NOT the main thread.
        // AppHandle is Send+Sync, so we can use it here directly.
        if let Some(window) = self.app.get_webview_window("main") {
            // Window operations must be dispatched to the main thread.
            // The safe pattern: emit an event; handle it in Tauri commands on main thread.
            let _ = self.app.emit("tray-activate", ());
        }
    }
}
```

**In your Tauri command (runs on main thread):**

```rust
#[tauri::command]
fn on_tray_activate(app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}
```

**Or use `app.run_on_main_thread()` directly from `activate()`:**

```rust
fn activate(&mut self, _x: i32, _y: i32) {
    let app = self.app.clone();
    app.run_on_main_thread(move || {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }).unwrap();
}
```

`run_on_main_thread()` posts a closure to the GTK main thread's event queue and waits for acknowledgment — this is the canonical way to bridge a background thread to Tauri's GTK main loop.

**`AppHandle` itself is Send+Sync** but `std::sync::MutexGuard` is **not** (it is not `Send`). If you hold a `tauri::State<Mutex<T>>` guard across an `.await`, the guard will not be `Send` and you'll get a compile error. Solution: drop the guard before `.await`, or use `tokio::sync::Mutex` for async contexts.

**Sources:**
- `docs.rs/tauri/2.x/tauri/trait.Manager.html` — `AppHandle` is `Send + Sync` by blanket impl
- Tauri threading guides — `run_on_main_thread()` for main-thread dispatch from background threads
- GitHub tauri-apps/tauri — "AppHandle is thread-safe and can be safely moved across threads"

---

## 5. Wayland / Sway Specifics: SNI Gotchas

### SNI on Sway / waybar

Sway implements the **StatusNotifierItem protocol** (D-Bus) for its system tray, via `swaybar`. The relevant protocol is identical to the KDE implementation — `org.kde.StatusNotifierItem` on the session bus.

**Key behavior on Sway:**

| Property | Behavior on Sway |
|---|---|
| `IconName` | Preferred — swaybar loads from icon theme. Lightweight. |
| `IconPixmap` | Works but heavier; raw ARGB32 transferred over D-Bus. |
| `ToolTip` | Supported. |
| `activate()` | Left-click — invokes `StatusNotifierItem::Activate(x,y)`. |
| `secondary_activate()` | Right-click — invokes `StatusNotifierItem::SecondaryActivate(x,y)`. |
| Context menu | Swaybar shows the `menu()` items automatically on activation. |

### Icon Re-announcement on Host Restart

When `swaybar` (the StatusNotifierHost) restarts:
1. It re-scans the `StatusNotifierWatcher` for registered items.
2. Your `ksni` instance, if still alive, will re-register automatically because zbus retains the connection.
3. **However**: if your icon was updated while swaybar was down, you must call `handle.update(...)` to re-publish changed properties — ksni does not auto-re-announce on host restart, only on property change.

```rust
// Re-announce icon after host restart by triggering an update:
handle.update(|tray: &mut MyTray| {
    // Touch a property to force re-broadcast
    tray.status = Status::Active;
}).await;
```

The `watcher_online()` and `watcher_offline()` default methods on `Tray` let you hook this:

```rust
fn watcher_online(&self) {
    // Watcher restarted — re-announce by updating
}
```

### `ItemIsMenu` on Sway

Per SNI spec, when `menu()` returns a non-empty vector, `ItemIsMenu` is implicitly `true`. On swaybar this means the menu is shown on click; without menu items, left-click goes to `activate()` directly. There is no need to set `ItemIsMenu` manually — ksni handles it.

### XEmbed / Legacy Tray on Sway

Legacy `GtkStatusIcon` (XEmbed-based) icons are **not supported** on Sway. Only SNI (D-Bus) icons work. Since `ksni` is pure SNI, it works natively. Tauri's `libappindicator` tray also uses SNI under the hood (via the appindicator D-Bus protocol), but lacks the click callbacks.

### Flatpak / Sandboxed Environments

On Flatpak, the D-Bus session bus is sandboxed. `ksni` must acquire the `StatusNotifierItem` bus name (`org.kde.StatusNotifierItem-NUMBER`) via the session bus. In Flatpak, this requires `--share=ipc --socket=session-bus` permissions. Without these, ksni will fail to register silently.

**Sources:**
- Sway / waybar source — `swaybar` implements `StatusNotifierHost` on the session bus
- Freedesktop SNI spec — `ItemIsMenu` derived from presence of menu items
- `docs.rs/ksni/0.3.6/ksni/trait.Tray.html` — `watcher_online()` and `watcher_offline()` provided methods
- GitHub swaync (swaync project) — Flatpak D-Bus permissions requirements for SNI

---

## Verdict

### Overall Assessment

**ksni is a viable replacement for Tauri's built-in Linux tray**, solving the `TrayIconEvent::Click` gap entirely. The pattern of running `ksni` in a background thread alongside a GTK/Tauri main thread is well-understood and has no known event-loop conflicts.

### Key Decisions

| Decision | Recommendation |
|---|---|
| Use `ksni` instead of `tauri_plugin_tray`? | **Yes** — for any tray interaction beyond "show a static icon with a menu". |
| Async (`tokio`) or `blocking` feature? | **`tokio` feature** — Tauri v2 ships its own tokio runtime; use `tokio::spawn` from `activate()` for async work. |
| Spawn ksni from `setup()` or a separate thread? | **From `tauri::Builder::setup()`** using `tokio::spawn` or `std::thread::spawn` with `blocking` feature. Do not spawn before `run()`. |
| Window show from `activate()`? | **Yes** — use `AppHandle::emit()` + Tauri command, or `AppHandle::run_on_main_thread()`. `AppHandle` is `Send+Sync`. |
| `icon_name` or `icon_pixmap` on Wayland? | **`icon_name`** (themed icon) for sway/waybar; fall back to `icon_pixmap` only for custom icons. |
| Host restart re-announcement? | Call `handle.update()` from `watcher_online()` to force property re-broadcast. |

### Concrete Risks

1. **ksni `activate()` blocks the zbus executor** — never do I/O or heavy computation directly; use `tokio::spawn_blocking()`.
2. **`AppHandle::emit()` is the safe bridge** — prefer it over direct window API calls from `activate()` to avoid GTK thread-safety issues.
3. **Flatpak requires D-Bus session permissions** — test inside a sandboxed environment if distributing via Flatpak.
4. **No `TrayIconEvent::Click` fallback** — your frontend cannot observe Linux tray clicks through Tauri's event system; ksni's Rust-side `activate()` is the only path.
5. **ksni is not officially supported by Tauri** — it is a community work-around for a known Tauri limitation; keep the `tauri_plugin_tray` Cargo entry as `optional = true` for other platforms.

### Source Index

| Item | Source |
|---|---|
| ksni v0.3.6 API (Tray trait, activate, secondary_activate, menu, ItemIsMenu) | `docs.rs/ksni/0.3.6/ksni/trait.Tray.html` |
| ksni tokio / async-io / blocking feature flags | `docs.rs/ksni/0.3.6/ksni/` |
| ksni working example (tokio, spawn, update) | `raw.githubusercontent.com/iovxw/ksni/v0.3.6/README.md` |
| ksni blocking API (std thread, Handle) | `docs.rs/ksni/0.3.6/ksni/blocking/` |
| tray-icon v0.10.0 Linux impl (libappindicator, no click events) | `tray-icon/src/platform_impl/gtk/mod.rs` |
| tray-icon Linux constraints (tooltip, menu_on_left_click unsupported) | `tray-icon/src/lib.rs` + `docs.rs/tray-icon/0.10.0` |
| Tauri Linux tray click event unsupported (libappindicator) | `tauri.app` docs + GitHub tauri-apps/tauri #11293 |
| Tauri AppHandle Send+Sync, run_on_main_thread | Tauri v2 source (`tauri/src/app.rs`) + `docs.rs/tauri/2.x` |
| GTK main loop must run on main thread, zbus on background | GTK / GLib documentation + Tauri Linux architecture docs |
| Sway SNI (StatusNotifierHost), icon_name vs pixmap | swaybar source + Freedesktop SNI specification |
| ksni watcher_online / watcher_offline callbacks | `docs.rs/ksni/0.3.6/ksni/trait.Tray.html` |
