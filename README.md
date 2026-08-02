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

## Fresh application boundary

This project does not migrate or manage data from the earlier `z13-helper`
application. Its first and only configuration schema is version 1 at:

```text
$XDG_CONFIG_HOME/z13helper/config.json
```

Unsupported or corrupt files at that new path are preserved and reported. No
installer searches for, imports, aliases, or deletes legacy data, binaries,
services, sockets, groups, desktop files, or application IDs.

Before enabling this daemon, manually stop and remove competing writers:

```sh
systemctl --user disable --now z13ctl.socket z13ctl.service z13gui.service
sudo systemctl disable --now z13helper-fan-service.service
```

Exact old unit names vary by installation. Remove old application data yourself
if desired; the build and installation targets deliberately do not do so.
`z13helperd` also refuses to start while a live z13ctl socket or legacy fan
service socket is present.

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
power-profiles-daemon is available.

Each apply is validated and serialized by `z13helperd`. It restores the selected
base's measured five-value stock PPT table, then applies independent PPD, PPT,
two eight-point fan curves, and undervolt state in a fail-closed order. A PL1
above 75 W is permitted only after fan protection has been prepared; lowering
power happens before relaxing that protection.

The high-power fan floor defaults to 204 PWM, engages at 70°C, releases at 65°C,
and has a five-second dwell. Firmware mode transforms only the curve copy sent
to firmware. Direct mode uses live engage/release hysteresis and dwell. Authored
curves are never modified. There is intentionally no 96°C panic override: CPU
and firmware throttling remain authoritative.

## CLI

`z13helperctl status` and `z13helperctl probe` print JSON. `watch` streams daemon
events, while `apply -` accepts a complete version-1 apply request on stdin.
Focused commands cover profile, PPD, PPT, fans, undervolt, lighting, battery,
panel overdrive, boot sound, and direct-fan release. The CLI never owns named GUI
profiles.

## License

MIT. ASUS and ROG are trademarks of their respective owner.
