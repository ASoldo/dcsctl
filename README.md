<img width="702" height="220" alt="image" src="https://github.com/user-attachments/assets/692c3cdb-02ce-4270-be77-7629e57fd8e2" />

# DCS TUI Dashboard

A lightweight terminal dashboard for Digital Combat Simulator (DCS) telemetry, built with Rust. It streams live aircraft data from DCS into a terminal user interface (TUI) using `ratatui`.

## Features

* Live telemetry display from DCS (position, IAS, TAS, Mach, attitude, acceleration).
* Real-time sparklines for IAS and altitude.
* Async UDP listener for high-frequency data.
* Clean TUI layout with `ratatui` and `crossterm`.
* Cross-platform and minimal dependencies.

## Architecture Overview

1. **DCS Export Script**: Custom `Export.lua` in your `Saved Games/DCS/Scripts` folder sends telemetry as JSON over UDP.
2. **Rust Backend**: Listens on UDP, parses JSON telemetry.
3. **TUI Frontend**: Displays live flight data and graphs in terminal.

## Prerequisites

* [Rust](https://www.rust-lang.org/) (edition 2021 or later)
* Digital Combat Simulator with LuaSocket (shipped with DCS)
* Terminal with UTF-8 support

## Installation

Clone the repository and build:

```bash
git clone https://github.com/yourusername/dcs-tui-dashboard.git
cd dcs-tui-dashboard
cargo build --release
```

## DCS Export.lua Setup

Create `Saved Games/DCS/Scripts/Export.lua` with the following:

```lua
package.path = package.path .. ";.\\LuaSocket\\?.lua;.\\LuaSocket\\?\\init.lua"
package.cpath = package.cpath .. ";.\\LuaSocket\\?.dll"

local has_socket, socket = pcall(require, "socket")
local udp = nil

-- TX rate
local HZ = 10
local DT = 1.0 / HZ
local lastSent = 0

-- ---- small helpers ----
local function log(msg)
	pcall(function()
		net.log("[EXPORT] " .. tostring(msg))
	end)
end

local function jnum(x)
	if type(x) ~= "number" or x ~= x or x == math.huge or x == -math.huge then
		return "null"
	end
	return string.format("%.6f", x)
end

local function jsonify(tbl)
	local parts = {}
	for k, v in pairs(tbl) do
		local key = '"' .. k .. '"'
		local val
		if type(v) == "string" then
			val = '"' .. v:gsub("\\", "\\\\"):gsub('"', '\\"') .. '"'
		elseif type(v) == "number" then
			val = jnum(v)
		elseif type(v) == "table" then
			local sub = {}
			for kk, vv in pairs(v) do
				local subval = (type(vv) == "number") and jnum(vv) or "null"
				table.insert(sub, '"' .. kk .. '":' .. subval)
			end
			val = "{" .. table.concat(sub, ",") .. "}"
		else
			val = "null"
		end
		table.insert(parts, key .. ":" .. val)
	end
	return "{" .. table.concat(parts, ",") .. "}"
end

-- ---- required DCS hooks ----
function LuaExportStart()
	-- called when a mission starts loading
	log("LuaExportStart()")
	if has_socket then
		udp = socket.udp()
		udp:settimeout(0)
		-- adjust target if your dashboard runs elsewhere
		udp:setpeername("127.0.0.1", 5010)
		log("UDP armed 127.0.0.1:5010 @ " .. HZ .. " Hz")
	else
		log("LuaSocket missing")
	end
	lastSent = 0
end

function LuaExportBeforeNextFrame()
	-- not used here, but defined because some tools expect the symbol
end

function LuaExportAfterNextFrame()
	-- called every frame while in a mission
	local t = LoGetModelTime()
	if not t or (t - lastSent) < DT then
		return
	end
	lastSent = t

	local self = LoGetSelfData() or {}
	local LLA = self.LatLongAlt or {}
	local pitch, bank, yaw = LoGetADIPitchBankYaw()

	local accel = LoGetAccelerationUnits() or {}

	local payload = {
		t = t,
		name = self.Name or "",
		lat = LLA.Lat,
		lon = LLA.Long,
		alt_msl = LLA.Alt,
		alt_agl = LoGetAltitudeAboveGroundLevel(),
		ias_ms = LoGetIndicatedAirSpeed(),
		tas_ms = LoGetTrueAirSpeed(),
		mach = LoGetMachNumber(),
		aoa_rad = LoGetAngleOfAttack(),
		vv_ms = LoGetVerticalVelocity(),
		att = { pitch = pitch, bank = bank, yaw = yaw },
		accel = { x = accel.x, y = accel.y, z = accel.z },
	}

	if udp then
		udp:send(jsonify(payload) .. "\n")
	end
end

function LuaExportStop()
	-- called when leaving a mission
	log("LuaExportStop()")
	if udp then
		udp:close()
		udp = nil
	end
end

function LuaExportActivityNextEvent(t)
	-- schedule next activity tick; not strictly needed since we use AfterNextFrame,
	-- but returning something sensible keeps other scripts happy.
	return (t or 0) + DT
end
```

Ensure `Scripts` folder exists; create it if necessary.

## Usage

Run the Rust dashboard:

```bash
cargo run --release
```

By default listens on `127.0.0.1:5010`. Override with `PORT` env var:

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
IAS sparkline   Altitude sparkline
```

## Development

Dependencies:

* [tokio](https://docs.rs/tokio/) for async networking
* [ratatui](https://ratatui.rs/) for TUI widgets
* [crossterm](https://crates.io/crates/crossterm) for terminal control
* [serde](https://serde.rs/) and [serde\_json](https://docs.rs/serde_json/) for parsing telemetry

### Adding More Telemetry

Extend the Lua export with more `LoGet*` values (e.g., fuel, RPM, gear status) and update the Rust structs and UI rendering.

## License

MIT License. See [LICENSE](LICENSE) for details.
