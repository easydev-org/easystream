#!/usr/bin/env python3
"""End-to-end check of the est ConfigSet path over the worker websocket.

IMPORTANT: the ws pipe forwards every read() as its own websocket frame, so one
est frame can arrive split across several ws frames (a 29-byte Hello was seen as
a 6-byte frame followed by a 23-byte one). Treat the ws payloads as a BYTE
STREAM and parse est frames out of it, never one-ws-frame-per-est-frame.
"""
import base64, socket, struct, sys, time
CRLF = chr(13) + chr(10)


def ws_connect(host, port):
    s = socket.create_connection((host, port), timeout=15)
    k = base64.b64encode(bytes(range(16))).decode()
    s.sendall(("GET /desktop/est/ws HTTP/1.1" + CRLF + "Host: h" + CRLF
               + "Upgrade: websocket" + CRLF + "Connection: Upgrade" + CRLF
               + "Sec-WebSocket-Key: " + k + CRLF + "Sec-WebSocket-Version: 13" + CRLF
               + "Sec-WebSocket-Protocol: est, worker-token" + CRLF + CRLF).encode())
    buf = b""
    while (CRLF + CRLF).encode() not in buf:
        buf += s.recv(4096)
    return s


def ws_send(s, payload):
    n = len(payload)
    h = (struct.pack("!BB", 0x82, 0x80 | n) if n < 126
         else struct.pack("!BBH", 0x82, 0x80 | 126, n))
    m = bytes([1, 2, 3, 4])
    s.sendall(h + m + bytes(payload[i] ^ m[i % 4] for i in range(n)))


def ws_payloads(s, t):
    """Raw ws payloads only; reassembly into est frames happens in est_frames."""
    s.settimeout(t)
    out = []
    end = time.time() + t
    while time.time() < end:
        try:
            h = s.recv(2)
            if len(h) < 2:
                break
            ln = h[1] & 0x7F
            if ln == 126:
                ln = struct.unpack("!H", s.recv(2))[0]
            elif ln == 127:
                ln = struct.unpack("!Q", s.recv(8))[0]
            d = b""
            while len(d) < ln:
                c = s.recv(ln - len(d))
                if not c:
                    break
                d += c
            if d:
                out.append(d)
        except socket.timeout:
            break
        except Exception:
            break
    return out


def est_frames(chunks):
    """Reassemble est frames from the concatenated ws payloads."""
    buf = b"".join(chunks)
    frames = []
    while len(buf) >= 6:
        typ = buf[0]
        ln = struct.unpack("!I", buf[2:6])[0]
        if len(buf) < 6 + ln:
            break
        frames.append((typ, buf[6:6 + ln]))
        buf = buf[6 + ln:]
    return frames


def describe_hello(pl):
    w = (pl[2] << 8) | pl[3]
    h = (pl[4] << 8) | pl[5]
    s = "%dx%d fps=%d paylen=%d" % (w, h, pl[6], len(pl))
    if len(pl) >= 23 and pl[15] == 1:
        br = (pl[18] << 24) | (pl[19] << 16) | (pl[20] << 8) | pl[21]
        s += " enc=%d flags=0x%02x bitrate=%d" % (pl[16], pl[17], br)
    return s



import re

def nal_types(payload):
    """NAL unit types in an Annex-B buffer.

    Both start-code lengths must be handled: x264 writes 00 00 00 01 before SPS
    and 00 00 01 before the slice. A detector that only looks for the long form
    sees a 15 KB frame containing "only SPS+PPS" and reports no IDR on a
    perfectly good keyframe - which is exactly how this test first failed.
    """
    out = []
    i, n = 0, len(payload)
    while i + 3 < n:
        if payload[i] == 0 and payload[i+1] == 0:
            if payload[i+2] == 1:
                if i + 3 < n: out.append(payload[i+3] & 0x1f)
                i += 3
                continue
            if payload[i+2] == 0 and i + 3 < n and payload[i+3] == 1:
                if i + 4 < n: out.append(payload[i+4] & 0x1f)
                i += 4
                continue
        i += 1
    return out

def idr_count(payload):
    return 1 if 5 in nal_types(payload) else 0

def main():
    host, port = sys.argv[1], int(sys.argv[2])
    s = ws_connect(host, port)
    fr = est_frames(ws_payloads(s, 6))
    hellos = [pl for t, pl in fr if t == 0x01 and len(pl) >= 7]
    if not hellos:
        print("FAIL: no Hello"); return 1
    print("hello:", describe_hello(hellos[0]))

    # Baseline: watch N consecutive video frames WITHOUT asking for anything.
    # A keyint of 2 s at 30 fps means an IDR every ~60 frames, so a short
    # window should contain none (or at most one).
    base = est_frames(ws_payloads(s, 3))
    base_v = [(t, pl) for t, pl in base if t == 0x02]
    base_idr = sum(1 for _, pl in base_v if idr_count(pl))
    print(f"baseline: {len(base_v)} frames, {base_idr} with IDR")

    # Make the screen change: a static desktop produces NO frames, so there is
    # nothing for the IDR to attach to. Moving the pointer forces a redraw.
    import time as _t
    for i in range(12):
        mv = bytes([0x10, 0]) + struct.pack("!I", 4) + struct.pack("!HH", 300 + i * 20, 300)
        ws_send(s, mv)
        _t.sleep(0.05)
    warm = est_frames(ws_payloads(s, 2))
    warm_v = [(t, pl) for t, pl in warm if t == 0x02]
    print(f"after pointer moves: {len(warm_v)} frames")

    # Now ask for a keyframe and look for an IDR immediately after.
    frame = bytes([0x0A, 0]) + struct.pack("!I", 0)
    print("sending KeyFrameReq:", frame.hex())
    ws_send(s, frame)
    # Keep the screen changing while we wait for the IDR.
    for i in range(16):
        mv = bytes([0x10, 0]) + struct.pack("!I", 4) + struct.pack("!HH", 600 - i * 20, 500)
        ws_send(s, mv)
        _t.sleep(0.05)
    after = est_frames(ws_payloads(s, 3))
    after_v = [(t, pl) for t, pl in after if t == 0x02]
    after_idr = sum(1 for _, pl in after_v if idr_count(pl))
    # Show what is actually in the stream: Annex-B start codes and NAL types.
    for t, pl in after_v[:6]:
        print(f"  frame len={len(pl)} nal_types={nal_types(pl)}")
    print(f"after PLI: {len(after_v)} frames, {after_idr} with IDR")

    ok = after_idr >= 1
    print("RESULT:", "PLI OK" if ok else "PLI FAILED")
    return 0 if ok else 2

if __name__ == "__main__":
    sys.exit(main())