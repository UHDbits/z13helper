# Build `z13helper` — a G-Helper-style native Linux GUI for z13ctl

## Mission

Build a new, native desktop GUI for controlling an **ASUS ROG Flow Z13 (2025, GZ302EA, Ryzen AI MAX "Strix Halo" APU)** on Linux. The app is a frontend for **z13ctl**, an existing Go CLI + daemon that owns all the hardware access (TDP/PPT power limits, fan curves, CPU undervolting, RGB lighting, battery charge limit, panel overdrive, performance profiles).

The app should feel like **G-Helper** — the well-liked Windows utility for ROG laptops — rather than like a generic settings panel. Two windows: a compact main window, and a separate "Fans + Power" window where custom profiles are built.

An existing GUI called **z13gui** already exists and works, but it is designed as a gamepad-navigable Wayland overlay drawer for Steam Gaming Mode, and it does not have the desktop-app ergonomics, named custom profiles, or AC/battery awareness this project needs. Treat z13gui as a reference implementation for *how to talk to z13ctl correctly*, not as a design to copy.

Work in the local checkout at `/home/uhdbits/Documents/z13gg`; the hosted repository is `UHDbits/z13helper`.

---

## Step 0 — Ask before you build

**Do not write code yet.** Ask one round of clarifying questions first, then proceed once answered. At minimum ask about the implementation language, since that decision cascades through everything else. Present the trade-offs honestly:

| Language | GUI binding | Pros | Cons |
| --- | --- | --- | --- |
| **Go** (recommended) | `gotk4` (`github.com/diamondburned/gotk4`) | Can import `github.com/UHDbits/z13ctl/api` directly — no protocol reimplementation, no drift. Same language as z13ctl and z13gui, so both reference codebases are directly reusable. Single static binary. | CGO build, slower compiles, GUI layer is hard to unit test. Go is **not currently installed** on this machine. |
| **Rust** | `gtk4-rs` + `libadwaita-rs` | Excellent, well-maintained GTK4 bindings; strong typing for a state machine like this; good async story with `zbus` for D-Bus. | Must reimplement the newline-delimited-JSON socket client (small — roughly 150 lines). Not currently installed. |
| **Python** | PyGObject | Fastest to iterate; PyGObject is first-class GTK4; already installed (3.14.6). | Runtime dependency, packaging is messier, no compile-time safety on a fairly stateful app. |
| **C / Vala** | native GTK4 | Smallest runtime footprint. | Slowest to write, most error-prone for the concurrency this app needs. |

Recommend **Go** unless the user prefers otherwise: importing the `api` package removes an entire class of protocol-drift bugs, and the two most relevant reference codebases are already Go.

Also worth asking in the same round:

1. **Should this replace z13gui or coexist with it?** This determines whether the app should subscribe to the Armoury Crate button `gui-toggle` event (two subscribers both toggling is fine at the daemon level, but two windows popping up is not).
2. **Preferred HUD mechanism** for the power-source-switch notification: a custom `gtk4-layer-shell` on-screen overlay like G-Helper's toast (recommended — matches G-Helper, works in a gaming session) versus a plain desktop notification via the notifications D-Bus portal (simpler, respects Do Not Disturb, but looks nothing like G-Helper).
3. **Final project/binary name** — `z13helper` is the repository and binary name.

Use the structured question tool rather than listing options in prose.

---

## Environment (verified on this machine)

Do not re-derive these; they were checked directly.

- Board: `GZ302EA`. `z13ctl` **v1.3.1** is installed and on `PATH`.
- `ryzen_smu` kernel module is **loaded**, so undervolting is available. (This is the `amkillam` fork requirement — Strix Halo needs it.)
- `gtk4` **4.22.4**, `libadwaita-1` **1.9.2**, `gtk4-layer-shell-0` **1.3.0** are all installed with dev headers.
- `powerprofilesctl` is installed, so power-profiles-daemon is present.
- `gcc` 16.1.1 and `pkg-config` are installed. **Go, Rust/Cargo, Vala, Meson, and Ninja are NOT installed** — factor the install step into the language recommendation and into setup instructions.
- The z13ctl daemon was **not running** at the time of the check (`$XDG_RUNTIME_DIR/z13ctl/z13ctl.sock` absent). Expect to start it with `systemctl --user enable --now z13ctl.socket z13ctl.service`, and expect users to hit this too — the app must degrade gracefully.

