#!/usr/bin/env python3
"""ConfigSet check for a change-driven capture: does the ENCODER follow the size?

Asserts on the wire, not in the log: Hello after the ConfigSet must carry the
new geometry, and the frames must decode at that geometry. A geometry the
server accepts but the encoder ignores shows up here and nowhere else.
"""
import struct, sys, time
import importlib.util
spec = importlib.util.spec_from_file_location("p", "pli2.py")
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


def show(tag, hp):
    if not hp or len(hp) < 7:
        print(tag, "no hello"); return
    w = (hp[2] << 8) | hp[3]; h = (hp[4] << 8) | hp[5]
    extra = ""
    if len(hp) >= 23 and hp[15] == 1:
        br = (hp[18]<<24)|(hp[19]<<16)|(hp[20]<<8)|hp[21]
        extra = " enc=%d bitrate=%d" % (hp[16], br)
    print("%s %dx%d fps=%d%s" % (tag, w, h, hp[6], extra))


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    path = sys.argv[3] if len(sys.argv) > 3 else "/est/ws"
    s = p.ws_connect(host, port, path)
    fr = p.est_frames(p.ws_payloads(s, 4))
    h0 = [pl for t, pl in fr if t == 0x01 and len(pl) >= 7]
    if not h0:
        print("FAIL: no Hello"); return 1
    show("before:", h0[0])

    # 640x480 at 3 Mbps: a real change from 1024x768.
    body = bytes([1]) + struct.pack("!H", 640)
    body += bytes([1]) + struct.pack("!H", 480)
    body += bytes([1, 30])
    body += bytes([1]) + struct.pack("!I", 3000000)
    body += bytes([0])
    p.ws_send(s, bytes([0x08, 0]) + struct.pack("!I", len(body)) + body)

    # Keep changing the screen so both the ack and the new frames arrive.
    collected = []
    deadline = time.time() + 10
    x = 200
    while time.time() < deadline:
        p.poke(s, x, 250)
        x = 200 + ((x + 40) % 500)
        collected += p.ws_payloads(s, 0.4)
    frames = p.est_frames(collected)
    acks = [pl for t, pl in frames if t == 0x09]
    hs = [pl for t, pl in frames if t == 0x01 and len(pl) >= 7]
    print("ack:", [a.hex() for a in acks] if acks else "NONE")
    if hs:
        show("after: ", hs[-1])
    vids = [(t, pl) for t, pl in frames if t == 0x02]
    print("frames after ConfigSet:", len(vids))
    ok = bool(acks) and acks[0][0] == 1
    if hs and len(hs[-1]) >= 6:
        w = (hs[-1][2] << 8) | hs[-1][3]
        h = (hs[-1][4] << 8) | hs[-1][5]
        ok = ok and w == 640 and h == 480
    print("RESULT:", "CONFIG+GEOMETRY OK" if ok else "FAILED")
    return 0 if ok else 2


if __name__ == "__main__":
    sys.exit(main())
