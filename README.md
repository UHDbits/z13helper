# z13helper

`z13helper` is a self-contained Linux control platform for the ASUS ROG Flow
Z13 (2025, GZ302EA). It ships a GTK4/libadwaita desktop application, one
privileged hardware daemon, and a diagnostic CLI. The GTK process never writes
hardware or opens hidraw, input devices, the SMU, or raw I/O ports; it may read
`/sys/class/power_supply` read-only as a UPower fallback.

The shipped components are:

| Component | Purpose |
|---|---|
| `z13helper` | Desktop UI and named user profiles |
| `z13helperd` | Root hardware daemon and sole hardware writer |
| `z13helperctl` | JSON-friendly diagnostics and scripting CLI |
| `z13helper-core` | Pure shared domain types, validation, and apply planning |
| `z13helper-client` | Shared Unix-socket transport for the UI and CLI |

The two libraries compile into the binaries; they are not separate services.
Keeping them separate prevents transport/I/O concerns from entering the pure,
unit-tested profile and safety model.

## Configuration boundary

The configuration starts at schema version 1 and lives at:

```text
$XDG_CONFIG_HOME/z13helper/config.json
```

An unsupported schema version at that path is left untouched and reported. A
corrupt or unparseable file is preserved alongside as `config.json.corrupt` and
fresh defaults are written at the original path. The application does not
discover, import, convert, alias, move, or delete data from other paths, and the
installer manages only current `z13helper` identifiers.

## Build and install

Rust 1.92+, GTK 4.14+, and libadwaita 1.5+ are required.

```sh
make build
make test
make lint
make install
make install-user-service
sudo make install-service
sudo usermod -aG z13helper "$USER"
```

`make install-user-service` installs the GUI as a user systemd service bound to
`graphical-session.target`. It starts hidden so the hardware GUI button can
open it. The unit optionally loads `%t/gamescope-environment`, allowing the
resident process to connect to gamescope's Xwayland display when Gaming Mode
starts. The privileged `z13helperd` service remains separate and must still be
installed and enabled as root.

Log out and back in after changing group membership. The system service creates
`/run/z13helper/z13helperd.sock` and stores flattened machine state atomically
at `/var/lib/z13helper/state.json`.

Optional GTK layer-shell support can be built with:

```sh
cargo build -p z13helper --features layer-shell
```

## Gamescope

When `GAMESCOPE_WAYLAND_DISPLAY` names a real socket inside
`XDG_RUNTIME_DIR` and `DISPLAY` is available, z13helper uses gamescope's
Xwayland overlay path. The resident main surface stays mapped while hidden;
the app clears input focus before setting its opacity to zero so an invisible
window cannot consume game input. Opening Fans + Power transfers overlay input
and visibility to that window, then restores the main window when it closes.
The Gamescope color chooser is an inline page, and selectors use in-surface
controls instead of popup surfaces that Gamescope cannot reliably composite.

The main Gamescope drawer starts at 320 logical pixels wide. Gamescope scaling
is derived from the X11 output width; on the Z13's native 2560-pixel-wide panel
this is approximately 1.5x. Set
`Z13HELPER_GAMESCOPE_SCALE` to a positive value to override detection; values
are clamped to 1.0–3.0 so an accidental setting cannot make the interface
unusable.

While the Gamescope overlay is visible, `z13helperd` exclusively captures
connected gamepads: the D-pad and left stick move GTK focus, A activates the
focused control, and B goes back or closes the active window. Capture is released just after the
dismiss-button release when the overlay hides. A short renewable lease also
releases it automatically if the GUI exits unexpectedly, so a crashed overlay
cannot keep input from a game.
Touchscreen and touchpad nodes without gamepad buttons are explicitly excluded
from controller capture.

For PlayStation and Nintendo controllers, the daemon attaches a narrow BPF LSM
program while the Gamescope overlay capture lease is active. It returns
`EAGAIN` only for Steam's process tree reading hidraw devices, preventing Steam
Input from also receiving the same controller presses. The installed system unit
grants only the required `CAP_BPF` and `CAP_PERFMON` capabilities in addition to
its existing raw-I/O capability; z13helper never pauses Steam as a fallback. If
the kernel does not provide BPF LSM support, ordinary evdev capture still works
and the daemon records that hidraw suppression is unavailable.

## Profiles and safety

Silent, Balanced, and Turbo are fresh version-1 defaults. Named profiles live
only in the user configuration. PPD is independently selectable when
power-profiles-daemon is available. On first use, the GUI asks the daemon to
load each built-in profile's factory CPU and GPU curve tables from firmware;
the bundled G-Helper-derived curves remain the fallback when that query fails.
ASUS PPT attributes retain the last values written and have no factory-read
mode, so untouched profiles restore their measured per-PPD five-value table.

Each apply is validated and serialized by `z13helperd`. PPD selects the firmware
power policy, after which the daemon applies optional PPT, two eight-point fan
curves, an 80–99°C per-profile APU thermal limit, and undervolt state in a
fail-closed order. The thermal limit sets both Strix Halo Tctl (MP1 `0x19`) and
cHTC (MP1 `0x63`), then verifies the effective Tctl value from the PM table. A PL1
at 80 W or above is permitted only after fan protection has been prepared; lowering
power happens before relaxing that protection.

At 80 W and above, the hardware copy locks point 7 to 80°C and at least 80% PWM and
point 8 to 90°C and 100%. An Advanced override can disable this protection only
after confirmation. Direct mode samples the native-resolution temperature every
250 ms, shares that sample with telemetry, and averages recent values before
interpolating the curve into raw 0–255 EC PWM duty. The per-profile averaging
window defaults to 6 seconds and can be set from 0–15 seconds; 0 disables it. Its
per-profile 1–5 speed-up and slow-down hysteresis applies only when temperature
direction reverses, defaulting to 3/3 like G-Helper. New direct control establishes
the curve target immediately; later PWM changes ramp up within one second and
down over roughly 2.5 seconds. RPM is read separately for telemetry.
There is intentionally no 96°C panic override: CPU and firmware throttling
remain authoritative.

## CLI

`z13helperctl status` and `z13helperctl probe` print JSON. `watch` streams daemon
events, while `apply -` accepts a complete apply request on stdin over the v2
wire protocol.
Focused commands cover PPD, PPT, fans, undervolt, lighting, battery,
panel overdrive, and direct-fan release. The CLI never owns named GUI profiles.

The battery slider stores the normal 40–100% charge limit. A separate one-time
100% override is persisted by `z13helperd`, remains active without the GUI, and
automatically restores the normal limit when battery telemetry reaches 100%.
`battery-charge-once on|off` exposes the same toggle to scripts.

The Display section stores panel overdrive as a user policy: either always on,
or on while plugged in and off on battery. Confirmed power-source transitions
apply the effective value through `z13helperd`; the GUI never writes the
firmware attribute directly.

## License

MIT. ASUS and ROG are trademarks of their respective owner.
