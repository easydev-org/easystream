# easystream

The **easystream daemon**: one process per OS that owns the screen (H.264 over
`est://`), the input, the a11y tree and the web UI. It is the only thing a
viewer or the Go worker connects to.

```
browser ──HTTP/WS──▶ daemon ──┬─ GET /            the easystream web UI
                              ├─ GET /est/ws      transparent est byte pipe
                              └─ GET /a11y/<...>  the a11y / input HTTP API
worker  ──forward──▶ daemon (the worker never parses est or a11y)
```

| Platform | Language | a11y API | Input | Capture | H.264 |
|----------|----------|----------|-------|---------|-------|
| Linux   | Rust   | AT-SPI2 (D-Bus) | XTest / wlr-virtual-* | X11 DRI3 / wlroots screencopy | libx264 |
| Windows | Rust   | UI Automation | `SendInput` | WGC / GDI | Media Foundation |
| macOS   | Swift  | `AXUIElement` | `CGEvent` | ScreenCaptureKit / CGWindowList | VideoToolbox |
| Android | Kotlin | `AccessibilityService` | `dispatchGesture` / `input` | `MediaProjection` | `MediaCodec` |

## Why native

The Go worker reaches these APIs through FFI (purego `dlopen`, hand-built COM
vtables, `offsetof`-derived struct layouts). Native implementations call the
same APIs as first-class citizens and unlock what FFI cannot reach:

- **Android `AccessibilityService`** — only an installed App component can bind
  it; a plain binary cannot, in any language.
- **macOS `ScreenCaptureKit`** — Objective-C, TCC-granted only to a signed `.app`.

## The contract

Two documents define it:

- **`EST.md`** — the `est://` wire protocol (video + input on one TCP socket).
- **`A11Y_HTTP.md`** — the daemon's HTTP surface (web UI, `/est/ws` pipe,
  `/a11y/*`).

Every daemon also runs the **same command surface** as the goa11y CLI, so it is
a drop-in for the worker's a11y backend. See `SPEC.md`:

```
a11y apps
a11y windows   [--app NAME|--pid PID]
a11y tree      [--app NAME|--pid PID] [--depth N]
a11y find      SELECTOR [--app NAME|--pid PID] [-o pretty|bounds|center]
a11y action    ACTION SELECTOR [--app NAME|--pid PID] [--value V]
a11y click     --at X,Y [--button left|right|middle] [--count N]
a11y type      TEXT
a11y key       KEY
a11y drag      --from X,Y --to X,Y [--duration-ms N]
a11y scroll    --at X,Y [--dx N] [--dy N]
a11y screenshot --out PATH [--region X,Y,W,H]
a11y clipboard [--set TEXT]
a11y encode    --out FILE [--frames N] [--fps N] [--bitrate N]
```

Output formats match xa11y (tab-separated `apps`, box-drawing `tree`,
`w,h` / `x,y` / `w,h,x,y` for `find -o`).

## Build

**Linux / Windows** (Rust; libdbus-1 + libx264 on Linux, mingw-w64 for the
cross-built Windows PE):
```
cd rust && cargo build --release
cd rust && ./build-windows.sh          # x86_64-pc-windows-gnu
```

**macOS** (Swift Package, system frameworks only):
```
cd macos && swift build -c release
```

**Android** (Gradle + Android SDK 35):
```
cd android && gradle assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
# enable the accessibility service, then grant MediaProjection
adb shell settings put secure enabled_accessibility_services dev.easystream/dev.easystream.A11yService
adb shell appops set dev.easystream PROJECT_MEDIA allow
```

## Notes

- **Android** runs as an installed APK exposing the daemon's HTTP surface
  (`127.0.0.1:48080`) plus the est socket (`127.0.0.1:48079`).
- **macOS capture** needs Screen Recording (TCC). On a dev box with SIP off and
  passwordless sudo the grant can be scripted; otherwise use the System Settings
  toggle. If the process is spawned by another binary, TCC attributes the request
  to that **responsible** parent — grant it there.
- **Android capture** on a headless emulator needs periodic UI changes to produce
  frames (MediaProjection only pushes on screen change); the recorder nudges the
  UI while running.