---

## Reference material

### Vendored in this repo

- `reference/screenshots/ghelper-main-window.png` — the target look for the main window.
- `reference/screenshots/ghelper-fans-and-power.png` — the target look for the Fans + Power window.
- `reference/z13ctl-docs/` — a snapshot of the z13ctl documentation and the public Go `api` package source (`client.go`, `types.go`). **Read `commands.md`, `daemon.md`, `api.md`, and `protocol.md` in full before designing anything.**

**Look at both screenshots.** They are the visual spec, and the layout details below assume you have seen them.

### Sibling checkouts on this machine

| Path | What it is | How to use it |
| --- | --- | --- |
| `/home/uhdbits/z13ctl` | The Go CLI + daemon. Read `CLAUDE.md`, `PROTOCOL.md`, `api/`, `internal/cli/{fan,tdp,undervolt,sysfs}.go`, `internal/daemon/{server,state}.go`. | **Authoritative source of truth** for every limit, format, and error mode. When docs and code disagree, believe the code. |
| `/home/uhdbits/z13gui` | The existing GTK4 (gotk4) overlay GUI. Read `CLAUDE.md` (excellent — a long list of GTK4/Wayland pitfalls already discovered the hard way), `internal/gui/tdp.go` (working Cairo fan-curve editor), `internal/daemon/daemon.go`, `internal/power/power.go`. | Reuse the *correct patterns*: socket-only access, daemon calls off the UI thread, the `handled`-flag error wrapper, testable logic split out of the GTK layer. Do not reuse its drawer/gamepad architecture. |
| `/home/uhdbits/g-helper` | The Windows C#/WinForms original. Read `app/Settings.cs` (main window), `app/Fans.cs` (Fans+Power window and the curve chart), `app/Mode/Modes.cs` and `app/Mode/ModeControl.cs` (the profile model and AC/battery switching), `app/AppConfig.cs`, `app/Helpers/ToastForm.cs` (the HUD). | The behavioral and visual spec. Copy its *interaction model*, not its code. |

### Upstream docs (mirrors of the vendored copies; fetch if you want the rendered versions)

- <https://UHDbits.github.io/z13ctl/commands/>
- <https://UHDbits.github.io/z13ctl/daemon/>
- <https://UHDbits.github.io/z13ctl/api/>
- <https://UHDbits.github.io/z13ctl/api-reference/>

---

## Architecture requirements

1. **All hardware access goes through the z13ctl daemon over its Unix socket.** Never write sysfs, never open hidraw, never shell out to the `z13ctl` binary. Socket path is `$XDG_RUNTIME_DIR/z13ctl/z13ctl.sock`, falling back to `/tmp/z13ctl/z13ctl.sock`. The wire format is newline-delimited JSON, one request object and one response object per connection, except `subscribe` which streams.

2. **Respect the `handled` convention.** Every `Send*` in the Go API returns `(handled bool, ..., err error)`. `(false, nil)` means *the daemon is not running* — it is not success. z13gui shipped a "dead button" bug by treating it as success; do not repeat that. Collapse the three-state result into one error type with a distinct `ErrDaemonNotRunning`, and show a persistent, actionable banner ("z13ctl daemon not running — `systemctl --user start z13ctl.service`") rather than failing silently. If you are not using Go, reproduce this semantics explicitly.

3. **Never block the UI thread on a daemon call.** The API bounds a call at 10 seconds. Snapshot widget values on the UI thread, do the socket I/O on a worker, marshal results back to the UI thread (`glib.IdleAdd` / equivalent). Never touch widgets from a worker.

