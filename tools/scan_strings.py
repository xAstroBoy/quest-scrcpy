#!/usr/bin/env python3
"""Crude strings + keyword grep over a binary, to map how Magic Island casting
produces its flat view. Usage: python tools/scan_strings.py <file> [more files]
"""
import re
import sys

KEYWORDS = [
    "VirtualDisplay", "createVirtualDisplay", "DisplayManager", "SurfaceControl",
    "MediaProjection", "MediaCodec", "ScreenCapture", "captureLayers", "captureDisplay",
    "Spectator", "spectator", "Projection", "projection", "Casting", "casting",
    "OWN_CONTENT", "PRESENTATION", "PUBLIC", "SECURE", "layerStack", "displayId",
    "createDisplay", "mirror", "Mirror", "monoscopic", "flatten", "undistort",
    "eyeBuffer", "spectatorCamera", "thirdEye", "third_eye", "Surface(", "ANativeWindow",
]

STR = re.compile(rb"[\x20-\x7e]{5,}")


def scan(path):
    with open(path, "rb") as f:
        data = f.read()
    found = {}
    for m in STR.finditer(data):
        s = m.group().decode("ascii", "ignore")
        for k in KEYWORDS:
            if k in s:
                found.setdefault(k, set()).add(s)
    print(f"\n===== {path} =====")
    for k in KEYWORDS:
        if k in found:
            hits = sorted(found[k])
            print(f"\n[{k}]  ({len(hits)})")
            for h in hits[:12]:
                print("   ", h[:160])


if __name__ == "__main__":
    for p in sys.argv[1:]:
        scan(p)
