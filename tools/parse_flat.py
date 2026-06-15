#!/usr/bin/env python3
"""Parse a captured FlatStream dump ([u32 w][u32 h] then [u32 len][AnnexB AU]...),
report NAL unit types (7=SPS 8=PPS 5=IDR 1=P), and write the concatenated raw
.h264 for playback/decode verification."""
import struct, sys

raw = open(sys.argv[1], "rb").read()
out_path = sys.argv[2] if len(sys.argv) > 2 else "flat.h264"
if len(raw) < 8:
    print("too short:", len(raw)); sys.exit(1)
w, h = struct.unpack(">II", raw[:8])
print("stream", w, "x", h, " total bytes", len(raw))
i, aus, nal = 8, 0, {}
out = bytearray()
while i + 4 <= len(raw):
    ln = struct.unpack(">I", raw[i:i + 4])[0]; i += 4
    if ln <= 0 or i + ln > len(raw):
        break
    au = raw[i:i + ln]; i += ln; aus += 1; out += au
    j, n = 0, len(au)
    while j < n - 4:
        if au[j] == 0 and au[j + 1] == 0 and au[j + 2] == 1:
            t = au[j + 3] & 0x1F; nal[t] = nal.get(t, 0) + 1; j += 3
        elif au[j] == 0 and au[j + 1] == 0 and au[j + 2] == 0 and au[j + 3] == 1:
            t = au[j + 4] & 0x1F; nal[t] = nal.get(t, 0) + 1; j += 4
        else:
            j += 1
print(f"access_units={aus} h264_bytes={len(out)} nal_types={dict(sorted(nal.items()))}")
print("  (need 7=SPS 8=PPS 5=IDR 1=P)")
open(out_path, "wb").write(out)
print("  wrote", out_path)
