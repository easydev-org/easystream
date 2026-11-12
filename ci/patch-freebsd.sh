#!/bin/sh
# Make easystream build on FreeBSD as an X11-only daemon.
#
# THIS IS THE SAME FOUR CHANGES THAT WERE MEASURED ON A REAL GHOSTBSD 14.1 GUEST, moved into
# a script so the CI job and a person's terminal run the identical thing. Each one is here
# because the build fails without it, and each failure is named.
#
#   1. build.rs has no freebsd branch, so it tries to compile the wlroots protocol sources
#      and link wayland-client / xkbcommon / libyuv - none of which the X11 path uses.
#
#   2. `probe.rs` calls `na_probe_wayland`, whose C body is in wlprobe.c. Not compiled on
#      FreeBSD, so the link fails with "undefined symbol: na_probe_wayland".
#
#   3. `x11shim.c` calls `memfd_create` with the glibc signature. FreeBSD has the syscall
#      since 13.0 but no glibc-style declaration in <sys/mman.h>.
#
#   4. `wayland.rs` and `portal.rs` are linked in by est.rs's enum variants. They cannot
#      build on FreeBSD (no wayland-client, no libpipewire), so they become stubs that keep
#      their public shape - est.rs is not edited at all.
#
# WHAT IS NOT CHANGED: est.rs, http.rs, a11y.rs, atspi.rs, x11.rs, x264.rs, opus.rs, png.rs,
# auth.rs, vcfg.rs. The AT-SPI backend works because at-spi2-core speaks D-Bus on FreeBSD
# too (`accessibility/at-spi2-core` is a maintained port).
set -eu

SRC=$(cd "$(dirname "$0")/.." && pwd)
cd "$SRC"

echo "=== 1. build.rs: a freebsd branch ==="
if ! grep -q freebsd build.rs; then
  python3 - <<'PY'
import re
p = "build.rs"
s = open(p).read()
marker = '    if target_os == "windows" {\n        return;\n    }\n'
assert marker in s, "build.rs no longer starts with the windows early-return"
branch = '''
    // FREBSD IS X11-ONLY. GhostBSD's desktop is Xorg, `probe.rs` picks Session::X11
    // because WAYLAND_DISPLAY is unset, and the wlroots protocol sources plus
    // wayland-client / xkbcommon / libyuv have no business in this build.
    //
    // What survives is exactly what src/x11shim.c includes: xcb, xcb-xtest, xcb-shm,
    // xcb-xfixes, xcb-render - plus the two encoders.
    if target_os == "freebsd" {
        for f in ["src/x264shim.cpp", "src/opusshim.cpp", "src/x11shim.c"] {
            println!("cargo:rerun-if-changed={f}");
        }
        cc::Build::new().file("src/x264shim.cpp").compile("na_x264");
        cc::Build::new().file("src/opusshim.cpp").compile("na_opus");
        cc::Build::new().file("src/x11shim.c").compile("na_x11");
        println!("cargo:rustc-link-lib=x264");
        println!("cargo:rustc-link-lib=opus");
        println!("cargo:rustc-link-lib=xcb");
        println!("cargo:rustc-link-lib=xcb-xtest");
        println!("cargo:rustc-link-lib=xcb-shm");
        println!("cargo:rustc-link-lib=xcb-xfixes");
        println!("cargo:rustc-link-lib=xcb-render");
        println!("cargo:rustc-link-lib=stdc++");
        return;
    }
'''
open(p, "w").write(s.replace(marker, marker + branch, 1))
print("  build.rs patched")
PY
else
  echo "  already patched"
fi

echo "=== 2. wayland.rs / portal.rs become stubs ==="
for pair in "wayland:Wayland" "portal:Portal"; do
  mod=${pair%%:*}
  f="src/$mod.rs"
  [ -f "$f.linux" ] || cp "$f" "$f.linux"
done

cat > src/wayland.rs <<'EOF'
//! The Wayland backends, absent on this platform.
//!
//! `est.rs` holds `Capture::Wl(Wayland)` and `Input::Wl(WlInput)` as enum variants and
//! matches on them in six places. Deleting the variants would mean editing those matches;
//! keeping them costs this file and avoids changing code that is not wrong.
//!
//! Nothing here fakes a frame or a click: every method says what a machine with no
//! compositor would say.
#![allow(dead_code)]

pub struct Wayland;

impl Wayland {
    pub fn open() -> Result<Wayland, String> {
        Err("the Wayland backend is not built on this target".into())
    }
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        Err("the Wayland backend is not built on this target".into())
    }
    pub fn grab_shm_ptr(&self) -> Option<(*const u8, u32, u32)> { None }
    pub fn close(&self) {}
}

pub struct WlInput;

