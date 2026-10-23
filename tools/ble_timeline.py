import asyncio, time, sys
from bleak import BleakScanner
t0 = time.time(); seen = []
def cb(d, ad):
    if d.address.upper() == "F0:F5:BD:0B:EC:E4":
        seen.append(round(time.time() - t0, 1))
async def main():
    async with BleakScanner(cb):
        await asyncio.sleep(float(sys.argv[1]))
    print("sightings (s since start):", seen[:5], "…", seen[-5:], "total", len(seen))
asyncio.run(main())
