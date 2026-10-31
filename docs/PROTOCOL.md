# Device ↔ server protocol v1 — `tank_readings`

Agreed between the firmware (this repo) and the Rails farm-management app on
2026-10-31. Plain HTTP on the LAN; one request per TCP connection.

## Request

```
POST <api_url>                       e.g. http://10.10.1.16:3000/api/v1/tank_readings
Host: <host>
Authorization: Bearer <api_token>    32 lowercase hex chars, issued per tank in the Rails UI
Content-Type: application/json
Content-Length: <n>
Connection: close
```

```json
{
  "device_id": "f0f5bd0bece4",
  "boot_id": 1810345540,
  "seq": 3,
  "fw": "0.1.0",
  "uptime_s": 46,
  "rssi": -79,
  "level": {
    "distance_mm": 196, "depth_mm": 444, "percent": 75.9, "litres": 133.2,
    "state": "ok",
    "event": null
  },
  "sampled_at_uptime_s": 41,
  "sampled_at": 1793465985
}
```

| Field | Type | Notes |
|---|---|---|
| `device_id` | string | WiFi MAC, 12 lowercase hex. Must match the tank the token belongs to. |
| `boot_id` | u32 | Random per boot, never 0. |
| `seq` | u32 | Starts at 1 on every boot. `(device_id, boot_id, seq)` identifies a report. |
| `fw` | string | Firmware version. |
| `uptime_s` | u64 | Seconds since boot when the report was built. |
| `rssi` | i32 or null | dBm. |
| `level.distance_mm/depth_mm` | u16 or null | null until the first valid sample. |
| `level.percent/litres` | f32 or null | |
| `level.state` | string | `unknown` `ok` `low` `critical` `sensor_fault` |
| `level.event` | string or null | `low` `critical` `recovered` `refill` `rapid_drop` `sensor_fault` `sensor_recovered`. Non-null means this report is an alert and was sent immediately. |
| `sampled_at_uptime_s` | u64 | Uptime when the sample was taken. Server derives `sampled_at = received_at − (uptime_s − sampled_at_uptime_s)`. |
| `sampled_at` | u64 or null | Unix seconds; present once the device has learned the time from `server_time`. Server prefers it when present. |
| `battery.volts` | f32 | Reserved, not sent yet. |
| `sensors` | object | Reserved for extra sensors, stored as JSON. |

Unknown keys are ignored by the server. Body stays under 512 bytes.

## Response

| Status | Meaning | Device action |
|---|---|---|
| 201 | stored | apply reply, drop from queue |
| 200 | duplicate `(device, boot_id, seq)`; same body as the 201 | same as 201 |
| 401 | bad or missing token | keep report, mark `unauthorized` in the UIs, retry every 5 min |
| 422 | invalid body or `device_id` ≠ tank's | drop the report, log the body |
| 5xx, connect error, timeout | | keep, retry with backoff 10 s → 5 min |

Body of 200/201, `Content-Length` set, no chunking, no gzip, ≤ 1 KB:

```json
{
  "server_time": 1793465974,
  "config": {
    "config_version": 7,
    "name": "hutch-1",
    "report_interval_s": 300, "sample_interval_s": 10,
    "low_pct": 25, "critical_pct": 10,
    "tank": { "empty_mm": 650, "full_mm": 50, "litres_per_mm": 0.25 }
  }
}
```

* `server_time` unix seconds, always present. The device stores
  `server_time − uptime` and timestamps later reports with it.
* `config` present only when the operator set something for this tank. Every
  field optional. `config_version` is bumped by the server on each change; the
  device applies the block only when the version differs from the last one it
  applied, then persists it to flash. Never contains WiFi credentials, the
  URL or the token.
* `firmware: {...}` is reserved for OTA and not sent yet.
* A reply that fails to parse is treated as success (the 2xx is what counts).

## Enrolment

1. Operator creates a water tank in the Rails UI and enters the `device_id`
   shown on the device's status page. The UI generates the `api_token`.
2. Operator writes `api_url` and `api_token` to the device over BLE
   (characteristics `…0004`, `…0005`) or the device web UI.
3. The device starts reporting; the first report proves the link.

## Device-side implementation notes

* `src/app/reporter.rs`: reports are serialised once and queued (newest 16);
  a retry resends the identical bytes. Replies are parsed into
  `tank_core::config::ServerReply`.
* Reporting cadence: every `report_interval_s` (default 300) plus immediately
  on every `level.event`; a `test_report` command sends one now.
* Status of the last attempt is in `GET /api/status` → `report`.