4. **Keep testable logic out of the GUI layer.** Profile modelling, the apply algorithm, curve validation and monotonic enforcement, config load/save/migration, and power-source debouncing should all live in pure modules with real unit tests. The GTK layer should be thin. z13gui found seven bugs the moment it did this extraction; do it from the start.

5. **The daemon is the source of truth for hardware state; your config file is the source of truth for profile definitions.** On window show and on a ~1 Hz timer while visible, call `get-state` and reconcile. Stop polling when the window is hidden.

6. **Reading back the active profile takes two calls.** `profile-get` reads sysfs `platform_profile` and therefore returns the **stock base** (`quiet`/`balanced`/`performance`) — it never returns `custom`. `get-state.profile` returns the daemon's **effective** profile and *does* return `custom` once any override is applied. You need both: the former tells you which base is active, the latter tells you whether overrides are live. This matters more than it sounds, because under the design in constraint 4 the app is normally on a stock base and `custom` simultaneously.

7. **Graceful degradation** is a first-class requirement, not an afterthought. The app must stay usable and honest when: the daemon is down, `ryzen_smu` is absent (hide/disable undervolting — use `get-state.undervolt_available`, and **never** call the SMU probe yourself, it is destructive and equivalent to an undervolt reset), power-profiles-daemon is absent (z13ctl no-ops the PPD call, so nothing breaks — just do not claim a PPD profile was set), or the panel-overdrive firmware attribute is missing.

---

## Critical constraints — read these carefully

These are the sharp edges. Getting any of them wrong produces a subtly broken app.

### 1. z13ctl's `custom` profile is a single global slot, not a profile system

z13ctl knows exactly four profiles: `quiet`, `balanced`, `performance`, and a **virtual** `custom`. `custom` is never written to `platform_profile`; it is a marker meaning "the daemon's saved fan curve / TDP / undervolt are the active ones." There is exactly one saved set.

Your named custom profiles ("Gaming", "Battery Saver", …) are therefore **purely a GUI-side concept**. You store their definitions in your own config, and applying one means pushing its values into z13ctl's single `custom` slot. Design the data model with this clearly in mind and document it, or you will confuse yourself later.

### 2. Switching to a stock profile wipes custom state in hardware

`profile --set quiet|balanced|performance` resets fan curves to firmware auto, resets the undervolt to stock, and writes that profile's stock PPT values. The saved values survive in daemon state for later recall, but they stop being applied.

This dictates the ordering in your apply algorithm: **set the stock base profile first, then layer the overrides on top.** Never the reverse.

### 3. Apply TDP *before* the fan curve, always

Above 75 W sustained (PL1), z13ctl clamps both fans to a 204 PWM (80%) floor:

- Raising PL1 above 75 W requires `force: true`, and z13ctl writes the 80% floor curve *before* the power limit, failing closed if the fan write fails.
- While PL1 > 75 W, a `fancurve` set containing **any** point below 204 PWM is rejected, and `fancurve-reset` is refused outright.
- `tdp-reset` is the escape hatch — it lowers power first, then releases the fans.

So: always send TDP first, then the fan curve. And in the curve editor, **clamp the minimum to 204 PWM (80%) whenever the profile's PL1 exceeds 75 W**, drawing a visible floor line and explaining why, so the user cannot author a curve that will be rejected. z13gui already does this; see its `enforceCurve` logic.

### 4. Let z13ctl drive power-profiles-daemon — do not manage PPD yourself

`z13ctl`'s `SetProfile` calls `powerprofilesctl set` itself after a successful stock-profile write, using a hardcoded mapping (`/home/uhdbits/z13ctl/internal/cli/sysfs.go`):

| z13ctl base profile | PPD profile it sets |
| --- | --- |
| `quiet` | `power-saver` |
| `balanced` | `balanced` |
| `performance` | `performance` |
| `custom` | *(no PPD call — the virtual profile never reaches `SetProfile`)* |

