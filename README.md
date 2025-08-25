<img width="1236" height="649" alt="image" src="https://github.com/user-attachments/assets/bc71300f-aa8e-472c-80c2-16b6e803bf09" />

# DCS TUI Dashboard

A lightweight terminal dashboard for Digital Combat Simulator (DCS) telemetry, built with Rust. It streams live aircraft data from DCS into a terminal user interface (TUI) using `ratatui`.

## Features

* Live telemetry display from DCS (position, IAS, TAS, Mach, attitude, acceleration).
* Engine/system stats: RPM, throttle, temps, fuel flow, nozzle %, manifold pressure (where available).
* Airframe stats: gear, flaps, airbrake, hook, wingsweep, WoW.
* Auto-hides values not exposed by the current module; missing values show as `---`.
* Real-time sparklines for IAS and altitude (scroll left-to-right, rightmost is latest value).
* Async UDP listener for high-frequency data (\~10 Hz).
* Clean TUI layout with `ratatui` and `crossterm`.
* Cross-platform and minimal dependencies.

## Telemetry Signals

Depending on the aircraft module, the exporter sends:

| Signal | Meaning                                                  |
| ------ | -------------------------------------------------------- |
| RPM %  | Engine spool speed as % of max                           |
| THR %  | Pilot throttle input (estimated from RPM if not exposed) |
| TEMP   | Exhaust/inter-turbine temperature                        |
| FF     | Fuel flow                                                |
| NOZ %  | Afterburner nozzle area (jets with AB)                   |
| MAP    | Manifold pressure / torque (props)                       |
| Gear   | Gear position (0–1)                                      |
| Flaps  | Flaps position (0–1)                                     |
| Airbrk | Airbrake position (0–1)                                  |
| Hook   | Tailhook position (carrier aircraft)                     |
| Wing   | Wing sweep (e.g., F-14)                                  |
| WoW    | Weight on wheels indicator                               |

Some aircraft will not provide all of these signals; the dashboard adapts accordingly.

## Architecture Overview

1. **DCS Export Script**: Custom `Export.lua` in your `Saved Games/DCS/Scripts` folder sends telemetry as JSON over UDP.
2. **Rust Backend**: Listens on UDP, parses JSON telemetry.
3. **TUI Frontend**: Displays live flight and systems data and graphs in terminal.

## Prerequisites

* [Rust](https://www.rust-lang.org/) (edition 2021 or later)
* Digital Combat Simulator with LuaSocket (shipped with DCS)
* Terminal with UTF-8 support

## Installation

Clone the repository and build:

```bash
git clone https://github.com/ASoldo/dcsctl.git
cd dcsctl
cargo build --release
```

## DCS Export.lua Setup

Create `Saved Games/DCS/Scripts/Export.lua` with the following (ensure `Scripts` folder exists):

```lua
-- Lua export code (see repository for full script)
-- Sends flight, engine, and mech telemetry as JSON via UDP to 127.0.0.1:5010 at 10 Hz.
-- Includes RPM, throttle, temps, fuel flow, nozzle %, MAP (if available), gear, flaps, airbrake, hook, wing, WoW.
```

Refer to the repository for the full script with normalization helpers.

## Usage

Run the Rust dashboard:

```bash
cargo run --release
```

By default it listens on `127.0.0.1:5010`. Override with `PORT` env var:

```bash
PORT=6000 cargo run --release
```

Launch DCS and start a mission. Telemetry should appear in the terminal with live updates.

### Controls

* `Ctrl+C`, `q`, or `Esc` to quit.

## Example Output

```
DCS Dash — Airframe: Su-25T   POS: 43.72221, 44.03159

IAS: 279.6 kt  ALT: 2999 m  Mach: 0.51
AoA: 0.00°  Pitch: 0.00°  Bank: 0.00°
RPM/Throttle/Systems panels, IAS & Altitude sparklines
```

## Development

Dependencies:

* [tokio](https://docs.rs/tokio/) for async networking
* [ratatui](https://ratatui.rs/) for TUI widgets
* [crossterm](https://crates.io/crates/crossterm) for terminal control
* [serde](https://serde.rs/) and [serde\_json](https://docs.rs/serde_json/) for parsing telemetry

### Adding More Telemetry

Extend the Lua export with additional `LoGet*` values and update Rust structs/UI rendering accordingly. Normalization helpers for engine and mech systems are already provided.

## License

MIT License. See [LICENSE](LICENSE) for details.
