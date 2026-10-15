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

import sys
import urllib.parse
import urllib.request


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
    c.check(
        "apps: pid<TAB>name",
        all(len(l.split("\t")) >= 2 and l.split("\t")[0].isdigit() for l in lines[:5]),
        lines[:2],
    )

    # A pid to use for the tree/find checks: the focused one when marked.
    pid = ""
    for l in lines:
        parts = l.split("\t")
        # pid 0 is not a real process (some registries list a helper with pid 0);
        # a tree walk needs a real pid.
        if len(parts) >= 3 and "focused" in parts[2] and parts[0] != "0":
            pid = parts[0]
            break
    if not pid and lines:
        cand = lines[0].split("\t")[0]
        pid = cand if cand != "0" else ""

    # App-dependent checks: skip on a desktop with no ATK apps (the daemon has
    # nothing to walk, so an empty result is correct, not a failure).
    if not pid:
        c.check("apps: no ATK apps on this bare desktop (skipping tree/find)", True)
        return c.report()

    # ── /a11y/tree: box-drawing text ────────────────────────────────────────
    st, ct, body = get(base, "/a11y/tree", depth=3, **({"pid": pid} if pid else {}))
    text = body.decode("utf-8", "replace")
    c.check("tree: 200 + text/plain", st == 200 and ct.startswith("text/plain"), f"{st} {ct}")
    c.check("tree: non-empty", len(text.strip()) > 0)
    c.check("tree: has a role word", any(w in text for w in ("panel", "window", "frame", "application", "root", "desktop")), text[:80])

    # ── /a11y/find: pretty / bounds / center ────────────────────────────────
    st, ct, body = get(base, "/a11y/find", sel="*", o="bounds", **({"pid": pid} if pid else {}))
    text = body.decode("utf-8", "replace")
    c.check("find?o=bounds: 200", st == 200, f"{st}")
    first = next((l for l in text.splitlines() if l.strip()), "")
    c.check(
        "find?o=bounds: x,y,w,h",
        first.count(",") == 3 and all(p.lstrip("-").isdigit() for p in first.split(",")),
        first,
    )

    st, _, body = get(base, "/a11y/find", sel="*", o="center", **({"pid": pid} if pid else {}))
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
    st, _, body = get(base, "/a11y/find", sel="*", o="value", **({"pid": pid} if pid else {}))
    c.check("find?o=value: 200", st == 200, f"{st}")

    # ── /a11y/screenshot: PNG magic + real dimensions ───────────────────────
    st, ct, body = get(base, "/a11y/screenshot")
    c.check("screenshot: 200 + image/png", st == 200 and ct.startswith("image/png"), f"{st} {ct}")
    c.check("screenshot: PNG magic", body[:8] == b"\x89PNG\r\n\x1a\n", body[:8])
    if len(body) > 24:
        w = int.from_bytes(body[16:20], "big")
        h = int.from_bytes(body[20:24], "big")
        c.check("screenshot: 1024x768", (w, h) == (1024, 768), f"{w}x{h}")

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
