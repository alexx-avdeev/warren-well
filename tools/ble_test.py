import asyncio, struct, sys
from bleak import BleakScanner, BleakClient

BASE = "7a1e{:04x}-5a7e-4a6b-9b0e-2b4c6d8e0f10"
SSID, PASS, URL, TOKEN, CAL, INTERVALS, STATUS, COMMAND = (BASE.format(i) for i in range(2, 10))

async def main():
    print("scanning…")
    dev = await BleakScanner.find_device_by_filter(lambda d, ad: d.address.upper() == "F0:F5:BD:0B:EC:E4" or (ad.local_name or d.name or "").startswith("tank-"), timeout=25)
    if not dev:
        print("device not found"); sys.exit(1)
    print("found", dev.name, dev.address)
    async with BleakClient(dev, timeout=20) as c:
        print("connected, mtu", c.mtu_size)
        print("status:", (await c.read_gatt_char(STATUS)).decode())
        print("ssid before:", repr((await c.read_gatt_char(SSID)).decode()))
        await c.write_gatt_char(SSID, b"TankTest", response=True)
        await c.write_gatt_char(PASS, b"tanktest123", response=True)
        await c.write_gatt_char(URL, b"http://192.168.50.1:3000/api/v1/tank_readings", response=True)
        print("ssid after:", repr((await c.read_gatt_char(SSID)).decode()))
        print("url after:", repr((await c.read_gatt_char(URL)).decode()))
        cal = await c.read_gatt_char(CAL)
        print("tank cal:", struct.unpack("<HHf", cal))
        await c.write_gatt_char(CAL, struct.pack("<HHf", 600, 60, 0.36), response=True)
        print("tank cal after:", struct.unpack("<HHf", await c.read_gatt_char(CAL)))
        print("intervals:", struct.unpack("<HHBB", await c.read_gatt_char(INTERVALS)))
        # subscribe to status notifications and wait for one sample (10 s period)
        got = asyncio.Event()
        def on_notify(_, data): print("notify:", data.decode()); got.set()
        await c.start_notify(STATUS, on_notify)
        try:
            await asyncio.wait_for(got.wait(), 15)
        except asyncio.TimeoutError:
            print("no notification within 15 s")
        await c.stop_notify(STATUS)
        print("command 6 (test report)…")
        await c.write_gatt_char(COMMAND, bytes([6]), response=True)
        print("command 1 (save + apply wifi)…")
        await c.write_gatt_char(COMMAND, bytes([1]), response=True)
        await asyncio.sleep(1)
    print("disconnected")

asyncio.run(main())
