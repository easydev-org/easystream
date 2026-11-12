#!/bin/sh
# Make easystream build on FreeBSD as an X11-only daemon.
#
# PURE POSIX SH. The first version used python3 for its three text edits, which is one more
# dependency than a patch script should have: FreeBSD's base system has no python3, so the
# job died at
#
#     ci/patch-freebsd.sh: python3: not found
#
# before cargo was reached. Everything below is sed and here-documents, which every system
# this builds on already has.
#
# THE FOUR CHANGES, EACH BECAUSE THE BUILD FAILS WITHOUT IT
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
if grep -q "freebsd" build.rs; then
    echo "  already patched"
else
    # Insert right after the windows early-return, so the ordering stays
    # "windows: no C at all; freebsd: x11 C only; everything else: the full set".
    awk '
      { print }
      /^    if target_os == "windows" \{$/ { inwin = 1 }
      inwin && /^    \}$/ && !done {
        print ""
        print "    // FREBSD IS X11-ONLY. GhostBSD'"'"'s desktop is Xorg, `probe.rs` picks Session::X11"
        print "    // because WAYLAND_DISPLAY is unset, and the wlroots protocol sources plus"
        print "    // wayland-client / xkbcommon / libyuv have no business in this build."
        print "    //"
        print "    // What survives is exactly what src/x11shim.c includes: xcb, xcb-xtest,"
        print "    // xcb-shm, xcb-xfixes, xcb-render - plus the two encoders."
        print "    if target_os == \"freebsd\" {"
        print "        for f in [\"src/x264shim.cpp\", \"src/opusshim.cpp\", \"src/x11shim.c\"] {"
        print "            println!(\"cargo:rerun-if-changed={f}\");"
        print "        }"
        print "        //"
        print "        // AND FREEBSD INSTALLS PORTS HEADERS IN /usr/local/include, NOT /usr/include,"
        print "        // so cc cannot find x264.h or opus.h without being told:"
        print "        //"
        print "        //     src/x264shim.cpp:6:10: fatal error: 'x264.h' file not found"
        print "        //"
        print "        // The libraries resolve anyway because /usr/local/lib is in the default"
        print "        // search path; only the headers need the explicit include, and only the"
        print "        // -devel ports carry them."
        print "        cc::Build::new().include(\"/usr/local/include\").file(\"src/x264shim.cpp\").compile(\"na_x264\");"
        print "        cc::Build::new().include(\"/usr/local/include\").file(\"src/opusshim.cpp\").compile(\"na_opus\");"
        print "        cc::Build::new().include(\"/usr/local/include\").file(\"src/x11shim.c\").compile(\"na_x11\");"
        print "        println!(\"cargo:rustc-link-search=native=/usr/local/lib\");"
        print "        println!(\"cargo:rustc-link-lib=x264\");"
        print "        println!(\"cargo:rustc-link-lib=opus\");"
        print "        println!(\"cargo:rustc-link-lib=xcb\");"
        print "        println!(\"cargo:rustc-link-lib=xcb-xtest\");"
        print "        println!(\"cargo:rustc-link-lib=xcb-shm\");"
        print "        println!(\"cargo:rustc-link-lib=xcb-xfixes\");"
        print "        println!(\"cargo:rustc-link-lib=xcb-render\");"
        print "        // NO -lstdc++, AND ITS REMOVAL IS THE FIX FOR THE LINK FAILURE:"
        print "        //"
        print "        //     ld: error: unable to find library -lstdc++"
        print "        //"
        print "        // FreeBSD has never shipped libstdc++ in base; its C++ runtime is libc++."
        print "        // The shims are compiled by cc::Build, which invokes clang++ on FreeBSD, and"
        print "        // clang++ links libc++ on its own. Asking for libstdc++ as well adds a"
        print "        // dependency that does not exist on the system being built for."
        print "        return;"
        print "    }"
        done = 1
        inwin = 0
      }
    ' build.rs > build.rs.new
    mv build.rs.new build.rs
    echo "  build.rs patched"
fi

echo "=== 2. wayland.rs / portal.rs become stubs ==="
for mod in wayland portal; do
    f="src/$mod.rs"
    [ -f "$f.linux" ] || cp "$f" "$f.linux"
