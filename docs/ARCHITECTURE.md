# Rabbit Water Tank Level Monitor — Architecture Proposal

Status: **agreed design; phases 0–3 implemented with a simulated sensor** (2026-10-23, rev 4). See `README.md` for the current status table, API and BLE service.

Decisions so far: A02YYUW ultrasonic sensor; square tank with closed lid (volume is
linear in depth); 12 V lithium battery + solar panel at the tank; WiFi available and
the Rails server is on the **same LAN**; thresholds 25 % low / 10 % critical; more
sensors will be added to the board later.

## 1. Hardware baseline

| Item | Value |
|---|---|
| MCU | ESP32-C6 (QFN40, rev v0.1), RISC-V, 160 MHz |
| Radios | WiFi 6 (2.4 GHz), BLE 5, IEEE 802.15.4 (Zigbee/Thread) |
| Flash / RAM | 16 MB / 512 KB SRAM + 16 KB LP RAM |
| Dev connection | native USB-Serial-JTAG, `/dev/ttyACM0` |
| Base MAC | f0:f5:bd:0b:ec:e4 (used as device ID) |
| Sensor | DFRobot **A02YYUW** waterproof ultrasonic (SKU SEN0311) |

### A02YYUW facts that drive the design

| Parameter | Value |
|---|---|
| Supply | 3.3–5 V, ≤5 mA standby, ≤8 mA average |
| Range | 3–450 cm, **3 cm blind zone** |
| Beam | 60° |
| Protection | IP67, -15…60 °C |
| Output | UART TTL 9600 8N1, 4-byte frame |
| Frame | `0xFF, DATA_H, DATA_L, SUM`; distance = `DATA_H*256 + DATA_L` **mm**; `SUM = (0xFF + H + L) & 0xFF` |
| RX pin high/floating | *processed* (filtered) value, response 100–300 ms |
| RX pin low | *real-time* value, response 100 ms |
| Pins | VCC, GND, RX (mode select), TX (data). Wire colours vary by batch; trust the labels on the connector. |

Design consequences:

* **Power the sensor from 3V3**, not 5 V, so its TX idles at a level the C6
  GPIO tolerates without a divider.
* **Tie sensor RX high** (to 3V3 or leave floating) for the processed mode.
  We may drive it from a GPIO later to switch modes, but there is no need yet.
* **Mounting:** the probe must sit ≥ 5 cm above the *maximum* water level
  (3 cm blind zone + margin). Full tank therefore reads ≈50 mm, not 0.
* **Beam width:** 60° in a narrow tank means wall echoes. If the tank is
  narrower than roughly the sensor height, or has a float valve / inlet pipe
  in view, drop the probe into a **stilling tube** (75–110 mm PVC pipe, open
  at the bottom, vent hole at the top). Still water inside the tube also kills
  surface ripple noise.
* **Temperature:** speed of sound drifts ~0.17 %/°C. Over a 50 cm tank and a
  30 °C seasonal swing that is ~2.5 cm. Acceptable for "is there enough
  water", fixable later with a DS18B20 if you want litres to the cm.
* **Condensation** on the probe face in a closed tank can blank readings;
  the firmware must treat "no valid frame" as a distinct state, not as empty.

### Power: 12 V lithium + solar

The 12 V battery is generous for this load, so the plan is **always-on with
WiFi power-save first, measure, then add sleep only if the numbers say so**.
Always-on keeps BLE and the local web UI reachable at any time, which deep
sleep would not.

| Mode | Avg current @3.3 V (est.) | Energy/day incl. ~15 % buck loss | Days on 12 V 7 Ah LiFePO4 (~70 Wh usable), no sun |
|---|---|---|---|
| Always on, WiFi modem-sleep (DTIM), sensor on | ~30 mA | ~2.7 Wh | ~25 |
| + light sleep between samples, WiFi keep-alive | ~8 mA | ~0.75 Wh | ~90 |
| Deep sleep, wake every 5 min to sample + report | ~1 mA | ~0.1 Wh | years (battery self-discharge dominates) |

Even a 10 W panel at a poor winter yield of 1 h-equivalent/day gives ~10 Wh,
so row 1 is already sustainable. Peaks: WiFi TX bursts ~350 mA for ms.

Hardware notes:

* **Buck converter 12 V → 3.3 V**, ≥ 600 mA, low quiescent current (e.g.
  Pololu D24V5F3/D36V6F3 class, or any sync buck with Iq < 1 mA). Feed the
  devkit's 3V3 pin (bypassing the on-board LDO) **or** buck to 5 V into the
  5V pin and accept ~35 % LDO loss. Prefer 3.3 V direct; just do not have USB
  plugged in at the same time unless the board has a protection diode.
* **Battery voltage sense**: divider 100 kΩ / 22 kΩ from battery + to an ADC
  pin (14.6 V full-charge → 2.6 V, within the 12 dB attenuation range).
  Reported in every payload; Rails can alert on low battery and you get a
  free view of solar health. Quiescent draw of the divider: ~0.1 mA.
* Optional: power the sensor through a GPIO-controlled high-side switch to
  save its 5–8 mA between samples. Only worth it if we go to light sleep.
* Fuse on the battery lead; the solar charge controller's load output is a
  fine place to take power from if it has one (gives low-voltage cutoff).

## 2. Software stack (bare metal, `no_std`)

Already bootstrapped by `esp-generate` 1.4.0 and verified to build and flash:

| Layer | Crate | Role |
|---|---|---|
| HAL | `esp-hal` 1.2 | UART, GPIO, timers, flash, RNG |
| Scheduler | `esp-rtos` 0.4 + `embassy-executor` 0.10 | async tasks, no OS |
| Radio | `esp-radio` 1.0-beta (wifi + ble + coex) | WiFi STA/AP + BLE controller |
| TCP/IP | `embassy-net` 0.9 / `smoltcp` 0.14 | DHCP, DNS, TCP, UDP |
| BLE host | `trouble-host` 0.7 | GATT server |
| Heap | `esp-alloc` | 128 KB heap (needed for coex + TLS) |
| Logging | `log` + `esp-println` | serial log over USB |
| Bootloader | `esp-bootloader-esp-idf` | ESP-IDF 2nd stage bootloader, OTA slots |

Crates to add per phase (current versions on crates.io):

| Need | Crate | Notes |
|---|---|---|
| HTTP(S) client | `reqwless` 0.14 | built for `embassy-net`; TLS via `embedded-tls` |
| TLS | `embedded-tls` 0.19 | pure Rust, **TLS 1.3 only**, no alloc needed |
| HTTP server (local UI) | `picoserve` 0.20 | async router, serves static HTML from flash |
| SoftAP captive portal (optional) | `edge-dhcp` / `edge-captive` 0.8 | only if we add AP-mode provisioning |
| Time sync | `sntpc` 0.11 | or take `Date` from the Rails response |
| Flash key/value | `sequential-storage` 8 + `esp-storage` 0.10 | wear-levelled config |
| JSON | `serde-json-core` 0.7 | serialise reports without alloc |
| Fixed-size buffers | `heapless` 0.9 | strings, queues, median window |
| MQTT (only if §4 option B chosen) | `rust-mqtt` 0.6 or `mqttrust` 1.0 | both `no_std`, async |

## 3. Firmware module layout

```
src/
  lib.rs
  config.rs        DeviceConfig {ssid, psk, api_url, api_token, device_name,
                   tank: {empty_mm, full_mm, litres_per_mm},
                   battery: {divider_ratio, low_volts},
                   sample_interval_s, report_interval_s, low_pct, critical_pct}
  sensors/
    mod.rs         `Sensor` trait: id(), async read() -> Reading; registry
                   of enabled sensors; each contributes a JSON sub-object
    battery.rs     ADC divider → volts (phase 2)
    ds18b20.rs     (later) 1-Wire temperature, also speed-of-sound correction
  level/
    a02yyuw.rs     async UART frame parser: sync on 0xFF, verify SUM, mm or
                   Error::{Checksum, Timeout, BlindZone, OutOfRange}
    filter.rs      sliding median of N (odd, e.g. 7) + outlier rejection
                   + "no valid frame for 10 s" → SensorFault state
    tank.rs        distance_mm → depth_mm → percent → litres. Square tank ⇒
                   linear: litres = (empty_mm - distance_mm) * litres_per_mm,
                   with empty_mm/full_mm set by the two-point calibration
                   (calibrate_empty / calibrate_full) and litres_per_mm from
                   the footprint (side² / 1e6) or from a measured fill
    mod.rs         sampler task: every sample_interval_s publish Level{...}
  net/wifi.rs      connect, reconnect with backoff, RSSI, DHCP
  net/report.rs    build JSON, HTTP POST (TLS behind optional feature), offline queue, apply config from reply
  net/time.rs      SNTP or server-time offset
  ble/gatt.rs      provisioning + status GATT service
  web/server.rs    picoserve routes, basic auth
  web/index.html   single-page status/config UI, included with include_str!
  ota.rs           (phase 4) download image to inactive slot, mark, reboot
bin/main.rs        init, spawn tasks:
                   [wifi] [net stack] [level sampler] [reporter] [ble] [web]
```

