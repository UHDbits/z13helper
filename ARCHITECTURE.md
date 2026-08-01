# Architecture

z13-helper is a three-crate Cargo workspace:

| Crate | Role |
|---|---|
| `z13ctl-client` | NDJSON Unix-socket client for the z13ctl daemon |
| `z13-helper-core` | Pure logic: profiles, apply sequencing, curve math, config, debounce |
| `z13-helper` | Thin GTK4 / libadwaita UI |

`cargo test` covers the first two; the GTK layer is exercised manually.

## Threading

Never call the daemon from the GTK thread (commands have a 10 s deadline).
Pattern:

1. Snapshot widget values on the UI thread.
2. `worker::blocking` runs the socket I/O on a std thread.
3. The result is delivered back on the GLib main context via `async_channel`.

An `applying` in-flight guard serialises profile applies. Telemetry is a 1 Hz
timer that stops doing work while the window is hidden and skips ticks while a
previous `get-state` is still in flight.

## Error model (`handled` collapse)

The Go API returns `(handled bool, err error)` where `(false, nil)` means the
daemon was not reached. We collapse that into:

- `DaemonError::NotRunning` — socket missing / refused
- `DaemonError::PermissionDenied` — EACCES (run `sudo z13ctl setup`)
- `DaemonError::Timeout` / `Rejected` / `Protocol`

`NotRunning` reveals a persistent `AdwBanner` with the systemctl hint. Never
treat dial failure as success (z13gui "dead button" bug).

## Apply ordering

See README. Critical consequences:

- Stock `profile-set` wipes custom fans / UV / PPT from hardware — it must run
  first so the outgoing profile's overrides are cleared.
- Above 75 W PL1, z13ctl writes a 204 PWM floor **before** the power limit and
  rejects any later curve with a point below that floor. Always send TDP first;
  the curve editor clamps to the floor when the profile's PL1 exceeds 75 W.
- Steps 2–4 only flip the daemon's in-memory `Profile` marker to `custom`.
  `platform_profile` and PPD stay at the base from step 1.

## Profiles vs daemon `custom`

GUI named profiles live in `$XDG_CONFIG_HOME/z13-helper/config.json`. The
daemon has exactly one saved custom slot. Applying a GUI profile means pushing
into that slot. `profile-get` returns the stock base from sysfs (never
`custom`); `get-state.profile` reports `custom` when overrides are live. Both
are needed for the mode header (`Mode: Balanced+ 20W`).

## Power source + HUD

- Primary: UPower `OnBattery` via zbus (polled on a background thread).
- Fallback: `/sys/class/power_supply/*/online` (Mains) or Battery status.
- Debounce: pure `PowerDebouncer` in core (~2 s, configurable).
- HUD: gtk4-layer-shell overlay when built with `--features layer-shell`
  (empty input region for click-through). Under gamescope (`GDK_BACKEND=x11`
  after validating `GAMESCOPE_WAYLAND_DISPLAY`), set
  `GAMESCOPE_EXTERNAL_OVERLAY` (display-only, no input — the right atom for a
  toast; z13gui uses `STEAM_OVERLAY` because its drawer needs input). Fallback:
  `org.freedesktop.Notifications`.

## gui-toggle

Background subscribe with exponential-backoff reconnect. 50 ms leading-edge
debounce (stay under ~120 ms — 250 ms swallowed real presses in z13gui).
Toggle presents/hides the main window. This app replaces z13gui for that
button; do not run both.

## GTK4 / Wayland pitfalls (from z13gui — do not re-introduce)

- Set `GTK_A11Y=none` before GTK init (AT-SPI D-Bus timeouts).
- No `hexpand` in CSS — use `set_hexpand` in code.
- No `scale.add_mark()` — GtkGizmo / pixman warnings.
- No `box-shadow` on animated containers (Wayland Vulkan smearing).
- Gamescope advertises layer-shell but does not implement anchoring/margins —
  force `GDK_BACKEND=x11` when the gamescope Wayland socket exists; validate
  the socket (stale env is common).
- Never touch widgets from worker threads.
- Cairo-painted widgets (fan curve) paint their own colours; CSS tokens do not
  apply to the canvas.
- Never call `SMUProbeUndervolt` from a short-lived client — it is destructive
  (writes CO offset 0). Use `get-state.undervolt_available` only.
