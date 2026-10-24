#!/usr/bin/env python3
"""Run several ConfigSets in a row and check the geometry follows each one.

The single-change check passes on a daemon that only honours the FIRST change
(Android did exactly that: the projection token is single-use, so the second
createVirtualDisplay throws and every later change is acked and ignored).
Alternating sizes is what catches it.
"""
import struct, sys, time, importlib.util
spec = importlib.util.spec_from_file_location("p", "pli2.py")
p = importlib.util.module_from_spec(spec); spec.loader.exec_module(p)


def geom(hp):
    if len(hp) >= 27 and hp[15] == 1 and (hp[17] & 4):
        return ((hp[23] << 8) | hp[24], (hp[25] << 8) | hp[26])
    return ((hp[2] << 8) | hp[3], (hp[4] << 8) | hp[5])


def one(host, port, path, tw, th, secs=12):
    s = p.ws_connect(host, port, path)
    fr = p.est_frames(p.ws_payloads(s, 4))
    h0 = [pl for t, pl in fr if t == 0x01 and len(pl) >= 7]
    before = geom(h0[0]) if h0 else None
    body = (bytes([1]) + struct.pack("!H", tw) + bytes([1]) + struct.pack("!H", th)
            + bytes([1, 30]) + bytes([1]) + struct.pack("!I", 3000000) + bytes([0]))
    p.ws_send(s, bytes([0x08, 0]) + struct.pack("!I", len(body)) + body)
    col = []; dl = time.time() + secs; x = 200
    while time.time() < dl:
        p.poke(s, x, 250); x = 200 + ((x + 40) % 500)
        col += p.ws_payloads(s, 0.4)
    fr2 = p.est_frames(col)
    acks = [pl for t, pl in fr2 if t == 0x09]
    hs = [pl for t, pl in fr2 if t == 0x01 and len(pl) >= 7]
    vids = [(t, pl) for t, pl in fr2 if t == 0x02]
    after = geom(hs[-1]) if hs else None
    ok = bool(acks) and acks[0][0] == 1 and after == (tw, th) and len(vids) > 0
    print("  %dx%d: before=%s ack=%s after=%s frames=%d -> %s"
          % (tw, th, before, acks[0][0] if acks else None, after, len(vids),
             "OK" if ok else "FAILED"))
    return ok


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    path = sys.argv[3] if len(sys.argv) > 3 else "/est/ws"
    sizes = [(640, 480), (800, 600), (640, 480)]
    results = [one(host, port, path, w, h) for w, h in sizes]
    print("RESULT:", "%d/%d changes OK" % (sum(results), len(results)))
    return 0 if all(results) else 2


if __name__ == "__main__":
    sys.exit(main())
