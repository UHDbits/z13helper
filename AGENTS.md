# Agent guide — z13helper

Self-contained GTK4/libadwaita control platform for the ASUS ROG Flow Z13
(GZ302EA). Read `README.md` and `ARCHITECTURE.md` before changing apply logic,
socket protocol, lifecycle restoration, or threading.

## Hard constraints

- `z13helperd` is the only hardware writer. The GUI and CLI never write sysfs,
  open hidraw/input/SMU/raw I/O, or invoke another control CLI.
- The GUI and CLI never probe the SMU. They consume cached
  `DaemonState.undervolt_available` only.
- Never perform socket calls on the GTK main thread or touch widgets from worker
  threads. Use the UI worker service and return results to GLib.
- Do not discover, read, migrate, alias, move, or delete data outside the
  current application paths. Config begins at schema 1 in
  `$XDG_CONFIG_HOME/z13helper/config.json`.
- Do not add compatibility aliases, predecessor-path checks, or installers that
  remove unrelated data or units.
- Keep scope to the existing GZ302EA product; no stretch features.

## Workspace

| Crate | Role |
|---|---|
| `crates/z13helper-core` | Pure domain logic, protocol/error types, config, apply planning |
| `crates/z13helper-client` | Versioned NDJSON Unix-socket transport |
| `crates/z13helper` | Thin GTK controllers, views, widgets, and services |
| `crates/z13helperd` | Unified privileged hardware daemon |
| `crates/z13helperctl` | Stateless diagnostics and scripting CLI |

Socket: `/run/z13helper/z13helperd.sock`. Daemon state:
`/var/lib/z13helper/state.json`. Application ID:
`com.ashtonantila.z13helper`.

## Commands

```sh
make build
make run
make test
make lint
make fmt
```

The toolchain may live under workspace `.cargo`/`.rustup`. Socket tests can
require execution outside a restricted sandbox.

## Apply invariants

- Prevalidate a complete request before touching hardware and serialize applies.
- Selecting a base chooses the profile through power-profiles-daemon over D-Bus
  and restores its measured five-value stock PPT table.
- PPD is independent. Reject unknown selections; warn and continue if PPD is
  absent.
- At 80 W or above PL1, confirm target fan protection before raising power;
  abandon the increase if preparation fails.
- When lowering power, write PPT before relaxing previous fan protection.
- Undervolt is last. Retain safety fan protection on rollback/failure and report
  degraded state if rollback is incomplete.

## Fan policy

- Preserve two authored eight-point curves without safety mutation; only the
  copy written to hardware is transformed.
- At 80 W and above, the hardware copy locks point 7 to 80°C and at least 204
  PWM (80%) and point 8 to 90°C and 255 (100%), unless the confirmed Advanced
  override is set.
- Direct mode samples temperature every 250 ms, averages over a per-profile
  window (default 6 s, settable 0–15 s, 0 disables), then interpolates the
  curve to raw 0–255 PWM.
- Per-profile directional hysteresis of 1–5 (default 3/3) applies only when
  temperature direction reverses, and only in direct mode.
- Unchanged PWM duties are not rewritten.
- There is no 96°C panic/full-speed override; CPU/firmware throttling is
  authoritative.
- Release direct EC control first at startup, before suspend, and on sensor/EC
  failure, shutdown, or failed restoration.

## UI conventions

- Use named section views with `sync_from` and intent callbacks, not positional
  tuples or scattered reconciliation flags. All views share depth-based
  `SyncGuard`.
- Use 12 px outer margins and section gaps, 8 px row spacing, 6 px compact
  spacing. Keep the three built-ins plus Fans + Power fully visible.
- Use full-width PPT/undervolt scales and two balanced fan charts. Wide layouts
  are side-by-side; narrow layouts stack inside a scroller. Keep one persistent
  bottom action bar.
- Preserve Escape/Close, profile reload, `ColorDialogButton`, and tracing.
- Never use CSS `hexpand`, `Scale::add_mark`, or box shadows on animated
  containers. Validate the gamescope socket before forcing X11.

## Testing

- Logic changes require unit tests in core/client/daemon.
- Config tests cover only fresh v1 defaults, round trips, corruption
  preservation, validation, and unsupported-version rejection.
- Keep regression coverage proving temperature alone never overrides a curve.
- Run `make test` and `make lint` before finishing. Commit only when requested.
