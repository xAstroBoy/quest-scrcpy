#!/usr/bin/env python3
"""Recover a wedged / laggy wireless Quest that's stuck 'offline'. Keep
connecting (bare IP, like you do by hand) and fire a reboot every cycle through
every route, so the instant the device surfaces for even a moment, it lands.

    python tools/recover_reboot.py [ip]        (default 192.168.1.35)

Ctrl-C to stop. If wireless debugging doesn't persist across reboots you'll need
to re-pair once it's back.
"""
import subprocess
import sys
import time

IP = (sys.argv[1] if len(sys.argv) > 1 else "192.168.1.35").split(":")[0]
BAD = ("offline", "error", "closed", "no devices", "not found", "cannot",
       "unable", "timeout", "unauthorized", "failed", "daemon", "more than one")


def adb(args, timeout=8):
    try:
        p = subprocess.run(["adb"] + args, capture_output=True, text=True, timeout=timeout)
        return p.returncode, ((p.stdout or "") + (p.stderr or "")).strip()
    except subprocess.TimeoutExpired:
        return 124, "timeout"
    except FileNotFoundError:
        print("adb not found on PATH"); sys.exit(1)


print(f"[recover] hammering reboot at {IP} until it surfaces. Ctrl-C to stop.")
attempt = 0
while True:
    attempt += 1
    adb(["connect", IP], 6)  # bare IP, exactly like `adb connect 192.168.1.35`

    # Fire reboot through every route; with one device, plain `adb reboot` works.
    landed = False
    for args in (["reboot"], ["-s", IP, "reboot"], ["-s", f"{IP}:5555", "reboot"]):
        rc, out = adb(args, 8)
        low = out.lower()
        if rc == 0 and not any(w in low for w in BAD):
            print(f"[{attempt}] `adb {' '.join(args)}` -> OK")
            landed = True
            break
        if "unauthorized" in low:
            print(f"[{attempt}] reachable but UNAUTHORIZED — trust this PC in the headset prompt.")

    if attempt <= 5 or attempt % 10 == 0:
        print(f"[{attempt}] still down ({out!r})…")

    if landed:
        print("[recover] reboot accepted — the headset is restarting now.")
        print(f"[recover] give it ~30-60s, then: adb connect {IP}")
        break

    time.sleep(0.8)
