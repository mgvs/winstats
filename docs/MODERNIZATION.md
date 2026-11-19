# Battery & power popup, popup styles

## Architecture (as found)

- **Stack:** pure Rust (toolchain pinned to 1.77), `windows` 0.58 bindings, no UI framework.
  Everything is drawn by a small software rasteriser (`src/canvas.rs`: premultiplied BGRA,
  antialiased primitives, GDI-rendered text) and presented through `UpdateLayeredWindow` or a
  DWM-backed window.
- **Popups:** `popup.rs` — one top-level window using Windows 11 acrylic/Mica (`backdrop.rs`)
  with a self-drawn layered fallback, per-monitor DPI scaling, light/dark. Content is laid out
  by the `Ui` helper (rows, sections, charts, hit-testing, tooltips).
- **Telemetry:** `metrics/` polled from a `WM_TIMER` on the UI thread; slow sources use worker
  threads.
- **Config:** `%APPDATA%\winstats\config.toml` via serde with `#[serde(default)]`, so new
  fields are backward compatible. 29 locales, a test enforces identical key sets.
- **Licensing:** MIT; no new dependencies.

The existing renderer, backdrop and popup system were extended, not replaced.

## What this adds

| Area | Files |
|---|---|
| Battery telemetry worker (battery class IOCTLs, `GetSystemPowerStatus`, energy saver, 7 days of history persisted to `battery_history.csv`) | `src/metrics/power.rs` |
| Energy Meter Interface reader (RAPL / platform rails, no admin needed) | `src/metrics/emi.rs` |
| Power-flow model: measured vs calculated vs unavailable, no double counting | `src/metrics/powerflow.rs` |
| Learned system-power model for AC, display state | `src/metrics/powermodel.rs`, `src/metrics/displays.rs` |
| Standby drain on battery | `src/metrics/sleepdrain.rs` |
| Screen-off timeout and keep-awake | `src/metrics/quick.rs` |
| Slow reads of the battery popup off the UI thread (screen timeouts, GPU power states) | `src/metrics/controls.rs` |
| Battery popup: Sankey power-flow diagram, readings, history charts | `src/popup_battery.rs` |
| Battery and consumption charts in their own resizable window | `src/popup_chart.rs` |
| Shared popup components (segmented buttons) | `src/popup_cards.rs` |
| Popup styles | `src/style.rs` |
| Canvas: ribbons with travelling pulses, dashed curves, area charts, region save/restore, Segoe UI Variable | `src/canvas.rs` |
| Partial-redraw animation timer (only the band rectangle is redrawn) | `src/popup.rs`, `src/main.rs` |
| Config: `animations`, `animation_mode`, `style`, `battery_history_hours` | `src/config.rs` |
| Strings: `[power]` section and new `[menu]` entries | `locales/*.toml` |

### Data honesty rules (`powerflow.rs`, unit-tested)

- Battery power = fuel-gauge rate (**measured**). Current = rate / voltage (**calc.**).
- On battery: system draw *is* the discharge rate — one measurement, counted once.
- On AC: system draw only from a platform energy-meter rail (PSYS/SYS). Adapter input is then
  battery + system (**calc.**). Without such a rail the learned estimate below stands in,
  clearly marked, else “— not reported”.
- CPU-package (RAPL PKG) power is shown as its own row and never used as system power.
- Time to full is an estimate from capacity and rate.

### Resource use

- The worker samples every `update_ms` while the battery (or CPU) popup is open, every 10 s
  otherwise (30 s with energy saver on). No battery and no energy meter → no worker.
