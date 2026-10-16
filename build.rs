fn main() {
    // The Windows backend uses the `windows` crate (no C shims); the Linux
    // backends need the x264/xcb/wayland shims. Compile those only on non-Windows
    // so the same crate cross-compiles to x86_64-pc-windows-gnu from Linux.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        return;
    }
    // Re-run this script when any C/C++ shim or protocol source changes.
    // Without these, cargo only re-runs the build script when build.rs itself
    // changes, so edits to the .c shims were silently NOT recompiled (a stale
    // wayland shim kept its per-frame leak while the source looked fixed).
    for f in [
        "src/x264shim.cpp",
        "src/opusshim.cpp",
        "src/x11shim.c",
        "src/wlshim.c",
        "src/wlinput.c",
        "proto/wlr-screencopy-protocol.c",
        "proto/wlr-virtual-pointer-unstable-v1-protocol.c",
        "proto/wlr-virtual-keyboard-unstable-v1-protocol.c",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
    // Compile the tiny x264 shim (avoids binding x264's huge, version-dependent
    // x264_param_t / x264_picture_t layouts from Rust).
    cc::Build::new().file("src/x264shim.cpp").compile("na_x264");
    cc::Build::new().file("src/opusshim.cpp").compile("na_opus");
    cc::Build::new().file("src/x11shim.c").compile("na_x11");
    cc::Build::new()
        .file("src/wlshim.c")
        .include("proto")
        .file("proto/wlr-screencopy-protocol.c")
        .include("proto")
        .compile("na_wl");
    cc::Build::new()
        .file("src/wlinput.c")
        .include("proto")
        .file("proto/wlr-virtual-pointer-unstable-v1-protocol.c")
        .file("proto/wlr-virtual-keyboard-unstable-v1-protocol.c")
        .include("proto")
        .compile("na_wlin");
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
