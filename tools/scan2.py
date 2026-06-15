#!/usr/bin/env python3
import re, sys
KW = ["xrCreate","xrEnumerate","xrBegin","xrAcquire","xrLocate","XR_META","XR_FB",
      "XR_KHR","Spectator","spectator","Composition","CompositionLayer","Projection",
      "ProjectionLayer","eye","Eye","Fov","fov","Mrc","MRC","mrc","passthrough",
      "AMediaCodec","MediaCodec","encoder","Encoder","ANativeWindow","Surface",
      "eglCreate","EGLSurface","glReadPixels","capture","Capture","monoscopic",
      "thirdPerson","third_person","SpectatorCamera","instance extension","xrGetSystem"]
STR = re.compile(rb"[\x20-\x7e]{4,}")
def scan(path):
    data=open(path,"rb").read()
    found={}
    for m in STR.finditer(data):
        s=m.group().decode("ascii","ignore")
        for k in KW:
            if k in s: found.setdefault(k,set()).add(s)
    print(f"\n===== {path} =====")
    for k in KW:
        if k in found:
            hits=sorted(found[k]); print(f"\n[{k}] ({len(hits)})")
            for h in hits[:14]: print("   ",h[:150])
for p in sys.argv[1:]: scan(p)
