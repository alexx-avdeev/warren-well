import asyncio
from bleak import BleakScanner
async def main():
    devs = await BleakScanner.discover(timeout=15, return_adv=True)
    print(len(devs), "devices")
    for addr, (d, ad) in devs.items():
        if addr.startswith("F0:F5") or (ad.local_name or "").startswith("tank") or d.name:
            print(addr, repr(ad.local_name), repr(d.name), ad.rssi, list(ad.service_uuids))
asyncio.run(main())
