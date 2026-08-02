# z13helper

`z13helper` is a self-contained Linux control platform for the ASUS ROG Flow
Z13 (2025, GZ302EA). It ships a GTK4/libadwaita desktop application, one
privileged hardware daemon, and a diagnostic CLI. The GTK process never opens
sysfs, hidraw, input devices, the SMU, or raw I/O ports.

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

Unsupported or corrupt files at that path are preserved and reported. The
application does not discover, import, convert, alias, move, or delete data from
other paths, and the installer manages only current `z13helper` identifiers.

## Build and install

Rust 1.80+, GTK 4.14+, and libadwaita 1.5+ are required.

```sh
make build
make test
make lint
make install
sudo make install-service
sudo usermod -aG z13helper "$USER"
```

Log out and back in after changing group membership. The system service creates
`/run/z13helper/z13helperd.sock` and stores flattened machine state atomically
at `/var/lib/z13helper/state.json`.

Optional GTK layer-shell support can be built with:

```sh
cargo build -p z13helper --features layer-shell
```

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
after confirmation. Direct mode interpolates the curve into raw 0–255 EC PWM
duty and provides per-profile 1–5 speed-up and slow-down temperature hysteresis,
defaulting to 3/3 like G-Helper. RPM is read separately for telemetry. There is
intentionally no 96°C panic override: CPU and firmware throttling remain
authoritative.

## CLI

`z13helperctl status` and `z13helperctl probe` print JSON. `watch` streams daemon
events, while `apply -` accepts a complete version-1 apply request on stdin.
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