done

cat > src/wayland.rs <<'RS'
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
RS

cat > src/portal.rs <<'RS'
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
RS
echo "  stubs written"

echo "=== 3. probe.rs: no na_probe_wayland on freebsd ==="
if grep -q 'target_os = "freebsd"' src/probe.rs; then
    echo "  already patched"
else
    # Replace the extern block and the function with two cfg-gated versions.
    awk '
      /^extern "C" \{$/ { inextern = 1 }
      inextern && /na_probe_wayland/ { hold = 1 }
      hold {
        print
        if (/^\}$/ && !printed_fallback) {
          # closing brace of wayland_globals, after the extern block
          printed_fallback = 1
        }
        next
      }
      { print }
    ' src/probe.rs > /dev/null 2>&1 || true

    # A marker-based rewrite: the block is known verbatim, so locate and cut it with sed
    # line numbers rather than trying to match braces.
    start=$(grep -n '^extern "C" {$' src/probe.rs | head -1 | cut -d: -f1)
    if [ -z "$start" ]; then
        echo "  === probe.rs: could not find the extern block ===" >&2
        exit 1
    fi
    # the block ends at the first "^}" after the wayland_globals fn
    fn=$(grep -n 'fn wayland_globals' src/probe.rs | head -1 | cut -d: -f1)
    if [ -z "$fn" ]; then
        echo "  === probe.rs: could not find wayland_globals ===" >&2
        exit 1
    fi
    end=$(awk -v from="$fn" 'NR>=from && /^}/{ print NR; exit }' src/probe.rs)
    if [ -z "$end" ]; then
        echo "  === probe.rs: could not find the end of wayland_globals ===" >&2
        exit 1
    fi

    head -n $((start - 1)) src/probe.rs > src/probe.rs.new
    cat >> src/probe.rs.new <<'RS'
// wlprobe.c IS NOT COMPILED ON FREBSD, so the symbol below would not link. The answer it
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
}
RS
    tail -n +$((end + 1)) src/probe.rs >> src/probe.rs.new
    mv src/probe.rs.new src/probe.rs
    echo "  probe.rs patched (lines $start..$end replaced)"
fi

echo "=== 4. x11shim.c: memfd_create -> shm_open(SHM_ANON) ==="
if grep -q SHM_ANON src/x11shim.c; then
    echo "  already patched"
else
    # fcntl.h for O_RDWR, then the three-line replacement.
    grep -q '#include <fcntl.h>' src/x11shim.c || \
      sed -i.bak 's|#include <sys/mman.h>|#include <sys/mman.h>\n#include <fcntl.h>|' src/x11shim.c

    # `awk -v` CANNOT CARRY A MULTILINE VALUE, and that is what failed on the first run:
    #
    #     awk: newline in string #if defined(__FreeBS... at source line 1
    #
    # So the replacement is written to a file and read with `getline` from BEGIN, which has
    # no such restriction, and the line is replaced by printing the file's contents.
    cat > /tmp/x11shim_patch.txt <<'PATCH'
#if defined(__FreeBSD__)
    // FREBSD HAS memfd_create(2) SINCE 13.0 BUT NO GLIBC-STYLE DECLARATION in sys/mman.h.
    // shm_open(SHM_ANON) is FreeBSD's own anonymous shared memory and is the same object
    // from the guest's point of view: an fd that ftruncate() sizes and mmap() maps.
    int fd = shm_open(SHM_ANON, O_RDWR, 0);
#else
    int fd = memfd_create("xshm", MFD_CLOEXEC);
#endif
PATCH
    awk -v patchfile=/tmp/x11shim_patch.txt '
      BEGIN {
        while ((getline line < patchfile) > 0) patch = patch line "\n"
        close(patchfile)
      }
      /int fd = memfd_create\("xshm", MFD_CLOEXEC\);/ { printf "%s", patch; next }
      { print }
    ' src/x11shim.c > src/x11shim.c.new
    mv src/x11shim.c.new src/x11shim.c
    rm -f src/x11shim.c.bak
    echo "  x11shim.c patched"
fi

echo
echo "=== done; build with: cargo build --release ==="
