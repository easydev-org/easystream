fn main() {
    // The Windows backend uses the `windows` crate (no C shims); the Linux
    // backends need the x264/xcb/wayland shims. Compile those only on non-Windows
    // so the same crate cross-compiles to x86_64-pc-windows-gnu from Linux.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        return;
    }

    // THE INCLUDE PATHS COME FROM pkg-config, NOT FROM A GUESS ABOUT WHERE HEADERS LIVE.
    //
    // Debian and Ubuntu install `wayland-client.h` directly into /usr/include, so `#include
    // <wayland-client.h>` resolves with no -I at all. openSUSE does not: its wayland-devel
    // package installs the header into /usr/include/wayland/ and its own .pc file says so -
    //
    //     includedir=${prefix}/include/wayland
    //     Cflags: -I${includedir}
    //
    // so the same source fails there with
    //
    //     src/wlshim.c:10:10: fatal error: wayland-client.h: No such file or directory
    //
    // That is a property of the distribution's packaging, not of the code, and asking the
    // .pc file is how a build learns it. The libraries behind these .pc files are already
    // hard dependencies of the link step below, so a missing one is a missing build
    // dependency either way - what changes is that the error now names the package instead of
    // a header.
    //
    // Every shim gets the union of these paths. They are a handful of -I flags and applying
    // a redundant one costs nothing, whereas tracking which shim needs which package is a
    // list that drifts the first time a shim includes something new.
    let mut include_dirs: Vec<String> = Vec::new();
    for pkg in [
        "wayland-client",
        "xkbcommon",
        "xcb",
        "x264",
        "opus",
        "libyuv",
        "dbus-1",
        "libpipewire-0.3",
    ] {
        // A MISSING .pc FILE IS NOT AN ERROR HERE, AND SILENCE ABOUT IT WOULD BE.
        //
        // Three states are distinct and only one of them is fine:
        //
        //   pkg-config is absent          nothing can be asked; say so once, loudly, so the
        //                                 next failure is read as "install pkg-config" and not
        //                                 as a broken build script
        //   pkg-config present, .pc absent the package is not installed; the link will fail
        //                                 later with "cannot find -lwayland-client", which
        //                                 names the missing thing adequately
        //   both present                  paths are collected
        //
        // Only the third adds -I flags, and on Debian nothing needs them - which is why this
        // must not be a hard failure: nine of the ten jobs here build fine with an empty set.
        let out = std::process::Command::new("pkg-config")
            .args(["--cflags-only-I", pkg])
            .output();
        match out {
            Ok(out) if out.status.success() => {
                for flag in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                    if let Some(dir) = flag.strip_prefix("-I") {
                        if !include_dirs.iter().any(|d| d == dir) {
                            include_dirs.push(dir.to_string());
                        }
                    }
                }
            }
            Ok(_) => {}  // not installed: the link step reports it by name
            Err(e) => {
                println!(
                    "cargo:warning=pkg-config could not be run ({e}); \
                     include paths for {pkg} are unknown and the C shims may not compile"
                );
            }
        }
    }

    // ONE BUILDER PER SHIM, CONFIGURED THE SAME WAY.
    //
    // `cc::Build::file` takes `&mut self` and returns `&mut Self`, so the chain
    // `cc::Build::new().file(..)` is a `&mut Build` pointing at a temporary - which cannot be
    // passed by value to a function. The closure therefore takes the FILE LIST and builds the
    // whole thing, which also keeps `include("proto")` in exactly one place: every shim that
    // includes a generated protocol header needs it, and one that does not is unaffected.
    let shim = |files: &[&str]| -> cc::Build {
        let mut b = cc::Build::new();
        for f in files {
            b.file(f);
        }
        for dir in &include_dirs {
            b.include(dir);
        }
        b.include("proto");
        b
    };
    // Re-run this script when any C/C++ shim or protocol source changes.
    // Without these, cargo only re-runs the build script when build.rs itself
    // changes, so edits to the .c shims were silently NOT recompiled (a stale
    // wayland shim kept its per-frame leak while the source looked fixed).
    for f in [
        "src/x264shim.cpp",
        "src/opusshim.cpp",
        "src/x11shim.c",
        "src/wlshim.c",
        "src/portalshim.c",
        "src/wlprobe.c",
        "src/wlinput.c",
        "proto/wlr-screencopy-protocol.c",
        "proto/wlr-virtual-pointer-unstable-v1-protocol.c",
        "proto/wlr-virtual-keyboard-unstable-v1-protocol.c",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
    // Compile the tiny x264 shim (avoids binding x264's huge, version-dependent
    // x264_param_t / x264_picture_t layouts from Rust).
    shim(&["src/x264shim.cpp"]).compile("na_x264");
    shim(&["src/opusshim.cpp"]).compile("na_opus");
    shim(&["src/x11shim.c"]).compile("na_x11");
    shim(&["src/wlshim.c", "proto/wlr-screencopy-protocol.c"]).compile("na_wl");
    shim(&[
        "src/wlinput.c",
        "proto/wlr-virtual-pointer-unstable-v1-protocol.c",
        "proto/wlr-virtual-keyboard-unstable-v1-protocol.c",
    ])
    .compile("na_wlin");
    // The portal ScreenCast + PipeWire capture backend. libpipewire and
    // libdbus are OPTIONAL: if their headers are missing we skip it and
    // easystream still builds (wlroots/X11 paths unaffected).
    let have_pw = std::process::Command::new("pkg-config")
        .args(["--exists", "libpipewire-0.3", "dbus-1"]).status()
        .map(|s| s.success()).unwrap_or(false);
    if have_pw {
        // The include paths for dbus and pipewire are already in `include_dirs` - they are
        // asked for in the loop at the top for exactly this reason.
        shim(&["src/portalshim.c"]).compile("na_portal");
        println!("cargo:rustc-cfg=have_portal");
        println!("cargo:rustc-link-lib=pipewire-0.3");
        println!("cargo:rustc-link-lib=dbus-1");
    }
    // Wayland capability probe (registry globals) — linked with wayland-client.
    //
    // MUST stay OUTSIDE the `if have_pw` block above: probe.rs calls
    // na_probe_wayland unconditionally, so nesting this inside the portal block
    // made the link fail with "undefined symbol: na_probe_wayland" whenever
    // libpipewire was absent. The probe only needs wayland-client (a hard dep).
    shim(&["src/wlprobe.c"]).compile("na_wlprobe");
    println!("cargo:rustc-link-lib=x264");
    println!("cargo:rustc-link-lib=opus");
    println!("cargo:rustc-link-lib=xcb");
    println!("cargo:rustc-link-lib=xcb-xtest");
    println!("cargo:rustc-link-lib=xcb-shm");
    println!("cargo:rustc-link-lib=xcb-xfixes");
    println!("cargo:rustc-link-lib=xcb-render");
    println!("cargo:rustc-link-lib=wayland-client");
    println!("cargo:rustc-link-lib=xkbcommon");
    println!("cargo:rustc-link-lib=yuv");
    println!("cargo:rustc-link-lib=stdc++");
}
