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

### Video block (Hello extension, appended at offset 15)

Video used to be described by four bytes at offsets 2-6 and nothing else, so a
client could not tell a hardware encoder from a software one, or learn the
bitrate it was about to receive. The block below is APPENDED, never interleaved:
every existing field keeps its offset, and a client that reads only up to the
audio block (offset 14) parses a Hello exactly as before.

```
offset 15: u8  hasVideoConfig  1 = the fields below are present
          16: u8  encoder       1 = x264 (software)   2 = VAAPI
                                3 = Media Foundation  4 = VideoToolbox
                                5 = MediaCodec        0 = unknown
          17: u8  flags         bit0 = geometry/fps/bitrate can be changed at
                                       run time via ConfigSet
                                bit1 = the encoder is hardware accelerated
          18: u32 bitrate       bits per second, big-endian
          22: u8  keyint        keyframe interval in frames
```

`hasVideoConfig = 0` means the server does not implement this block: treat the
geometry at offsets 2-6 as the whole truth and offer the user no quality control.

A client MUST re-read Hello on every reconfiguration. The server sends a fresh
Hello after applying a `ConfigSet`, and that is what tells the client to
reconfigure its decoder; Hello is the only authoritative statement of geometry.

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

Audio is opt-in per connection:

```
0x06 AudioReq (C->S): empty   turn Audio frames ON for this connection
0x07 AudioOff (C->S): empty   turn Audio frames OFF (video is unaffected)
```

The codec is Opus at 48 kHz, 2 channels, 20 ms frames. How each platform
captures the *system* audio and encodes it:

| Platform | Capture | Encoder | Notes |
|----------|---------|---------|-------|
| Linux | PulseAudio monitor source | libopus | |
| Windows | WASAPI loopback | libopus (mingw, static) | no endpoint ⇒ `hasAudio = 0` |
| Android | `AudioPlaybackCapture` | platform `MediaCodec` | needs MediaProjection + `RECORD_AUDIO` |
| macOS | BlackHole loopback in a clocked aggregate device, read via AUHAL | AudioToolbox `kAudioFormatOpus` | needs the BlackHole HAL plug-in; ScreenCaptureKit's `capturesAudio` does not work in a VM |

Capture failure is never fatal: the daemon advertises `hasAudio = 0` in Hello
and simply sends no `0x05` frames.

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

## Reconfiguration (quality / encoder)

The server owns the encoder, so the client asks for changes rather than making
them. Every field carries its own `has*` flag, so a client can change bitrate
without touching geometry, and the server rebuilds the encoder only when
something actually differs.

```
0x08 ConfigSet (C->S):
  u8  hasWidth,   u16 width       (0 = unchanged)
  u8  hasHeight,  u16 height      (0 = unchanged)
  u8  hasFps,     u8  fps         (0 = unchanged)
  u8  hasBitrate, u32 bitrate     (0 = unchanged; bits per second)
  u8  hasEncoder, u8  encoder     (0 = unchanged, 0xFF = let the server pick)

0x09 ConfigAck (S->C):
  u8  ok          1 = applied
  u8  errLen, err[errLen]        UTF-8 reason when ok = 0
```

A successful ConfigSet always produces, in this order:

1. the encoder is torn down and rebuilt with the new parameters,
2. the next encoded frame is forced to be an IDR (so nothing waits for a natural
   keyframe),
3. a fresh `Hello` is sent, carrying the new geometry and video block,
4. `ConfigAck` with `ok = 1`.

Sending Hello BEFORE the first new `Video` frame is what lets the client
reconfigure `VideoDecoder` in time; a client that reconfigures after the first
frame sees exactly one garbled frame, which is why the order is fixed here.

Refusals are honest: an unsupported `encoder`, a geometry the capture source
cannot produce, or a bitrate outside the sane range must answer `ok = 0` with a
reason, and leave the running configuration untouched.

A client in VIEW mode MUST NOT be able to reconfigure (see Auth): quality is a
control-plane operation and is dropped with the other mutations.

### Quality presets

The UI offers three named presets, and every platform must map them the same way
so that "高清" means the same thing on Linux, Windows, macOS and Android. The
presets are a CLIENT-side convenience: they are expanded into a ConfigSet with
explicit fields, so the wire protocol itself never carries a preset name.

| Preset | UI label | Scale | fps | Bitrate |
|--------|----------|-------|-----|---------|
| 0 | 标清 (SD)     | longest side 1280 | 30 | 3 Mbps |
| 1 | 高清 (HD)     | longest side 1920 | 30 | 8 Mbps |
| 2 | 原画 (native) | capture size, unchanged | 30 | 20 Mbps |

Scaling is applied after capture and before encode, preserving aspect ratio and
rounding to even dimensions (H.264 requires even width and height in 4:2:0).

"原画" sends the capture geometry unchanged, which is the only preset that
guarantees no rescale — and therefore the only one whose CPU cost is not
dominated by the scaler. A server that cannot scale (a platform whose capture
source only offers one size) may refuse presets 0 and 1 with ok = 0; it must not
pretend to have applied them.


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
