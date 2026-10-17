# easystream — the HTTP surface of the daemon

An **easystream daemon** (the native accessibility/input/capture process, one
per OS) exposes **one HTTP server**. It is the single thing a viewer or the Go
worker connects to:

```
GET  /                 the easystream web UI (index.html)
GET  /app.js           the web UI client
GET  /est/ws           WebSocket <-> est byte pipe (transparent; see EST.md)
GET  /health
GET  /capabilities     which path each capability resolved to (JSON)

# a11y (the worker uses these over HTTP; never over est)
GET  /a11y/apps
GET  /a11y/tree        [?app=NAME | ?pid=PID] [&depth=N]
GET  /a11y/find        ?sel=SELECTOR [&app= | &pid=] [&o=pretty|bounds|center]
POST /a11y/action      ?action=ACTION&sel=SELECTOR [&value=V] [&app= | &pid=]
GET  /a11y/screenshot  [?region=X,Y,W,H]        -> image/png
GET  /a11y/clipboard   [?set=TEXT]

# /capabilities
#
# Decided ONCE at startup (src/probe.rs) by asking the compositor which Wayland
# globals it exports and the portal which interfaces it exports — rather than
# trying one path, waiting for it to fail, and falling back. Useful because the
# paths differ wildly in cost and availability:
#
#   capture   zwlr_screencopy (wlroots, instant) | portal ScreenCast | X11
#   input     zwlr_virtual_pointer/keyboard      | portal RemoteDesktop | X11
#   a11y      AT-SPI over the session bus (no portal interface exists for this)
#   clipboard wl-copy | xclip/xsel | none
#
# e.g. GNOME (mutter exports no screencopy) and sway (wlroots):
#   {"session":"Wayland","capture":"Portal","input":"PortalRemoteDesktop",
#    "a11y":"Atspi","clipboard":"None",
#    "notes":["wayland globals=55 screencopy=false vptr=false vkbd=false",
#             "portal ScreenCast"]}
#   {"session":"Wayland","capture":"ZwlrScreencopy","input":"WlrVirtual",
#    "a11y":"None","clipboard":"Wl",
#    "notes":["wayland globals=47 screencopy=true vptr=true vkbd=true",
#             "org.a11y.Bus not owned"]}

# input (also HTTP: the worker's a11y surface never uses est)
GET  /a11y/click       ?x=N&y=N [&button=left|right|middle] [&count=N]
GET  /a11y/type        ?text=...        (POST body also accepted)
GET  /a11y/key         ?name=KEY
GET  /a11y/scroll      ?x=N&y=N [&dx=N&dy=N]
GET  /a11y/drag        ?x1=&y1=&x2=&y2= [&ms=N]
```

## Conventions

- Plain HTTP/1.1 on `0.0.0.0:$EAZYSTREAM_HTTP_PORT` (default **48078**).
- `text/plain; charset=utf-8` for the tree/list endpoints, in the SAME formats
  the one-shot CLI prints (tab-separated `apps`, box-drawing `tree`,
  `w,h` / `x,y` / `w,h,x,y`).
- `/a11y/screenshot` returns `image/png`.
- Errors are `text/plain` with a 4xx/5xx status.

## Why HTTP for a11y but est for the screen

The screen is a hot, long-lived, ordered byte stream — a raw socket (est) fits
it. The a11y surface is request/response over a tree that the daemon owns; HTTP
makes it trivially callable from the Go worker and from scripts, and it keeps
the worker a **pure forwarder** (it never parses the a11y payloads).

## The web UI

Every daemon embeds the easystream web UI and serves it at `/`. The UI is the
source of truth in **this repository's `webui/`** (index.html + app.js); each
daemon carries a copy as an embedded asset (`rust/webui/`,
`android/app/src/main/assets/webui/`, `macos/Sources/a11y/Resources/webui/`) so
it needs no network and no external repository.

The browser speaks the **est protocol directly** over `est/ws` (a transparent
byte pipe to the daemon's own est listener): video out, input in. All UI paths
are resolved RELATIVE to the page, so the daemon works whether it is reached at
`/` or behind a forwarder under a prefix (e.g. the worker's `/easystream/`). The
daemon does NOT parse the frames it relays on `est/ws`, so EST.md can evolve
without touching the pipe.
