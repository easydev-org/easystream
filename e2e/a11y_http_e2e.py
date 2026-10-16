#!/usr/bin/env python3
"""End-to-end test for the easystream A11Y_HTTP surface.

It calls the REAL endpoints of a DEPLOYED daemon (through the worker's
`/easystream/` forwarder) and checks the contract in A11Y_HTTP.md holds on
every OS. Point it at one or more daemons:

    ./a11y_http_e2e.py https://x11test.../easystream \\
                       https://waylandtest.../easystream \\
                       https://wintest.../easystream \\
                       https://mactest.../easystream \\
                       https://androidtest.../easystream

Exit status is non-zero if any check fails, so it works as a CI gate.
"""

import struct
import sys
import urllib.parse
import urllib.request
import zlib


def png_stats(body):
    """Decode a PNG and return (w, h, distinct_colors, nonblack_ratio).

    A format-only check passes on a BLANK frame — which is exactly what an
    Xwayland capture of a Wayland desktop produced (its root window is empty).
    So the content must be inspected: a real desktop has many colours and
    non-black pixels. Returns None if the PNG cannot be decoded.
    """
    if body[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    i = 8
    w = h = colortype = None
    idat = b""
    while i + 8 <= len(body):
        ln = struct.unpack_from(">I", body, i)[0]
        typ = body[i + 4:i + 8]
        data = body[i + 8:i + 8 + ln]
        if typ == b"IHDR":
            w, h, _bd, colortype = struct.unpack_from(">IIBB", data, 0)
        elif typ == b"IDAT":
            idat += data
        elif typ == b"IEND":
            break
        i += 12 + ln
    if not w or not idat:
        return None
    bpp = {0: 1, 2: 3, 4: 2, 6: 4}.get(colortype)
    if bpp is None:
        return None
    raw = zlib.decompress(idat)
    stride = w * bpp
    out = bytearray()
    prev = bytearray(stride)
    pos = 0
    for _ in range(h):
        f = raw[pos]; pos += 1
        line = bytearray(raw[pos:pos + stride]); pos += stride
        if f == 1:
            for x in range(bpp, stride):
                line[x] = (line[x] + line[x - bpp]) & 0xff
        elif f == 2:
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 0xff
        elif f == 3:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 0xff
        elif f == 4:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                b = prev[x]
                c = prev[x - bpp] if x >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 0xff
        out += line
        prev = line
    colors = set()
    nonblack = total = 0
    for y in range(0, h, max(1, h // 60)):
        base = y * stride
        for x in range(0, w, max(1, w // 60)):
            o = base + x * bpp
            px = tuple(out[o:o + bpp])
            colors.add(px)
            if any(v > 16 for v in px[:3]):
                nonblack += 1
            total += 1
    return (w, h, len(colors), nonblack / max(1, total))


def get(base, path, **params):
    """GET base+path with query params; returns (status, content_type, body)."""
    url = base.rstrip("/") + path
    if params:
        url += "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"User-Agent": "easystream-e2e"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type", ""), e.read()


class Checks:
    def __init__(self, name):
        self.name = name
        self.passed = 0
        self.failed = []

    def check(self, label, ok, detail=""):
        if ok:
            self.passed += 1
            print(f"  PASS  {label}")
        else:
            self.failed.append(label)
            print(f"  FAIL  {label}  {detail}")

    def report(self):
        n = self.passed + len(self.failed)
        print(f"[{self.name}] {self.passed}/{n} passed")
        for f in self.failed:
            print(f"[{self.name}] FAILED: {f}")
        return not self.failed


def run(base):
    name = base.split("//")[-1].split(".")[0]
    c = Checks(name)

    # ── /a11y/apps: tab-separated pid\tname\t[focused] ──────────────────────
    st, ct, body = get(base, "/a11y/apps")
    text = body.decode("utf-8", "replace")
    c.check("apps: 200 + text/plain", st == 200 and ct.startswith("text/plain"), f"{st} {ct}")
    lines = [l for l in text.splitlines() if l.strip()]
    # An EMPTY list is legitimate: a bare X11/Wayland desktop has no ATK apps
    # registered with AT-SPI (xterm is Xt, not ATK). Only the FORMAT is
    # mandatory when apps exist.
    # The identifier is a pid (Windows/macOS) OR a D-Bus bus name (":1.1") on
    # newer at-spi2-core. Both are opaque to the caller; only "id<TAB>name" is
    # required.
    c.check(
        "apps: id<TAB>name",
        all(len(l.split("\t")) >= 2 and l.split("\t")[0] for l in lines[:5]),
        lines[:2],
    )

    # The first field is the app identifier: a pid on Windows/macOS, a D-Bus
    # bus name (":1.1") on newer at-spi2-core, where the pid is no longer
    # recoverable. It goes into ?app= (the pid form still works too).
    #
    # Pick the first app that actually HAS a walkable tree: system helpers like
    # macOS's loginwindow expose no window, so their tree is empty and that is
    # not a failure of the daemon.
    app = ""
    for l in lines:
        cand = l.split("\t")[0]
        st, _, body = get(base, "/a11y/tree", depth=3, app=cand)
        # A tree with only the root line (e.g. macOS loginwindow) has no
        # elements to find, so its find is legitimately empty — keep looking.
        if st == 200 and len(body.decode("utf-8", "replace").strip().splitlines()) > 1:
            app = cand
            break
    if not app and lines:
        app = lines[0].split("\t")[0]

    # App-dependent checks: skip on a desktop with no ATK apps (the daemon has
    # nothing to walk, so an empty result is correct, not a failure).
    if not app:
        c.check("apps: no ATK apps on this bare desktop (skipping tree/find)", True)
        return c.report()

    # ── /a11y/tree: box-drawing text ────────────────────────────────────────
    st, ct, body = get(base, "/a11y/tree", depth=3, app=app)
    text = body.decode("utf-8", "replace")
    c.check("tree: 200 + text/plain", st == 200 and ct.startswith("text/plain"), f"{st} {ct}")
    c.check("tree: non-empty", len(text.strip()) > 0)
    c.check("tree: has a role word", any(w in text for w in ("panel", "window", "frame", "application", "root", "desktop")), text[:80])

    # ── /a11y/find: pretty / bounds / center ────────────────────────────────
    st, ct, body = get(base, "/a11y/find", sel="*", o="bounds", app=app)
    text = body.decode("utf-8", "replace")
    c.check("find?o=bounds: 200", st == 200, f"{st}")
    first = next((l for l in text.splitlines() if l.strip()), "")
    c.check(
        "find?o=bounds: x,y,w,h",
        first.count(",") == 3 and all(p.lstrip("-").isdigit() for p in first.split(",")),
        first,
    )

    st, _, body = get(base, "/a11y/find", sel="*", o="center", app=app)
    text = body.decode("utf-8", "replace")
    first = next((l for l in text.splitlines() if l.strip()), "")
    # A11Y_HTTP.md: center is "x,y" (bounds is "w,h,x,y"). Every backend must
    # agree — a mismatch here was a real bug in the Windows port.
    c.check(
        "find?o=center: x,y",
        st == 200 and first.count(",") == 1 and all(p.lstrip("-").isdigit() for p in first.split(",")),
        first,
    )

    # ── /a11y/find?o=value: an element's CONTENT (e.g. a text area) ─────────
    st, _, body = get(base, "/a11y/find", sel="*", o="value", app=app)
    c.check("find?o=value: 200", st == 200, f"{st}")

    # ── /a11y/screenshot: PNG magic + real dimensions ───────────────────────
    st, ct, body = get(base, "/a11y/screenshot")
    c.check("screenshot: 200 + image/png", st == 200 and ct.startswith("image/png"), f"{st} {ct}")
    c.check("screenshot: PNG magic", body[:8] == b"\x89PNG\r\n\x1a\n", body[:8])
    stats = png_stats(body) if st == 200 else None
    if stats is None:
        c.check("screenshot: decodable PNG", False, "could not decode")
    else:
        w, h, colors, nonblack = stats
        c.check("screenshot: 1024x768", (w, h) == (1024, 768), f"{w}x{h}")
        # The content check that a format-only test cannot make: a BLANK frame
        # (an Xwayland capture of a Wayland desktop) has ~1 colour and no
        # non-black pixels, yet is a valid 1024x768 PNG.
        c.check("screenshot: not blank", colors >= 4, f"{colors} distinct colours")
        c.check("screenshot: has visible content", nonblack > 0.01, f"nonblack={nonblack:.3f}")

    # ── /a11y/clipboard: round-trip ─────────────────────────────────────────
    token = "easystream-e2e-clip"
    st, _, _ = get(base, "/a11y/clipboard", set=token)
    c.check("clipboard: set 200", st == 200, f"{st}")
    st, _, body = get(base, "/a11y/clipboard")
    c.check("clipboard: round-trip", st == 200 and body.decode("utf-8", "replace") == token, body[:40])

    # ── input verbs: every one must answer 200 ─────────────────────────────
    for label, path, params in [
        ("click", "/a11y/click", {"x": 400, "y": 300}),
        ("type", "/a11y/type", {"text": "x"}),
        ("key", "/a11y/key", {"name": "Return"}),
        ("scroll", "/a11y/scroll", {"x": 400, "y": 300, "dx": 0, "dy": 100}),
        ("drag", "/a11y/drag", {"x1": 300, "y1": 300, "x2": 400, "y2": 400, "ms": 100}),
    ]:
        st, _, body = get(base, path, **params)
        c.check(f"input {label}: 200", st == 200, f"{st} {body[:40]}")

    return c.report()


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    ok = True
    for base in argv[1:]:
        print(f"\n=== {base}")
        ok &= run(base)
    print("\n" + ("ALL PASSED" if ok else "FAILURES PRESENT"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