Tasks communicate through `embassy-sync` channels/watch and a shared
`Mutex<Config>` so BLE, web and cloud can all mutate the same config safely.
The latest `Level` lives in an `embassy_sync::watch::Watch` that the
reporter, BLE notify and web page all read.

### Sampling and reporting cadence

Water level in a drinker tank changes slowly. Proposed defaults:

| Thing | Default | Why |
|---|---|---|
| Sensor sample | every 10 s | cheap; feeds the median filter |
| Report to Rails | every 5 min, **or immediately** on a state change | keeps traffic tiny, alerts fast |
| State changes | `ok → low → critical`, `refill detected`, `sensor fault`, `rapid drop` (leak) | these are what you actually want to know |

Rails does the day-to-day analytics (consumption per day = sum of level
drops excluding refills × tank cross-section). The device stays dumb and
sends distance + derived level; if you change tank geometry in Rails the
history can be recomputed.

## 4. Getting data into the Rails app — options

### Option A — Direct HTTP POST to a Rails JSON endpoint (**chosen**)

```
POST http://<rails-host-or-ip>:3000/api/v1/tank_readings
Authorization: Bearer <per-device token>
Content-Type: application/json

{ "device_id": "f0f5bd0bece4", "seq": 1842, "fw": "0.1.0",
  "uptime_s": 86400, "rssi": -61,
  "level": { "distance_mm": 312, "depth_mm": 438, "percent": 73.0, "litres": 87.6,
             "state": "ok", "event": null },
  "battery": { "volts": 13.12 },
  "sampled_at": "2026-10-23T14:05:00Z" }

200 OK
{ "server_time": "2026-10-23T14:05:01Z",
  "config": { "report_interval_s": 300, "low_pct": 25, "critical_pct": 10,
              "tank": { "sensor_offset_mm": 50, "full_depth_mm": 600, ... } } }
```

* **Pros:** zero new infrastructure; Rails already does HTTP, JSON, auth and
  background jobs. Easy to test with `curl`. The reply carries *desired config*,
  so every report doubles as a command poll — this gives you most of what MQTT's
  "server push" would, at a 5-minute latency that is fine for a tank level.
* **Same LAN ⇒ plain HTTP.** Since the Rails server sits on the same
  network, phase 2 uses HTTP without TLS: no certificate to embed, no
  TLS 1.3 requirement on the server, less heap, simpler debugging with
  `tcpdump`. A per-device bearer token still authenticates the device.
  `embedded-tls` stays behind an optional `tls` cargo feature for the day the
  server moves off-site.
* **Addressing:** point the device at the Rails host's IP or a DHCP
  reservation / router DNS name. Optional later: mDNS (`edge-mdns`) so the
  device itself is reachable as `tank-xxxx.local` and Rails can link to its
  local UI.
* **Cons:** device-initiated only. Server cannot wake the device instantly.
* **Resilience:** `seq` makes the endpoint idempotent; the device keeps a
  ring buffer of unsent reports (RAM) and drains it when the link returns.

### Option B — MQTT broker + Rails subscriber

Device → `mqtt://broker` (`farm/tank/<id>/level`), Rails consumes.

* **Pros:** true bidirectional push, QoS 1, last-will gives free online/offline
  presence, scales to hundreds of devices, tolerant of flaky WiFi.
* **Cons:** one more server to run, secure and monitor. Rails needs a
  long-running subscriber (Ruby `mqtt` gem daemon under systemd) **or** a
  broker that can webhook (EMQX rules engine → `POST` to Rails; Mosquitto
  cannot). TLS story on the device is the same as A.
* **When it wins:** more than ~10–20 devices, you need sub-second commands,
  or you want presence/alerts the moment a device drops.

