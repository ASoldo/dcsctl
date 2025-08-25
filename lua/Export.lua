package.path = package.path .. ";.\\LuaSocket\\?.lua;.\\LuaSocket\\?\\init.lua"
package.cpath = package.cpath .. ";.\\LuaSocket\\?.dll"

local has_socket, socket = pcall(require, "socket")
local udp = nil

-- TX cadence
local HZ = 10
local DT = 1.0 / HZ
local lastSent = 0

-- ------------- helpers -------------
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
				if type(vv) == "number" then
					table.insert(sub, '"' .. kk .. '":' .. jnum(vv))
				elseif type(vv) == "string" then
					table.insert(sub, '"' .. kk .. '":"' .. vv:gsub("\\", "\\\\"):gsub('"', '\\"') .. '"')
				elseif type(vv) == "table" then
					local sub2 = {}
					for k3, v3 in pairs(vv) do
						local enc = (type(v3) == "number") and jnum(v3) or "null"
						table.insert(sub2, '"' .. k3 .. '":' .. enc)
					end
					table.insert(sub, '"' .. kk .. '":{' .. table.concat(sub2, ",") .. "}")
				else
					table.insert(sub, '"' .. kk .. '":null')
				end
			end
			val = "{" .. table.concat(sub, ",") .. "}"
		else
			val = "null"
		end
		table.insert(parts, key .. ":" .. val)
	end
	return "{" .. table.concat(parts, ",") .. "}"
end

local function N(x)
	return type(x) == "number" and x or nil
end
local function clamp(x, lo, hi)
	if type(x) ~= "number" then
		return nil
	end
	if x < lo then
		return lo
	end
	if x > hi then
		return hi
	end
	return x
end

-- ------------- engine normalization -------------
local function get_engine()
	local e = LoGetEngineInfo() or {}

	-- RPM (often percent 0..100)
	local rL, rR
	if type(e.RPM) == "table" then
		rL = N(e.RPM.left or e.RPM[1])
		rR = N(e.RPM.right or e.RPM[2])
	else
		rL = N(e.RPM)
	end

	-- Throttle 0..1; estimate from RPM if missing
	local thL, thR
	if type(e.Throttle) == "table" then
		thL = N(e.Throttle.left or e.Throttle[1])
		thR = N(e.Throttle.right or e.Throttle[2])
	else
		thL = N(e.Throttle)
	end

	local thr_est = false
	if thL == nil and rL ~= nil then
		thL = clamp(rL / 100.0, 0, 1)
		thr_est = true
	end
	if thR == nil and rR ~= nil then
		thR = clamp(rR / 100.0, 0, 1)
		thr_est = true
	end

	-- Nozzle 0..1 (many airframes don't expose)
	local nozL, nozR
	local noz = e.Nozzle or e.NOZ or e.nozzle
	if type(noz) == "table" then
		nozL = N(noz.left or noz[1])
		nozR = N(noz.right or noz[2])
	else
		nozL = N(noz)
	end
	local noz_present = (nozL ~= nil or nozR ~= nil)

	-- Temps (EGT/ITT)
	local tL, tR
	local temp = e.Temperature or e.Temp or e.EGT
	if type(temp) == "table" then
		tL = N(temp.left or temp[1])
		tR = N(temp.right or temp[2])
	else
		tL = N(temp)
	end

	-- Fuel flow
	local ffL, ffR
	local ff = e.FuelFlow or e.Fuel_Consumption or e.FuelConsumption
	if type(ff) == "table" then
		ffL = N(ff.left or ff[1])
		ffR = N(ff.right or ff[2])
	else
		ffL = N(ff)
	end

	-- Manifold pressure (props; jets often nil)
	local mapL, mapR
	local map = e.Manifold or e.MAP or e.Pressure
	if type(map) == "table" then
		mapL = N(map.left or map[1])
		mapR = N(map.right or map[2])
	else
		mapL = N(map)
	end
	local map_present = (mapL ~= nil or mapR ~= nil)

	return {
		rpm = { L = rL, R = rR },
		thrtl = { L = thL, R = thR },
		thrtl_est = thr_est, -- true if synthesized from RPM
		noz = { L = nozL, R = nozR },
		noz_present = noz_present, -- true if nozzle data exists
		temp = { L = tL, R = tR },
		fuelf = { L = ffL, R = ffR },
		map = { L = mapL, R = mapR },
		map_present = map_present, -- true if MAP exists
	}
end

-- ------------- mech normalization -------------
-- Coerce number/bool/table to 0..1
local function to_ratio(v)
	local tv = type(v)
	if tv == "number" then
		return v
	end
	if tv == "boolean" then
		return v and 1 or 0
	end
	if tv == "table" then
		if type(v.value) == "number" then
			return v.value
		end
		local sum, cnt = 0, 0
		for _, vv in pairs(v) do
			if type(vv) == "number" then
				sum = sum + vv
				cnt = cnt + 1
			end
		end
		if cnt > 0 then
			return sum / cnt
		end
	end
	return nil
end

local function pick_ratio(tbl, ...)
	if type(tbl) ~= "table" then
		return nil
	end
	local names = { ... }
	for i = 1, #names do
		local val = tbl[names[i]]
		if val ~= nil then
			return to_ratio(val)
		end
	end
	return nil
end

local function get_mech()
	local m = LoGetMechInfo() or {}

	local gear = pick_ratio(m, "gear", "gear_status", "undercarriage", "landing_gear")
	local flaps = pick_ratio(m, "flaps", "flaps_status")
	local airbrake =
		pick_ratio(m, "speedbrakes", "speed_brakes", "speed_brake", "speedbrake", "airbrake", "air_brake", "air_brakes")
	local hook = pick_ratio(m, "hook", "tailhook")
	local wing = pick_ratio(m, "wing", "wing_sweep", "wingsweep")
	local wow = pick_ratio(m, "weight_on_wheels", "WOW", "weight_on_wheel")

	-- Last-resort WoW guess
	local wow_guess = false
	if wow == nil then
		local agl = LoGetAltitudeAboveGroundLevel() or 1e9
		local tas = LoGetTrueAirSpeed() or 1e9
		local vv = LoGetVerticalVelocity() or 1e9
		if agl < 0.5 and math.abs(vv) < 1.0 and tas < 2.0 then
			wow = 1
			wow_guess = true
		end
	end

	return {
		gear = gear,
		flaps = flaps,
		airbrake = airbrake,
		hook = hook,
		wing = wing,
		wow = wow,
		wow_guess = wow_guess,
	}
end

-- ------------- DCS hooks -------------
function LuaExportStart()
	log("LuaExportStart()")
	if has_socket then
		udp = socket.udp()
		udp:settimeout(0)
		udp:setpeername("127.0.0.1", 5010)
		log("UDP armed 127.0.0.1:5010 @ " .. HZ .. " Hz")
	else
		log("LuaSocket missing")
	end
	lastSent = 0
end

function LuaExportBeforeNextFrame()
	-- present for compatibility
end

function LuaExportAfterNextFrame()
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
		-- flight baseline
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

		-- systems
		engine = get_engine(),
		mech = get_mech(),
	}

	if udp then
		udp:send(jsonify(payload) .. "\n")
	end
end

function LuaExportStop()
	log("LuaExportStop()")
	if udp then
		udp:close()
		udp = nil
	end
end

function LuaExportActivityNextEvent(t)
	return (t or 0) + DT
end