The important part, and the reason this is simple rather than a hazard: **the TDP, fan curve, and undervolt handlers never call `SetProfile`.** They set `d.state.Profile = "custom"`, which is only an in-memory marker used to interpret PPT readback and to drive resume/startup restore. Verified in `/home/uhdbits/z13ctl/internal/daemon/server.go` — `handleFanCurve`, `handleUndervolt`, and the TDP handler each do `d.state.Profile = "custom"` and nothing more. `platform_profile` in sysfs keeps whatever stock value you last wrote, and PPD is never re-touched.

So you can freely apply custom TDP, fan curves, and undervolt **while staying on a stock base profile**, and the PPD profile chosen in step 1 of the apply sequence simply persists.

**Therefore: the base profile and the PPD profile are one control, not two.** A GUI profile stores a single `base` of `quiet`, `balanced`, or `performance`; the user picks it knowing the PPD profile it implies, and the dropdown label should show both (e.g. `Balanced — PPD: balanced`). Apply it first, then layer the overrides. Do not add a D-Bus PPD client to "correct" the value afterwards — there is nothing to correct, and a second writer would only introduce races with z13ctl.

Two consequences to note in the UI or README rather than engineer around:

- Off-diagonal combinations are not reachable. You cannot pair `platform_profile = performance` with PPD `power-saver`. This is a deliberate trade for having a single coherent control, and the coupling matches what these profiles mean anyway.
- If PPD lacks a profile in the mapping (some systems have no `performance`), z13ctl ignores the failure silently — `_ = exec.Command(bin, "set", ppd).Run()` — and PPD stays where it was. A **read-only** check of PPD's `ActiveProfile` after applying, used only to display the true state, is a reasonable optional nicety. Do not turn it into a write.

### 5. Undervolt is volatile and conditional

Range is **0 to −40** (all-core CPU Curve Optimizer), integers only. There is **no sysfs readback** — the daemon's saved state is the only source. It resets on every reboot and every sleep/resume, and the daemon only reapplies it when the `custom` profile is active. `undervolt-get` returns `{"cpu_co": int, "active": bool, "profile": string}` — use `active` to distinguish "applied right now" from "saved for later." iGPU Curve Optimizer is not supported on Strix Halo; do not expose it.

### 6. Fan curves: exactly 8 points, applied to both fans

The wire format is 8 comma-separated `temp:speed` pairs, e.g. `48:2,53:22,57:30,60:43,63:56,65:68,70:89,76:102`. Speed is PWM 0–255, or a percentage with a `%` suffix. Validation: exactly 8 points; temperatures strictly increasing, 0–120 °C; speeds monotonically non-decreasing. Both physical fans cool the same APU and always receive the same curve — **the Fans + Power window has one chart, not two.** `pwm_enable` mode values are `0` = full speed, `1` = custom, `2` = firmware auto.

### 7. TDP ranges

`pl1_spl` (sustained) is 5–75 W by default and 5–93 W with `force`. `pl2_sppt` (short boost) and `fppt` (fast boost) go to 93 W without `force`. `apu_sppt` and `platform_sppt` mirror PL2 automatically — do not expose them. Enforce the ordering PL1 ≤ PL2 ≤ PL3 in the sliders, the way G-Helper does.

Stock PPT values, useful as the defaults for new profiles derived from each base:

| Base | PL1 | PL2 | PL3 |
| --- | --- | --- | --- |
| `quiet` | 40 | 55 | 55 |
| `balanced` | 52 | 71 | 70 |
| `performance` | 70 | 86 | 86 |

Note that raw `tdp --get` readback is the kernel's cached value and holds a stale 5 W default after a fresh boot; prefer `get-state`, which substitutes the measured per-profile table.

### 8. Lighting device mapping

