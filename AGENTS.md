# Agent guide — z13-helper

G-Helper-style GTK4/libadwaita GUI for the ASUS ROG Flow Z13 (GZ302EA). Frontend
only for the **z13ctl** daemon. Replaces **z13gui** for desktop use.

Read [README.md](README.md) and [ARCHITECTURE.md](ARCHITECTURE.md) before
changing apply logic, socket protocol, or threading.

## Hard constraints

- **Never** write sysfs, open hidraw, access raw I/O ports, or shell out to the
  `z13ctl` CLI from the GTK app. Normal hardware I/O goes through z13ctl;
  experimental direct fan control goes through the narrowly privileged
  `z13-helper-fan-service`. Both are reached through socket clients in
  `z13ctl-client`.
- **Never** call `SMUProbeUndervolt` / probe the SMU from this app. Use
  `get-state.undervolt_available` only.
- **Never** call the daemon from the GTK/main thread (10 s command deadline).
  Use `worker::blocking` → result on the GLib main context.
- **Never** touch widgets from worker threads.
- Do **not** run alongside `z13gui.service` (both claim Armoury Crate
  `gui-toggle`).
- No stretch goals unless the user asks: keep scope to the existing product.

## Workspace

| Crate | Role |
|---|---|
| `crates/z13ctl-client` | Socket client + error model |
| `crates/z13-helper-core` | Profiles, apply order, curve math, config, debounce (unit-tested) |
| `crates/z13-helper` | Thin GTK UI |
| `crates/z13-helper-fan-service` | Privileged direct-EC fan companion |

Binary / config / desktop name: **`z13-helper`**. Config:
`$XDG_CONFIG_HOME/z13-helper/config.json`.

Optional feature: `layer-shell` (gtk4-layer-shell may be missing on the host).

## Commands

Toolchain may live under workspace `.cargo` / `.rustup` (gitignored). Prefer:

```sh
make build    # release
make run
make test     # client + core; GTK is manual
make lint     # clippy -D warnings + rustfmt --check
make fmt
```

If `cargo` is missing from `PATH`: `export PATH="$PWD/.cargo/bin:$PATH"`.

## Apply order (do not reorder)

1. `profile-set <base>` — **always**, even if unchanged (clears prior overrides + PPD).
2. TDP / PPT (if enabled) — **before** fans.
3. Fan curve (if enabled).
4. Undervolt (if enabled and available).

PL1 above 75 W → daemon enforces an ~80% fan floor; send TDP first and clamp
curves accordingly.

## Profile model

- Daemon stock bases: quiet / balanced / performance (+ one virtual `custom` slot).
- GUI builtins: **Silent / Balanced / Turbo** (Quiet / Balanced / Performance).
- Named customs are GUI-only in config.json; applying pushes into the daemon’s
  single custom slot.
- Base + PPD are **one** control (`Profile.base`). Do not expose independent PPD.
- `profile-get` → stock base from sysfs; `get-state.profile` → `custom` when
  overrides are live. Mode header needs both.
- Builtins ship with override flags off. **Restore Factory Defaults** must reset
  stock PPT/curve/UV (and builtin `base` by id) and refresh the Fans+Power UI.

## UI conventions

- Main window: Silent / Balanced / Turbo / Fans+Power always fully visible;
  only **custom** mode buttons scroll if needed. Do not wrap the builtin row in
  a height-capped `ScrolledWindow` (clips `min-height: 72px` mode buttons).
- Prefer full-width scales under rows — ActionRow suffixes crush sliders.
- Charge limit writes: clamp to daemon range (min **40**).
- Sync toggles/sliders from `get-state` on load; don’t assume local defaults.
- Fans+Power: Close + Escape; reload curve/power/UV when the profile dropdown
  changes; equal-width power sliders.

## GTK / Wayland pitfalls (from z13gui)

- Set `GTK_A11Y=none` before GTK init.
- No `hexpand` in CSS — use `set_hexpand` in code.
- No `scale.add_mark()`; no `box-shadow` on animated containers.
- Gamescope: force `GDK_BACKEND=x11` when the gamescope Wayland socket is real;
  HUD overlay atom is `GAMESCOPE_EXTERNAL_OVERLAY` (display-only).
- Cairo fan curve paints its own colours (CSS does not style the canvas).
- Dial/`NotRunning` must show a banner — never treat unreachable daemon as success.

## Testing expectations

- Logic changes → unit tests in `z13-helper-core` / `z13ctl-client`.
- Run `make test` and `make lint` before finishing a task.
- Commit only when the user asks; GPG signing may prompt them.
