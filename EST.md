# easystream (`est://`) — the easystream wire protocol

`est://` is the SINGLE interface a easystream daemon exposes. Instead of a
dozen HTTP endpoints, one TCP connection multiplexes the video stream, input
injection and a11y queries — like VNC's RFB or RDP, but minimal and
WebCodecs-friendly (the video channel is raw Annex-B H.264).

The Go worker is a thin `est` client: it forwards everything and owns no a11y
logic itself.

## Transport

- TCP, default port **48079** (`EAZYSTREAM_PORT`). TLS is a future addition.
- All integers are **big-endian**.
- Every message is one frame:

```
 0        1        2                5                              N
+--------+--------+----------------+-------------------------------+
| type   | flags  | length (u32)   | payload (length bytes)        |
+--------+--------+----------------+-------------------------------+
```

`type` is one byte (below). `flags` is reserved (0). `length` is the payload
size in bytes (0 allowed). Unknown types MUST be skipped (read `length`).

## Handshake

On connect the server sends `Hello` immediately (no client preface):

```
0x01 Hello (S->C):
  u8  version      = 1
  u8  codec        1 = H.264 (Annex-B)
  u16 width
  u16 height
  u8  fps
  --- audio block (extension; a v1 client ignores the bytes past offset 7) ---
  u8  hasAudio     1 = Audio frames follow, 0 = no audio
  u8  audioCodec   1 = Opus
  u32 sampleRate   e.g. 48000
  u8  channels     1 or 2
  u8  frameMs      Opus frame duration (20)
```

The client then sends control frames; the server streams `Video` frames as they
are produced (it starts the capture+encode pipeline on connect).

## Video (S->C)

```
0x02 Video:
  bytes: one H.264 access unit in Annex-B (start codes 00 00 00 01), with SPS/PPS
         repeated before every IDR so a mid-stream join decodes.
```

This is the hot path: the server pushes one `Video` frame per encoded AU. A
client that only wants the stream sends nothing else.

## Audio (S->C)

```
0x05 Audio:
  bytes: one Opus packet (20 ms). Present only when Hello.hasAudio == 1.
```

## Auth (C->S, optional)

**View is the open default** — a client that sends nothing is read-only and
needs no token. Only CONTROL (input + a11y actions + clipboard writes) requires
the token configured in `EAZYSTREAM_CONTROL_TOKEN`.

A client asks for CONTROL by sending:

```
0x03 Auth (C->S):  bytes: utf8 token
0x04 AuthOk (S->C): u8 mode  (0 = view/read-only, 1 = control)
```

`AuthOk` ALWAYS returns a mode: a missing or wrong token simply yields `0`
(view). The HTTP pipe derives the mode from the request token (`?token=` or the
2nd Sec-WebSocket-Protocol item) and relays it to est with the same frame.

In VIEW mode the server DROPS every mutation frame: pointer (0x10-0x12),
keyboard (0x13-0x14), a11y ActionReq (0x26) and ClipSet (0x32).

## Pointer (C->S)

```
0x10 Move:    u16 x, u16 y
0x11 Button:  u8 button (1=left,2=middle,3=right), u8 down (1=press,0=release)
0x12 Scroll:  u16 x, u16 y, i16 dx, i16 dy
```

## Keyboard (C->S)

```
0x13 Key:     u8 nameLen, name[nameLen] (utf8, e.g. "Return","a","F5"),
              u8 down (1=press,0=release)
0x14 Text:    bytes: utf8 text to type
```

## a11y (request/response, C->S then S->C)

Each request carries a `u32 id`; the matching response echoes it (so many can
be in flight on the one connection).

```
0x20 AppsReq:  u32 id
0x21 AppsResp: u32 id, then repeated records:
                 u32 pid, u8 nameLen, name[nameLen], u8 focused
               terminated by a zero-length record (u32 pid=0xFFFFFFFF)

0x22 TreeReq:  u32 id, u32 pid (0 = focused app), u8 depth
0x23 TreeResp: u32 id, u32 textLen, text[textLen]   (box-drawing tree, as xa11y)

0x24 FindReq:  u32 id, u32 pid, u8 selLen, sel[selLen], u8 out (0=pretty,1=bounds,2=center)
0x25 FindResp: u32 id, u32 textLen, text[textLen]

0x26 ActionReq: u32 id, u8 actLen, act[actLen], u32 pid, u8 selLen, sel[selLen],
                u8 hasValue, (u8 valLen, val[valLen])?
0x27 ActionResp: u32 id, u8 ok, u8 msgLen, msg[msgLen]
```

## Clipboard

```
0x30 ClipGet:  u32 id
0x31 ClipData: u32 id, u32 len, text[len]
0x32 ClipSet:  u32 id, u32 len, text[len]
```

## Screenshot

```
0x40 ShotReq:  u32 id, u8 regionLen, region[regionLen]  ("" = full screen)
0x41 ShotResp: u32 id, u32 pngLen, png[pngLen]
```

## Keepalive / control

```
0x50 Ping:     bytes (opaque)
0x51 Pong:     bytes (echoed)
0x7F Close:    u8 reason
```

## Notes

- The server MUST handle each connection concurrently (the video push never
  blocks control handling on other connections), and within a connection it
  interleaves `Video` with responses to requests.
- The video pipeline (capture -> encode) is owned entirely by the server: the
  client never sees pixels before encoding, and never supplies frames.
- Input injection, a11y tree, capture and encode are all the SAME process
  (the platform's first-class language), so there is no cross-backend
  deadlock (the bug that killed the mixed uinput + wlr-virtual-pointer path).
