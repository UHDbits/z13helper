# Reference material

Snapshots gathered for `../PROMPT.md`. Nothing here is part of the application.

## `screenshots/`

G-Helper (Windows) screenshots that serve as the visual spec.

- `ghelper-main-window.png` — target look for the main window. The GPU Mode, Flicker-free Dimming, refresh-rate, and "Run on startup" sections shown here are **not** wanted; see the prompt.
- `ghelper-fans-and-power.png` — target look for the Fans + Power window. The two separate fan charts shown here collapse into **one**, since z13ctl applies a single curve to both fans.

## `z13ctl-docs/`

Copied from the `z13ctl` checkout at `/home/uhdbits/z13ctl` (v1.3.1) so the prompt stays self-contained.

| File | Source | Rendered at |
| --- | --- | --- |
| `commands.md` | `docs/commands.md` | <https://dahui.github.io/z13ctl/commands/> |
| `daemon.md` | `docs/daemon.md` | <https://dahui.github.io/z13ctl/daemon/> |
| `api.md` | `docs/api.md` | <https://dahui.github.io/z13ctl/api/> |
| `installation.md`, `getting-started.md`, `index.md` | `docs/` | — |
| `protocol.md` | `PROTOCOL.md` | — |
| `client.go`, `types.go` | `api/` | <https://dahui.github.io/z13ctl/api-reference/> is generated from these |

The live checkout is authoritative if these drift. When docs and code disagree, believe the code.