### Option C — Zigbee / Thread over 802.15.4

Needs a coordinator / border router on site and a bridge to Rails. Rust support
on the C6 for Zigbee is immature. **Not now**, but the right future path for
battery sensors in hutches/fields without WiFi.

### Option D — ActionCable / WebSocket, CoAP

WebSocket is heavy for `no_std`; CoAP needs a gateway Rails doesn't speak.
**Rejected.**

### Decision

**A, over plain HTTP on the LAN.** The transport sits behind a small
`Reporter` trait so **B** can be added later without touching `level`,
`config`, BLE or web.

### Rails side (minimal)

* `Device` model: `has_secure_token :api_token`, `mac`, `name`, tank
  geometry columns, `report_interval_s`, `low_pct`, `critical_pct`,
  `last_seen_at`, `last_rssi`, `state`.
* `TankReading` model: `device_id`, `seq`, `distance_mm`, `depth_mm`,
  `percent`, `litres`, `state`, `event`, `battery_volts`, `sampled_at`, plus
  a `sensors` **jsonb** column holding the whole payload so future sensors
  land without a migration; unique index on `(device_id, seq)`.
* `Api::V1::TankReadingsController#create`: authenticate bearer, upsert,
  return `config` JSON. Fire a notification job on `event` (low, critical,
  sensor_fault, leak). Reports arrive every 5 min, so no special scaling.
* Optional: "Add device" page that uses **Web Bluetooth** to provision the
  device straight from the Rails UI (see §5).

## 5. Management over Bluetooth

`trouble-host` GATT server with a custom 128-bit service, advertised as
`TANK-<last 4 MAC>`:

| Characteristic | Access | Purpose |
|---|---|---|
| WiFi SSID | write | provisioning |
| WiFi password | write only | never readable |
| API URL / token | write | points at the Rails app |
| Tank geometry | read/write | offset, depth, shape, litres |
| Thresholds / intervals | read/write | |
| Live level (JSON) | read/notify | distance, %, litres, state, rssi, ip |
| Command | write | `reboot`, `calibrate_full`, `calibrate_empty`, `factory_reset`, `test_report` |

`calibrate_full` / `calibrate_empty` record the current raw distance as the
0 %/100 % reference — far easier than measuring the tank with a ruler.

Security: BLE is advertised **only** for 5 min after boot or while a button is
held, and writes require LE Secure Connections pairing with a PIN printed on
the device. Chrome's Web Bluetooth lets the Rails app itself be the
provisioning tool on desktop/Android; iOS users fall back to nRF Connect.

## 6. Management over the web

Two layers, both wanted:

1. **Local device UI** — `picoserve` on port 80 serving one embedded HTML
   page: live level gauge, WiFi status, config form, calibrate buttons,
   reboot. HTTP basic auth with a device password. LAN only, handy on site.
2. **Remote via Rails** — the desired-state `config` block in every report
   reply (§4 A). Change thresholds or geometry in the Rails admin and the
   device picks it up on its next report. No extra protocol.

Optional later: SoftAP + captive portal (`edge-dhcp`, `edge-captive`) so a
phone without BLE can provision the device. Skipped initially because BLE
already covers the "no WiFi yet" case.

## 7. Wiring

| A02YYUW | ESP32-C6 devkit | Note |
|---|---|---|
| VCC | 3V3 | not 5 V, see §1 |
| GND | GND | |
| TX | GPIO4 (UART1 RX) | any GPIO works via the matrix; avoid strapping pins 8, 9, 15 and USB pins 12, 13 |
| RX | 3V3 (or GPIO5 if we want to switch modes) | processed mode |

| Other | ESP32-C6 devkit | Note |
|---|---|---|
| Battery divider mid-point | GPIO1 (ADC1_CH1) | 100 k / 22 k, see §1 |
| Spare I2C bus | GPIO6 SDA, GPIO7 SCL | future sensors (BME280, light, …) |
| Spare 1-Wire | GPIO10 | future DS18B20(s), 4.7 kΩ pull-up |
| Provisioning button | GPIO9 (BOOT button on most devkits) | hold to enable BLE |

Bring the I2C and 1-Wire pins out to a header now so adding sensors later is
plug-in, not re-wiring. Sensor cable can be extended a few metres at 9600
baud without trouble.

### Mechanical (closed lid)

