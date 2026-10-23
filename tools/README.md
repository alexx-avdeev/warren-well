# Development tools

Python scripts need `bleak` (`python3 -m venv .venv && .venv/bin/pip install bleak`)
and BlueZ on the host.

| Script | Purpose |
|---|---|
| `ble_scan.py` | List nearby BLE advertisers; confirms the device is on air. |
| `ble_timeline.py <seconds>` | Record when the device's advertisements are seen. Used to diagnose the coexistence issue. |
| `ble_test.py` | End-to-end GATT test: reads status, writes WiFi/API/tank/interval characteristics, subscribes to status notifications, sends test-report and save+apply commands. **Edit the SSID/password/URL constants near the top before running** — command 1 persists them to the device. |

## Test hotspot (host has an idle WiFi adapter)

`hotspot/hostapd.conf` + `hotspot/dnsmasq.conf` bring up `TankTest` /
`tanktest123` on `wlo1` at 192.168.50.1 with DHCP. Adjust the interface name.

```sh
sudo ip link set wlo1 up && sudo ip addr add 192.168.50.1/24 dev wlo1
sudo hostapd -B tools/hotspot/hostapd.conf
sudo dnsmasq -C tools/hotspot/dnsmasq.conf
# tear down
sudo pkill hostapd; sudo pkill -f tools/hotspot/dnsmasq.conf; sudo ip addr flush dev wlo1
```

Note: the Intel iwlwifi AP did not deliver frames to the device while WiFi
power save was enabled; the firmware currently runs with power save off.