- Animation only while the popup is visible and something actually flows, and only when
  animations are allowed (menu: Auto follows Windows' “Animation effects”, or On / Off).
- About 64 fps (one frame per 15.6 ms timer tick). Each band keeps its own pulse position
  (`FlowMotion`), so a new reading changes the speed, not the position; a change of width
  glides over 0.6 s. The finished window under the bands is kept; a frame copies it back and
  draws the bands straight into it. The cost per frame is logged every 5 s as `anim:`.
- Per-process sampling runs only for the popups that list processes, so the once-a-second
  refresh does not stall the flow animation. Rendered texts are cached per (text, font).

## The battery popup

- **Names in the flow:** CPU cores (P/E), Graphics (iGPU), discrete GPU (its board power only
  while it is awake and busy, so a sleeping GPU is never woken), Chip rest. Each card has a
  tooltip saying what it holds and whether it is measured, calculated or learned.
- **Learned split of the rest:** the model (`powermodel::Parts`) gives the lit screens' terms,
  the conversion losses and its base. Once it has learned, "Rest of the system" becomes Screens
  (estimated), Power conversion (estimated) and Other system (the remainder up to the measured
  total, so the parts add up).
- **Quick settings:** keep awake (`SetThreadExecutionState` on the UI thread; not saved) and
  screen off after, plugged in / on battery (`VIDEOIDLE` of the active power plan via powrprof,
  no elevation).
- **Standby on battery** (`metrics/sleepdrain.rs`): a pause of 2 min or more between ticks, on
  battery at both ends, counts as a standby. The capacity comes from the fuel gauge once it
  reads fresh after waking. Saved to `sleep_log.txt` (last 30).
- **History:** battery level and consumption charts with a 1 / 6 / 24 h range. A click on a
  chart opens the charts in their own resizable window with 1 h … 7 d and "Since unplugged",
  plus figures for the range: average and highest discharge, energy used, battery % used, and a
  full charge's runtime at that average. History is kept for 7 days, saved every 10 minutes.

## System power on AC (passive model)

Many notebooks expose no adapter or system power while plugged in: no ACPI power meter
(`_PMM`), no power-source info (`_PIF`), no PSYS energy meter. The development machine is one
of them. So the System card on AC shows a stand-in, clearly marked:

1. **Learned estimate** (`src/metrics/powermodel.rs`): on battery the gauge measures the whole
   machine, which trains
   `system ≈ base + k·CPU package + Σ lit panels (a_i + c_i · brightness · picture_i)`.
   - Learns only steady stretches: 90 s settle after any change, then 60 s unchanged; at most
     one point per 30 s.
   - Remote Desktop: valid only while the console screen is off (panels dark).
   - Offers an estimate only once every known panel was seen both lit and dark; the ± is the
     error measured on each sample *before* it is learned.
   - Saved in `%APPDATA%\winstats\power_model.txt` (readable, includes θ).
2. **Last on-battery reading** (`last_on_battery.txt`) when no estimate is ready yet.

Display state comes from `src/metrics/displays.rs`: active built-in panels (Display Config
API), console display on/off/dimmed (power notification), brightness (power scheme), picture
level (32 × 20 halftoned screen copy, lit panels only, never over RDP).

### Log for review: `%APPDATA%\winstats\power_log.csv`

One row per worker sample (10 s, faster while the battery/CPU popup is open); rolls over to
`power_log.1.csv` at 20 MB.

| Column | Meaning |
|---|---|
| `unix` | sample time |
| `on_ac`, `remote`, `console`, `valid` | power source; RDP session; console 0 off / 1 on / 2 dim; sample usable |
| `level`, `rate_w` | battery charge 0..1; gauge rate (+ charging) |
| `batt_w` | machine draw from the battery (on battery only) — the training target |
| `pkg_w`, `pp0_w`, `pp1_w`, `dram_w` | RAPL CPU package, cores, graphics, DRAM |
| `brightness`, `external`, `panels` | brightness %; external displays; `id@x,y:picture` per active panel |
| `lit0`, `lit1` | model slots: `off` or picture level |
| `est_w`, `est_err_w`, `status`, `n` | model output, earned error, readiness, learned points |
| `pkg_avg_w` | CPU package averaged over 30 s (time-weighted), what the battery popup sets against the gauge |

The 30 s window was measured, not assumed: on the development machine's first day of log, the
gauge's discharge rate correlates best with the CPU package averaged over 20–45 s (r = 0.47 vs
0.23 instant; spread of the remainder ±4.1 W vs ±5.1 W); the gauge updates every 10 s. Logs
written before this column existed are kept as `power_log.v1.csv`.

## Popup styles and layout

- **Styles** (`src/style.rs`, Settings → Style): presets for the strip and the popups (popup
  tint, card fill and outline, module colours, chart fill, dim text). Popup charts use
  `Canvas::area_chart` (an antialiased line over a fill that fades towards the bottom); taskbar
  widgets keep their pixel-exact `line_chart`.
- **Scrolling (all popups):** a popup taller than the screen stops at the screen; the mouse
  wheel scrolls the content, and a thin scroll bar shows in the right padding. A popup that
  would not fit shows fewer process rows first.

## Build

```
rustup (any) — rust-toolchain.toml installs 1.77 automatically
Visual Studio 2022 Build Tools, "Desktop development with C++" workload
cargo build --release          # target\release\winstats.exe, portable, static CRT
cargo test                      # unit tests
cargo test live_probe -- --ignored --nocapture        # dump this machine's battery/EMI data
cargo test battery_previews -- --ignored              # render popup previews to target\previews
cargo test style_previews -- --ignored                # the battery and CPU popups in every style
```

## Not yet done

- Translation of the new `[power]` and `[menu]` strings (most locales carry English for now).