Mount the stilling tube through the lid with the probe in a cap on top; the
tube hangs to ~2 cm above the tank floor with a small vent hole near the lid.
The closed lid means condensation on the probe face is likely on cold
mornings; the `SensorFault` state and a short grace period handle the
occasional blank reading, and the lid tube keeps the face above splash.

## 8. Partition table and OTA (set up now, use later)

Default template partition is a single 15.6 MB `factory` app. Switch to a
custom `partitions.csv` **before** the first real deployment so OTA never
needs a full erase:

```
nvs,      data, nvs,     0x9000,   0x6000
phy_init, data, phy,     0xf000,   0x1000
otadata,  data, ota,     0x10000,  0x2000
ota_0,    app,  ota_0,   0x20000,  0x400000
ota_1,    app,  ota_1,   0x420000, 0x400000
storage,  data, 0x40,    0x820000, 0x10000
```

OTA flow (phase 4): Rails serves the image (Active Storage), reply includes
`fw_url` + sha256; device streams it into the inactive slot with `reqwless`,
verifies, marks slot via `esp-bootloader-esp-idf::ota`, reboots, confirms
after first successful report (rollback otherwise).

## 9. Decisions and remaining questions

Resolved 2026-10-23:

| Question | Answer | Consequence |
|---|---|---|
| Tank | square, closed lid | linear volume model, two-point calibration, stilling tube through lid |
| Power | 12 V lithium + solar | always-on first, buck to 3.3 V, battery voltage reported, sleep modes only if measurements demand |
| Network | WiFi, Rails on same LAN | plain HTTP, no TLS in phase 2; `tls` feature kept for later |
| Thresholds | 25 % low, 10 % critical | defaults in `config.rs`, overridable from Rails |
| Fleet | one tank, more sensors on the same board later | `Sensor` trait + jsonb payload, I2C/1-Wire headers |

Still open (none block phase 1):

1. Tank inner side length and depth, or just capacity and depth? Only needed
   to show litres; percent works from calibration alone.
2. Alert channel in Rails (email, push, Telegram)?
3. Battery chemistry (LiFePO4 vs Li-ion) and capacity, for the low-battery
   threshold and the voltage-to-percent curve.
4. Time source: server time from the reply (default) or SNTP?

## 10. Implementation findings (2026-10-23)

* **Coexistence:** on esp-radio 1.0.0-beta.1 the BLE radio stays silent while
  the WiFi driver is created but idle. Workaround: scan WiFi every 20 s while
  unprovisioned. With WiFi connected, BLE advertises continuously (verified
  51 sightings/min from a BlueZ host).
* **Power save:** `PowerSaveMode::Minimum` made the device unreachable over
  IP on the test AP and starved BLE. Running with power save **off** until
  measured on the real AP; §1's power budget assumed modem-sleep, so expect
  ~50–80 mA average instead of ~30 mA until this is resolved (still fine on
  the 12 V battery + solar).
* **Flash config** lives in the stock `nvs` partition (0x9000, 24 KB) via
  `sequential-storage`; no custom partition table needed yet. OTA (§8) will
  still require one.
* **picoserve on stable Rust:** build the router inside the task function
  (`fn make_app() -> Router<impl PathRouter>`) instead of the nightly-only
  `AppBuilder` associated-type pattern.
* **Host tests** need the core logic in its own crate (`crates/tank-core`)
  because the firmware package's esp dependencies do not compile for x86.

## 11. Phased plan

| Phase | Deliverable | Verifies |
|---|---|---|
| 0 ✅ | Template builds, flashes, logs over USB | toolchain |
| 1 ◐ | `level` module: UART parser, median filter, mm/%/litres in the log — done against a **simulator**; real-sensor run pending delivery | sensor wiring, mounting, noise |
| 2 ◐ | WiFi STA ✅, state machine + events ✅, reporter builds the JSON ✅ but **logs instead of POSTing**; config-from-reply, offline queue, battery ADC pending | transport, auth, alerts, power draw measured |
| 3 ✅ | BLE provisioning service with calibrate commands; local web UI (no pairing / no HTTP auth yet) | management |
| 4 | OTA partitions + update flow | field maintainability |
| 5 | (as needed) light/deep sleep if phase 2 current is too high, DS18B20 + I2C sensors, mDNS, MQTT reporter, SoftAP portal, TLS | scale |
