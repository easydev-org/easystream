#!/usr/bin/env python3
"""PLI check for a CHANGE-DRIVEN capture (Windows WGC, Android VirtualDisplay).

Two traps this avoids, both of which produced false failures before:

  1. recv(2) can return ONE byte. TCP is a stream; treating a short read as a
     complete header desynchronises the whole parse and every later frame is
     lost. All reads below loop until the requested count is in hand.

  2. A static desktop emits NO frames. Reading one window, discarding it, then
     waiting for a second window sees nothing - not because the stream is
     broken but because nothing changed. The test keeps the pointer moving for
     the whole run and analyses ONE continuous window.
"""
import base64, socket, struct, sys, time
CRLF = chr(13) + chr(10)


def readn(s, n):
    buf = b""
    while len(buf) < n:
        c = s.recv(n - len(buf))
        if not c:
            break
        buf += c
    return buf


def ws_connect(host, port, path="/est/ws"):
    s = socket.create_connection((host, port), timeout=15)
    k = base64.b64encode(bytes(range(16))).decode()
    s.sendall(("GET " + path + " HTTP/1.1" + CRLF + "Host: h" + CRLF
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


def ws_payloads(s, seconds):
    """Collect ws payloads for `seconds`, tolerating quiet gaps.

    A change-driven capture sends in bursts: a 0.5 s silence is normal. Breaking
    out on the first socket.timeout threw away everything already buffered, which
    is why earlier runs reported 'no frames' while the daemon logged 'send 45 KB'
    on every tick.
    """
    s.settimeout(0.2)
    out = []
    end = time.time() + seconds
    while time.time() < end:
        try:
            h = readn(s, 2)
            if len(h) < 2:
                break
            ln = h[1] & 0x7F
            if ln == 126:
                ln = struct.unpack("!H", readn(s, 2))[0]
            elif ln == 127:
                ln = struct.unpack("!Q", readn(s, 8))[0]
            if ln:
                out.append(readn(s, ln))
        except socket.timeout:
            continue
        except Exception:
            break
    return out


def est_frames(chunks):
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


def nal_types(payload):
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


def poke(s, x, y):
    """Change the screen. WGC/MediaProjection only code on change."""
    ws_send(s, bytes([0x10, 0]) + struct.pack("!I", 4) + struct.pack("!HH", x, y))


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    path = sys.argv[3] if len(sys.argv) > 3 else "/est/ws"
    s = ws_connect(host, port, path)
    fr = est_frames(ws_payloads(s, 4))
    hellos = [pl for t, pl in fr if t == 0x01 and len(pl) >= 7]
    if not hellos:
        print("FAIL: no Hello"); return 1
    hp = hellos[0]
    print("hello: %dx%d fps=%d payload=%d" % ((hp[2]<<8)|hp[3], (hp[4]<<8)|hp[5], hp[6], len(hp)))

    # Keep the screen changing for the whole measurement, and collect ONE
    # window so nothing is discarded.
    collected = []
    deadline = time.time() + 12
    sent_pli = False
    pli_at = 0
    x = 200
    while time.time() < deadline:
        poke(s, x, 200 + (x % 300))
        x = 200 + ((x + 40) % 600)
        collected += ws_payloads(s, 0.5)
        if not sent_pli and time.time() > deadline - 7:
            frames = est_frames(collected)
            vids = [(t, pl) for t, pl in frames if t == 0x02]
            print("baseline: %d video frames, %d with IDR" % (len(vids), sum(1 for _, pl in vids if 5 in nal_types(pl))))
            ws_send(s, bytes([0x0A, 0]) + struct.pack("!I", 0))
            sent_pli = True
            pli_at = len(collected)

    frames = est_frames(collected)
    vids = [(t, pl) for t, pl in frames if t == 0x02]
    idrs = sum(1 for _, pl in vids if 5 in nal_types(pl))
    print("total: %d video frames, %d with IDR" % (len(vids), idrs))
    for t, pl in vids[:3]:
        print("  frame len=%d nal_types=%s" % (len(pl), nal_types(pl)))
    ok = len(vids) > 0 and idrs > 0
    print("RESULT:", "VIDEO+IDR OK" if ok else "FAILED")
    return 0 if ok else 2


if __name__ == "__main__":
    sys.exit(main())
