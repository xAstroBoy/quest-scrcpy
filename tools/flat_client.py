#!/usr/bin/env python3
"""Test client for the FlatStream agent: connect, read [w][h] + length-prefixed
Annex-B AUs for a few seconds, save the raw H.264, and report NAL unit types so
we can confirm the stream is valid (expect SPS=7, PPS=8, IDR=5, non-IDR=1)."""
import socket, struct, sys, time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 27200
SECS = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
OUT = sys.argv[3] if len(sys.argv) > 3 else "flat.h264"

s = socket.create_connection(("127.0.0.1", PORT), timeout=8)


def readn(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError("closed")
        b += c
    return b


w, h = struct.unpack(">II", readn(8))
print("stream", w, "x", h)

data = bytearray()
frames = 0
nal = {}
t0 = time.time()
try:
    while time.time() - t0 < SECS:
        ln = struct.unpack(">I", readn(4))[0]
        au = readn(ln)
        data += au
        frames += 1
        i = 0
        n = len(au)
        while i < n - 4:
            if au[i] == 0 and au[i + 1] == 0 and au[i + 2] == 1:
                t = au[i + 3] & 0x1F
                nal[t] = nal.get(t, 0) + 1
                i += 3
            elif au[i] == 0 and au[i + 1] == 0 and au[i + 2] == 0 and au[i + 3] == 1:
                t = au[i + 4] & 0x1F
                nal[t] = nal.get(t, 0) + 1
                i += 4
            else:
                i += 1
except Exception as e:
    print("end:", e)

with open(OUT, "wb") as f:
    f.write(data)
fps = frames / max(time.time() - t0, 0.001)
print(f"frames={frames} bytes={len(data)} ~{fps:.0f}fps nal_types={dict(sorted(nal.items()))}")
print("  (7=SPS 8=PPS 5=IDR 1=P)  saved", OUT)
s.close()
