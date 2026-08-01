# z13-helper

G-Helper-style GTK4/libadwaita desktop GUI for controlling an **ASUS ROG Flow Z13
(2025, GZ302EA)** on Linux. It is a frontend for **[z13ctl](https://github.com/dahui/z13ctl)** —
all hardware access goes through the z13ctl daemon socket. Never write sysfs,
never open hidraw, never shell out to the `z13ctl` binary from this app.

This project **replaces z13gui** for desktop use. Disable `z13gui.service` so
both apps do not fight over the Armoury Crate `gui-toggle` event:

```sh
systemctl --user disable --now z13gui.service
```

## Prerequisites

1. **z13ctl** installed and set up:
   ```sh
   sudo z13ctl setup          # group permissions on HID / sysfs
   systemctl --user enable --now z13ctl.socket z13ctl.service
   ```
2. **GTK4** ≥ 4.14 and **libadwaita** ≥ 1.5 (dev packages for building).
3. Optional: **`gtk4-layer-shell`** for a G-Helper-style Wayland HUD overlay
   (`cargo build -p z13-helper --features layer-shell`).
4. Optional: **`ryzen_smu`** (amkillam fork) for CPU undervolting. The app
   reads `undervolt_available` from the daemon and never probes the SMU itself.
5. Optional: **power-profiles-daemon**. z13ctl maps stock bases to PPD profiles;
   if PPD is missing, the write is silently ignored.

## Build & install

Rust 1.80+ required. This repo vendors a workspace-local rustup under `.cargo/`
if you bootstrapped that way; otherwise use a system toolchain.

```sh
make build          # release binary in target/release/z13-helper
make test           # pure-logic unit tests
make lint           # clippy -D warnings + rustfmt check
make install        # ~/.local/bin + .desktop + icon
make run
```

## Profile model

z13ctl has four profiles: `quiet`, `balanced`, `performance`, and a virtual
`custom` slot. Named custom profiles ("Gaming", …) are a **GUI-side** concept
stored in `~/.config/z13-helper/config.json`. Applying one pushes its values
into z13ctl's single custom slot.

Built-ins **Silent / Balanced / Turbo** ship with every override flag off, so
out of the box the app behaves like plain z13ctl.

### Apply order (not negotiable)

1. `profile-set <base>` — always, even if unchanged (clears prior overrides and
   sets the matching PPD profile).
2. TDP / PPT (if enabled) — **before** the fan curve.
3. Fan curve (if enabled).
4. Undervolt (if enabled and available).

### Why base and PPD are one control

z13ctl's `SetProfile` calls `powerprofilesctl set` itself after writing
`platform_profile`. TDP, fan, and undervolt handlers only set an in-memory
`custom` marker — they never retouch PPD. So the Power Profile dropdown stores
a single `base` and labels it with the PPD profile it implies
(`Balanced — PPD: balanced`). Off-diagonal combinations are intentionally
unreachable.

## Power source auto-switch

When AC/battery changes (UPower `OnBattery`, with sysfs fallback), the app
debounces ~2 s, applies the remembered profile for that source, and shows a
HUD toast (unless you clicked a main-window button — the highlight is enough).

## License

MIT — see sibling projects for trademark notes around ASUS / ROG naming.
