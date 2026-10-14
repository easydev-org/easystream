# easystream — the shared capability contract

Native, dependency-free implementations of the accessibility / input / capture
capability set, one per OS, each in that platform's first-class language:

| Platform | Language | a11y API | Input | Capture | H.264 |
|----------|----------|----------|-------|---------|-------|
| macOS   | Swift | AXUIElement | CGEvent | ScreenCaptureKit / CGWindowList | VideoToolbox |
| Windows | C#    | UI Automation | SendInput | WGC / GDI | Media Foundation / MFT |
| Android | Kotlin| AccessibilityService | dispatchGesture / KeyEvent | MediaProjection | MediaCodec |
| Linux   | Zig   | AT-SPI2 | XTest / wlr-virtual-* | X11 DRI3 / PipeWire (DMA-BUF) | VAAPI |

## Why native

The Go worker reaches these APIs through FFI (purego `dlopen`, hand-built COM
vtables, `offsetof` struct layouts). Native implementations call the same APIs
as first-class citizens and unlock what FFI cannot reach:

- Android `AccessibilityService` — only an installed App component can bind it.
- macOS `ScreenCaptureKit` — Objective-C, TCC-granted only to a signed `.app`.
- Windows WGC→MFT and Linux PipeWire/DRI3→VAAPI **zero-copy** (GPU handles).

## The daemon (unified contract)

**All implementations run as ONE long-lived HTTP/1.1 daemon on
`127.0.0.1:48079`** (override with `EAZYSTREAM_PORT`). It is started once by
the Go worker; the video stream is a single long-lived request, so there is
**no per-frame process spawn**.

```
GET  /health
GET  /apps
GET  /windows            [?app=NAME | ?pid=PID]
GET  /tree               [?app= | ?pid=] [&depth=N]
GET  /find?sel=SELECTOR  [&app= | &pid=] [&o=pretty|bounds|center]
POST /action             ?action=ACTION&sel=SELECTOR [&value=V]
POST /click              ?x=N&y=N [&button=left|right|middle] [&count=N]
POST /type               (body = text)
POST /key                ?name=KEY
POST /drag               ?x1=&y1=&x2=&y2= [&ms=N]
POST /scroll             ?x=&y= [&dx=N&dy=N]
GET  /screenshot         ?out=PATH [&region=X,Y,W,H]
GET  /clipboard          [?set=TEXT | ?get]
GET  /encode             [?fps=N&bitrate=N]   -> chunked Annex-B H.264
```

- `/encode` responds `transfer-encoding: chunked`, `content-type:
  application/octet-stream`, and streams one Annex-B access unit per HTTP
  chunk, forever (until the client disconnects). This is the worker's
  `NALSource`.
- Control endpoints return `text/plain` in the SAME formats as the one-shot
  CLI below (tab-separated `apps`, box-drawing `tree`, `w,h`/`x,y`/`w,h,x,y`).

## The CLI (one-shot, for debugging)

The same binary also runs one-shot commands (identical args + output as xa11y),
so a human or a script can drive it without the daemon:

```
a11y apps | windows | tree | find | action
a11y click | type | key | drag | scroll
a11y screenshot --out PATH [--region X,Y,W,H]
a11y clipboard [--set TEXT | --get]
a11y encode --out FILE|- [--frames N] [--fps N] [--bitrate N]
a11y serve            # run the daemon (default with no args)
```

## Capability map

| Capability | goa11y interface | macOS | Windows | Android | Linux |
|-----------|-----------------|-------|---------|---------|-------|
| a11y tree | `Provider.*` | AX | UIA | AccessibilityService | AT-SPI2 |
| a11y actions | `Provider.Press/...` | AXAction | UIA Patterns | performAction | AT-SPI2 Action |
| pointer | `InputProvider.Pointer*` | CGEvent | SendInput | dispatchGesture | XTest / wlr-virtual-pointer |
| keyboard | `InputProvider.KeyDown/TypeText` | CGEvent | SendInput | KeyEvent | XTest / wlr-virtual-keyboard |
| screenshot | `ScreenshotProvider.*` | SCK/CG | WGC | MediaProjection | XGetImage / PipeWire |
| H.264 | `VideoEncoder` | VideoToolbox | MFT | MediaCodec | VAAPI |
| clipboard | `Clipboard` | NSPasteboard | Win32 | ClipboardManager | xclip / wl-clipboard |
