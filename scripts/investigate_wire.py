#!/usr/bin/env python3
"""ArkSIM: investigate wire format by reading ALL available data."""
import socket, struct, time

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(("127.0.0.1", 18000))
s.settimeout(5)

print("Reading all available data for 5 seconds...")
alldata = b""
start = time.time()
while time.time() - start < 5.0:
    try:
        d = s.recv(65536)
        if d:
            alldata += d
            print(f"  t={time.time()-start:.1f}s: received {len(d)} bytes (total {len(alldata)})")
    except socket.timeout:
        break
    except BlockingIOError:
        break

s.close()

print(f"\nTotal received: {len(alldata)} bytes")

if len(alldata) < 4:
    print("Not enough data")
elif len(alldata) < 100:
    print(f"Raw hex: {alldata.hex()}")
    # Try to interpret as different frame formats
    for offset in range(min(5, len(alldata)-4)):
        le = struct.unpack("<I", alldata[offset:offset+4])[0]
        be = struct.unpack(">I", alldata[offset:offset+4])[0]
        print(f"  offset={offset}: LE_u32={le}, BE_u32={be}")
else:
    print(f"First 128 bytes hex: {alldata[:128].hex()}")
    # Try to parse as length-prefixed
    frame_len = struct.unpack("<I", alldata[:4])[0]
    print(f"Frame length (LE): {frame_len}")
    if frame_len < len(alldata):
        print(f"Payload starts at offset 4, {frame_len} bytes")
        print(f"Next 16 bytes after payload: {alldata[4:20].hex()}")
