"""Measure time-to-first-frame for ffmpeg H.264 pipe decode under *real-time*
access-unit pacing (what the live device stream looks like). Tries several flag
sets and reports which decode promptly vs. stall.

Usage: python tools/ff_latency_test.py <clip.h264> <W> <H> <fps>
"""
import subprocess, sys, threading, time

def split_aus(data):
    # Split Annex-B on 4-byte (00 00 00 01) / 3-byte (00 00 01) start codes.
    idxs = []
    i = 0
    n = len(data)
    while i + 3 < n:
        if data[i] == 0 and data[i+1] == 0:
            if data[i+2] == 1:
                idxs.append(i); i += 3; continue
            if data[i+2] == 0 and i+3 < n and data[i+3] == 1:
                idxs.append(i); i += 4; continue
        i += 1
    aus = []
    for k, s in enumerate(idxs):
        e = idxs[k+1] if k+1 < len(idxs) else n
        aus.append(data[s:e])
    return aus

def run(label, flags, clip, w, h, fps):
    data = open(clip, "rb").read()
    aus = split_aus(data)
    frame_bytes = w * h * 4
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error"] + flags + \
          ["-f", "h264", "-i", "pipe:0", "-an", "-f", "rawvideo",
           "-pix_fmt", "rgba", "-s", f"{w}x{h}", "pipe:1"]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL)
    start = time.time()
    first = [None]; total = [0]
    def reader():
        got = 0
        while True:
            chunk = p.stdout.read(frame_bytes)
            if not chunk or len(chunk) < frame_bytes:
                break
            got += 1
            if first[0] is None:
                first[0] = time.time() - start
            total[0] = got
    t = threading.Thread(target=reader, daemon=True); t.start()
    # Feed access units at real-time pace.
    dt = 1.0 / fps
    try:
        for au in aus:
            p.stdin.write(au); p.stdin.flush()
            time.sleep(dt)
    except BrokenPipeError:
        pass
    # Give it a moment to drain (still no EOF), then measure, then close.
    time.sleep(0.5)
    ttff = first[0]
    frames_before_eof = total[0]
    p.stdin.close()
    t.join(timeout=2)
    p.kill()
    print(f"{label:42} first_frame={'%.2fs'%ttff if ttff else 'NEVER':>7}  "
          f"frames_before_eof={frames_before_eof}  total={total[0]}")

if __name__ == "__main__":
    clip, w, h, fps = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
    cases = [
        ("probesize=500000 (current)", ["-probesize", "500000"]),
        ("probesize=32 analyze=0 lowdelay nobuf", ["-probesize", "32", "-analyzeduration", "0",
                                                   "-flags", "low_delay", "-fflags", "nobuffer"]),
        ("analyze=0 probesize=default", ["-analyzeduration", "0"]),
        ("probesize=32 analyze=0", ["-probesize", "32", "-analyzeduration", "0"]),
        ("probesize=1024 analyze=0", ["-probesize", "1024", "-analyzeduration", "0"]),
        ("analyzeduration=100000", ["-analyzeduration", "100000"]),
        ("probesize=8192", ["-probesize", "8192"]),
    ]
    for label, flags in cases:
        try:
            run(label, flags, clip, w, h, fps)
        except Exception as e:
            print(f"{label:42} ERROR {e}")
