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


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    s = ws_connect(host, port)
    fr = est_frames(ws_payloads(s, 5))
    authoks = [pl for t, pl in fr if t == 0x04]
    print("authok:", [a.hex() for a in authoks], "(1=control, 0=view)")
    hellos = [pl for t, pl in fr if t == 0x01 and len(pl) >= 7]
    print("frames:", [(hex(t), len(pl)) for t, pl in fr][:6])
    if not hellos:
        print("FAIL: no Hello"); return 1
    print("before:", describe_hello(hellos[0]))

    # ConfigSet: 640x480, 30 fps, 3 Mbps, encoder untouched
    body = bytes([1]) + struct.pack("!H", 640)
    body += bytes([1]) + struct.pack("!H", 480)
    body += bytes([1, 30])
    body += bytes([1]) + struct.pack("!I", 3000000)
    body += bytes([0])
    # est frames ALWAYS carry the 6-byte header: type, reserved, u32 length.
    # Sending 0x08 followed straight by the body made the server read the first
    # four body bytes as a length, wait for ~16 MiB that never came, and drop the
    # frame - which is why no ack and no new Hello ever appeared.
    # The type byte lives in the HEADER, not in the payload. Putting it in both
    # places shifted every field by one: the server read hasWidth from the 0x08
    # type byte and then parsed width=257 out of what were really the flag and
    # the high byte of 640 - and correctly refused an odd size.
    payload = body
    frame = bytes([0x08, 0]) + struct.pack("!I", len(payload)) + payload
    print("sending ConfigSet:", frame.hex(), "len=", len(frame))
    ws_send(s, frame)

    fr2 = []
    deadline = time.time() + 12
    while time.time() < deadline:
        fr2 += est_frames(ws_payloads(s, 2))
        if any(t == 0x09 for t, _ in fr2) and any(
                t == 0x01 and len(p) >= 23 for t, p in fr2):
            break
    acks = [pl for t, pl in fr2 if t == 0x09]
    h2 = [pl for t, pl in fr2 if t == 0x01 and len(pl) >= 7]
    print("ack:", [a.hex() for a in acks] if acks else "NONE")
    if h2:
        print("after: ", describe_hello(h2[0]))
    else:
        print("after:  no new Hello")
    ok = bool(acks) and acks[0][0] == 1 and bool(h2)
    print("RESULT:", "CONFIG OK" if ok else "CONFIG FAILED")
    return 0 if ok else 2


if __name__ == "__main__":
    sys.exit(main())
