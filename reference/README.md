# Reference material

Snapshots gathered for `../PROMPT.md`. Nothing here is part of the application.

## `screenshots/`

G-Helper (Windows) screenshots that serve as the visual spec.

- `ghelper-main-window.png` — target look for the main window. The GPU Mode, Flicker-free Dimming, refresh-rate, and "Run on startup" sections shown here are **not** wanted; see the prompt.
- `ghelper-fans-and-power.png` — visual reference for the Fans + Power window. The new daemon owns two independent fan paths, so the two-chart layout is retained.

## `z13ctl-docs/`

Copied from the former `z13ctl` checkout (v1.3.1) as implementation reference only. These files are documentation, never runtime dependencies or compatibility contracts.

| File | Source | Rendered at |
| --- | --- | --- |
| `commands.md` | `docs/commands.md` | <https://UHDbits.github.io/z13ctl/commands/> |
| `daemon.md` | `docs/daemon.md` | <https://UHDbits.github.io/z13ctl/daemon/> |
| `api.md` | `docs/api.md` | <https://UHDbits.github.io/z13ctl/api/> |
| `installation.md`, `getting-started.md`, `index.md` | `docs/` | — |
| `protocol.md` | `PROTOCOL.md` | — |
| `client.go`, `types.go` | `api/` | <https://UHDbits.github.io/z13ctl/api-reference/> is generated from these |

The live checkout is authoritative if these drift. When docs and code disagree, believe the code.