z13ctl exposes two RGB zones as separate HID devices: `keyboard` (the detachable folio, `0b05:1a30`) and `lightbar` (the rear light in the tablet body, `0b05:18c6`). Modes are `static`, `breathe`, `cycle`, `rainbow`, `strobe`; speeds are `slow`/`normal`/`fast`; brightness is an integer 0–3. Colors are `RRGGBB` hex without `#`.

**Gotcha:** `color 000000` does not mean black — it sets the Aura random-color flag and the firmware picks a color. To turn lighting off, use the `off` command or brightness 0.

There is no G-Helper-style "Slash Lighting" on this machine. Map the screenshot's Slash Lighting row onto the **lightbar** instead.

---

## Feature specification

### A. Main window

Model it on `reference/screenshots/ghelper-main-window.png`: a narrow single-column window (G-Helper is roughly 850 px wide at 192 DPI; scale sensibly for GTK), dark by default, made of stacked sections. Each section has a header row with a small icon, a bold left-aligned title that includes live state, and a right-aligned muted status label. Below each header sits a grid of large, equal-width buttons with the icon above the label. The active button is marked with a colored border and a subtle tint using a per-mode accent color — Silent green `#06B48A`, Balanced blue `#3AAEEF`, Turbo red `#FF2020`, Custom orange `#FF8000`. Use libadwaita so it follows the system light/dark preference, but keep G-Helper's button-grid character rather than defaulting to a plain preferences list.

Sections, top to bottom:

1. **Performance Mode.** Header reads `Mode: <profile name>`, with G-Helper's suffix convention: append `+` when a custom fan curve is actively applied and ` <N>W` when a custom power limit is applied — e.g. `Mode: Balanced+ 20W`. Right-aligned live telemetry: `APU: 44°C   Fan: 0 RPM`, refreshed about once a second from `get-state` while the window is visible.

   Button grid: **Silent**, **Balanced**, **Turbo**, **Fans + Power** on the first row, exactly as in the screenshot. User-created custom profiles flow into additional rows of the same grid, each highlighted in its base mode's accent color when active. Clicking a profile applies it. Clicking **Fans + Power** opens the second window and does *not* change the active profile.

2. **Display.** A single toggle row for **Panel Overdrive** (`paneloverdrive`). Nothing else — no refresh-rate buttons, no gamma or visual-mode controls.