impl WlInput {
    pub fn open() -> Result<WlInput, String> {
        Err("the Wayland input backend is not built on this target".into())
    }
    pub fn move_to(&self, _x: i16, _y: i16) {}
    pub fn set_output_size(&self, _w: u32, _h: u32) {}
    pub fn button(&self, _num: u8, _down: bool) {}
    pub fn scroll(&self, _dx: i32, _dy: i32) {}
    pub fn key(&self, _keycode: u8, _down: bool) {}
    pub fn close(&self) {}
}
EOF

cat > src/portal.rs <<'EOF'
//! The xdg-desktop-portal ScreenCast backend, absent on this platform.
//!
//! The portal path needs libpipewire and libdbus plus a D-Bus session owning a ScreenCast
//! handle. `build.rs` already skips compiling `portalshim.c` when those headers are
//! missing; this is the Rust side of the same decision, kept so `est.rs`'s
//! `Capture::Portal` arm keeps its shape.
#![allow(dead_code)]

pub struct Portal;

unsafe impl Send for Portal {}
unsafe impl Sync for Portal {}

impl Portal {
    pub fn open() -> Result<Portal, String> {
        Err("the portal backend is not built on this target".into())
    }
    pub fn size(&self) -> (u32, u32) { (0, 0) }
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        Err("the portal backend is not built on this target".into())
    }
    pub fn input_ok(&self) -> bool { false }
    pub fn move_to(&self, _x: f64, _y: f64) -> bool { false }
    pub fn button(&self, _code: i32, _down: bool) -> bool { false }
    pub fn axis(&self, _dx: f64, _dy: f64) -> bool { false }
    pub fn key(&self, _code: i32, _down: bool) -> bool { false }
    pub fn close(&self) {}
}
EOF
echo "  stubs written"

echo "=== 3. probe.rs: no na_probe_wayland on freebsd ==="
if ! grep -q 'target_os = "freebsd"' src/probe.rs; then
  python3 - <<'PY'
p = "src/probe.rs"
s = open(p).read()
old = '''extern "C" {
    fn na_probe_wayland(out: *mut libc::c_int) -> libc::c_int;
}

fn wayland_globals() -> Option<[i32; 8]> {
    let mut out = [0i32; 8];
    let rc = unsafe { na_probe_wayland(out.as_mut_ptr()) };
    if rc == 0 { Some(out) } else { None }
}'''
new = '''// wlprobe.c IS NOT COMPILED ON FREBSD, so the symbol below would not link. The answer it
// would give is known in advance: GhostBSD runs Xorg and exports no wlroots globals.
// Reporting None is what makes detect() take the `x_env -> Session::X11` branch.
#[cfg(not(target_os = "freebsd"))]
extern "C" {
    fn na_probe_wayland(out: *mut libc::c_int) -> libc::c_int;
}

#[cfg(not(target_os = "freebsd"))]
fn wayland_globals() -> Option<[i32; 8]> {
    let mut out = [0i32; 8];
    let rc = unsafe { na_probe_wayland(out.as_mut_ptr()) };
    if rc == 0 { Some(out) } else { None }
}

#[cfg(target_os = "freebsd")]
fn wayland_globals() -> Option<[i32; 8]> {
    None
}'''
assert old in s, "probe.rs's wayland probe changed shape"
open(p, "w").write(s.replace(old, new, 1))
print("  probe.rs patched")
PY
else
  echo "  already patched"
fi

echo "=== 4. x11shim.c: memfd_create -> shm_open(SHM_ANON) ==="
if ! grep -q SHM_ANON src/x11shim.c; then
  python3 - <<'PY'
p = "src/x11shim.c"
s = open(p).read()
old = '    int fd = memfd_create("xshm", MFD_CLOEXEC);'
new = '''#if defined(__FreeBSD__)
    // FREBSD HAS memfd_create(2) SINCE 13.0 BUT NO GLIBC-STYLE DECLARATION in sys/mman.h.
    // shm_open(SHM_ANON) is FreeBSD's own anonymous shared memory and is the same object
    // from the guest's point of view: an fd that ftruncate() sizes and mmap() maps.
    int fd = shm_open(SHM_ANON, O_RDWR, 0);
#else
    int fd = memfd_create("xshm", MFD_CLOEXEC);
#endif'''
assert old in s, "x11shim.c's memfd_create line changed"
s = s.replace(old, new, 1)
if "#include <fcntl.h>" not in s:
    s = s.replace("#include <sys/mman.h>", "#include <sys/mman.h>\n#include <fcntl.h>", 1)
open(p, "w").write(s)
print("  x11shim.c patched")
PY
else
  echo "  already patched"
fi

echo
echo "=== done; build with: cargo build --release ==="