3. **Lightbar** (the screenshot's Slash Lighting row) — mode dropdown, color picker, brightness. Targets `device: "lightbar"`.

4. **Laptop Keyboard** — mode dropdown, color picker, brightness, and a speed control for the animated modes. Targets `device: "keyboard"`. Hide the color and speed controls for modes that ignore them, the way z13ctl documents per-mode applicability.

5. **Battery Charge Limit.** Header shows `Battery Charge Limit: 80%` with a right-aligned live charge status. Slider from 40 to 100 in steps of 5, plus a quick `100%` button that removes the limit. **Debounce writes** — do not fire a daemon call per slider tick.

6. **Footer.** Version label and a **Quit** button. A **Boot Sound** toggle (`bootsound`, 0/1) is a reasonable extra to tuck in here.

**Explicitly excluded** (the user does not want these): GPU Mode, Flicker-free Dimming / Visual Mode, refresh-rate switching, and "Run on Startup" — the first two do not apply to this machine, and startup is handled by systemd on Linux.

### B. Fans + Power window

Model it on `reference/screenshots/ghelper-fans-and-power.png`. A separate window, wider than the main one, opened from the **Fans + Power** button and positioned adjacent to the main window like G-Helper does. Split into a narrower left column of sliders and a wider right column holding the profile selector and the fan chart.

**Tabs (left column):** **CPU** and **Advanced** only. **There is no GPU tab** — this machine is an APU.

**Profile selector (top-right, above the chart):** a dropdown listing all profiles, with a `+` button to create, a rename affordance, and a `−` button to delete. Follow G-Helper's model: creating a profile copies every setting from the currently selected one and names it `Custom N`; built-in profiles cannot be deleted or renamed. Selecting a profile here loads it into the editors and makes it active.

**CPU tab (left column):**

- **Power Profile** dropdown — this replaces G-Helper's "Windows Power Mode" and "CPU Boost" controls, both of which are gone. It is the profile's `base`, and because z13ctl derives the power-profiles-daemon profile from it (constraint 4), it selects both at once. Three entries, labeled with the PPD profile each implies: `Quiet — PPD: power-saver`, `Balanced — PPD: balanced`, `Performance — PPD: performance`. Changing it re-runs the apply sequence, which resets the hardware to that base's stock state before re-layering this profile's overrides.
- **Power Limits** — three sliders, matching the screenshot's labels: `SPL (CPU sustained)` → `pl1_spl`, `sPPT (CPU long boost)` → `pl2_sppt`, `fPPT (CPU short boost)` → `fppt`. Each shows its wattage. Enforce PL1 ≤ PL2 ≤ PL3. Sending PL1 above 75 W requires `force: true` and a clear warning in the UI about the 80% fan floor it imposes. Apply on slider release, not on every motion event.
- **Apply Power Limits** checkbox — when unchecked, this profile leaves PPT at its base profile's stock values.

**Advanced tab (left column):**

- **CPU Undervolt (Curve Optimizer)** — a slider from 0 to −40, integer steps, with the current value displayed. Include an **Apply Undervolt** checkbox parallel to the power-limits one. Show a clear explanation that the offset only takes effect on the custom profile and is reapplied automatically after sleep by the daemon. If `undervolt_available` is false, disable the control and explain that `ryzen_smu` (the `amkillam` fork) is not loaded.
- A **Factory Defaults** button belongs here or in the footer, resetting the current profile to its base's stock values.

**Fan chart (right column):** a **single** chart labeled something like "Fan Curve (both fans)".

- X axis: temperature, 20–110 °C, gridlines every 10.
- Y axis: fan speed as a percentage 0–100, with `OFF` rendered at zero. Optionally allow clicking the axis to toggle to an RPM display, as G-Helper does — but note z13ctl exposes no maximum-RPM constant, so any RPM figure is an estimate; percentage should be the default and the honest one.
- 8 draggable points. Hovering highlights a point and shows a tooltip like `65°C, 68%`. Dragging moves a point; **Shift+drag** moves the whole curve vertically. Dragging one point pushes its neighbors so the curve stays monotonically non-decreasing on both axes.
- **Clamp to Grid** checkbox (default on): locks point *i*'s temperature into the band `[30 + i*10, 30 + i*10 + 9]`, exactly as G-Helper does.
- Keyboard accessible: Tab between points, arrows to nudge by 1, Shift+arrows by 5. Do not ship a mouse-only editor.
- Draw the 80% floor line and clamp editing to it when the profile's PL1 exceeds 75 W.
- **Apply Custom Fan Curve** checkbox: when unchecked, the curve renders muted and the profile leaves fans in firmware auto mode.

`/home/uhdbits/z13gui/internal/gui/tdp.go` has a working Cairo implementation of most of this, including hit-testing and constraint enforcement — read it before writing your own.

### C. Profiles: data model and apply algorithm

Persist to `$XDG_CONFIG_HOME/z13helper/config.json` (default `~/.config/z13helper/config.json`). Write atomically via temp-file-plus-rename, debounce writes, keep a `.bak`, and include a `version` field. Something along these lines:

```json
{
  "version": 1,
  "active_profile": "balanced",
  "auto_switch_on_power_source": true,
  "last_profile_on_ac": "turbo",
  "last_profile_on_battery": "silent",
  "power_source_debounce_ms": 2000,
  "show_hud": true,
  "fan_clamp_to_grid": true,
  "profiles": [
    {
      "id": "silent",
      "name": "Silent",
      "builtin": true,
      "base": "quiet",
      "apply_power_limits": false,
      "pl1_spl": 40,
      "pl2_sppt": 55,
      "fppt": 55,
      "apply_fan_curve": false,
      "fan_curve": [[48,2],[53,22],[57,30],[60,43],[63,56],[65,68],[70,89],[76,102]],
      "apply_undervolt": false,
      "cpu_co": 0
    }
  ]
}
```

There is no separate `ppd_profile` field: `base` determines it, per constraint 4.

Ship three built-ins — Silent (`quiet`), Balanced (`balanced`), Turbo (`performance`) — with all override flags off, so out of the box the app behaves exactly like plain z13ctl.

**Applying a profile** must follow this order, and the order is not negotiable (see constraints 2, 3, and 4):

1. `SendProfileSet(profile.base)` — establishes the firmware baseline and stock PPT, and sets the matching PPD profile. It also resets fans to firmware auto and the undervolt to stock, which is exactly why it must come first. **Run this unconditionally, even when the base is unchanged from the previously active profile** — it is what clears the outgoing profile's overrides, so skipping it as an optimization leaves stale settings applied.
2. If `apply_power_limits`: `SendTdpSet(pl1, pl1, pl2, pl3, force = pl1 > 75)`. **TDP before fans.**
3. If `apply_fan_curve`: `SendFanCurveSet(curve)`. Skip if the curve would be rejected by the 80% floor and surface a clear error rather than a silent no-op.
4. If `apply_undervolt` and `undervolt_available`: `SendUndervoltSet(cpu_co)`.
5. Update the header label, the button highlight, and persist `active_profile` plus the per-power-source key.

Steps 2 through 4 flip the daemon's effective profile to `custom` without disturbing `platform_profile` or PPD, so the base selected in step 1 stays in force throughout. Nothing needs to run after them to defend the PPD choice.

Run the whole sequence on a worker with a single in-flight guard, so rapid profile clicking cannot interleave two sequences. Report a failure at any step in the error banner and leave the UI reflecting reality rather than intent.

### D. AC / battery auto-switching with HUD

Mirror G-Helper's behavior (`Program.SchedulePowerCheck` → `ModeControl.AutoPerformance`, and `Modes.SetCurrent`, which writes both `performance_mode` and `performance_{0|1}`).

- **Remember per power source.** Every time the user selects a profile, record it under `last_profile_on_ac` or `last_profile_on_battery` depending on the current source.
- **Detect the power source** via UPower on D-Bus: the `OnBattery` property of `org.freedesktop.UPower`, watched through `PropertiesChanged`. Fall back to polling `/sys/class/power_supply/*/online` only if UPower is unavailable. Do not poll as the primary mechanism.
- **Debounce** transitions by about 2 seconds (configurable). Plugging and unplugging generates chatter, and a real switch is expensive.
- **On a confirmed transition,** apply the remembered profile for the new source and show the HUD. Skip both if the profile is already active. Honor an `auto_switch_on_power_source` setting so users can turn the whole thing off.
- **HUD:** a G-Helper-style toast — bottom-center, roughly 300×100, semi-transparent dark rounded rectangle, large bold text with the profile name, an AC-plug or battery glyph, auto-dismissing after ~2 seconds, click-through so it never steals focus. Implement as a `gtk4-layer-shell` overlay surface on Wayland (1.3.0 is installed); fall back to a desktop notification where layer-shell is unavailable. Make it suppressible via `show_hud`.
- Also show the HUD when a profile is switched by any non-obvious means (a future hotkey or tray action), but **not** when the user clicks a button in the main window — the button highlight is already the feedback. This matches G-Helper's `notify` flag convention.

---

## Non-goals

Do not build: GPU mode switching, dGPU controls, or anything XG Mobile related (this is an APU-only machine); refresh-rate or gamma/visual-mode controls; Anime Matrix; gamepad navigation or a gamescope/Steam Gaming Mode overlay (that is z13gui's job); a hardware/FPS overlay; per-key RGB; Windows-specific concepts like a startup checkbox or Windows power modes.

Optional stretch goals, only if the core is solid and only if the user confirmed it in Step 0: a system tray icon with profile switching, `gui-toggle` subscription for the Armoury Crate button, and global hotkeys for cycling profiles.

---

## Quality bar and deliverables

- **Idiomatic, well-factored code** in the chosen language, with the pure logic (profiles, apply sequencing, curve math and validation, config migration, debouncing) separated from the GTK layer and covered by real unit tests. Aim for meaningful coverage of the pure modules; the GTK layer will not be unit-testable and that is fine.
- **A dependency manifest with pinned versions** appropriate to the language (`go.mod`, `Cargo.toml`, `pyproject.toml`), and a `Makefile` with `build`, `run`, `test`, `lint`, and `install` targets.
- **A `.desktop` file** and an icon, installed under the XDG data dirs.
- **A README** covering what the app does, prerequisites (z13ctl installed, `z13ctl setup` run for group permissions, daemon enabled, `ryzen_smu` for undervolting), build and install steps, the profile model, and an explanation of why the Power Profile dropdown sets both the ASUS platform profile and the PPD profile together (constraint 4) — users will otherwise wonder why they cannot pick them independently.
- **An architecture note** (`CLAUDE.md` or `ARCHITECTURE.md`) in the style of the ones in the two sibling repos, recording the threading rules, the apply ordering and why, and any GTK4 pitfalls you hit. `/home/uhdbits/z13gui/CLAUDE.md` has a "do not re-introduce" list of GTK4/Wayland bugs — read it first, and carry forward the ones that still apply.
- **Sensible commits** as you go. Do not dump the whole project in one commit.

## Suggested milestones

Verify each one actually runs against the real daemon before moving on. Do not build the entire app and then test it once at the end.

1. Project skeleton, build system, daemon client with the `handled` semantics, connectivity smoke test against the live daemon.
2. Config layer: schema, load/save/migrate, built-in profile seeding, unit tests.
3. Main window shell: sections, button grid, active-state styling, live telemetry polling, error banner.
4. Main window controls wired: profile apply, panel overdrive, both lighting zones, battery limit.
5. Fans + Power window: tabs, profile dropdown with create/rename/delete, the base/Power Profile dropdown, power sliders, undervolt.
6. Fan curve editor: rendering, drag, monotonic enforcement, clamp-to-grid, keyboard access, the 80% floor.
7. Power-source detection, per-source memory, auto-switch, HUD.
8. Packaging, docs, polish.

## Acceptance checklist

- [ ] Every hardware operation goes through the daemon socket; no sysfs writes, no hidraw, no shelling out to `z13ctl`.
- [ ] A daemon that is not running produces a clear, actionable banner — never a silently dead control.
- [ ] The UI never freezes; no daemon call runs on the UI thread.
- [ ] Applying a profile follows the required order. Verify empirically: select a profile with a custom TDP, fan curve, and undervolt, then confirm `powerprofilesctl get` still reports the profile implied by its base, `z13ctl profile --get` reports that base, and `z13ctl status` shows the custom TDP and fan curve actually applied. All four must hold at once.
- [ ] Switching between two profiles that share a base but differ in overrides fully clears the outgoing profile's settings — no stale fan curve or undervolt left applied.
- [ ] Custom profiles can be created, renamed, deleted, and persist across restarts.
- [ ] The fan curve editor cannot produce a curve that z13ctl will reject, including under the >75 W PL1 floor.
- [ ] Unplugging switches to the remembered battery profile and shows the HUD; plugging in switches back. Rapid plug/unplug does not thrash.
- [ ] Undervolt controls disable themselves cleanly when `ryzen_smu` is absent, and the app does not claim a PPD profile was applied when power-profiles-daemon is not installed.
- [ ] The main window is recognizably G-Helper-like next to `reference/screenshots/ghelper-main-window.png`.
- [ ] `make test` passes and `make lint` is clean.
