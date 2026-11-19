//! easystream (Rust) — the Linux screen daemon (see EST.md + A11Y_HTTP.md).
//!
//! ONE TCP connection multiplexes the H.264 video stream (0x02), input
//! (0x10-0x14), clipboard (0x30-0x32) and a11y queries (0x20-0x27). Capture,
//! encode and input injection all live in this one process, so there is no
//! cross-backend conflict. Rewritten in Rust so the ownership rules catch the
//! lifetime bugs that crashed the Zig version.
mod a11y;
mod auth;
mod audio;
mod clipboard;
mod opus;
mod atspi;
mod est;
mod png;

// The Linux backends (X11 / Wayland) use C shims + libx264.
mod wayland;
mod portal;
mod probe;
mod x11;
mod x264;
mod vcfg;


use std::env;

fn main() {
    // Log panics (the guest runs us with stderr redirected to a file).
    std::panic::set_hook(Box::new(|info| {
        eprintln!("easystream panic: {info}");
    }));
    // Kick off the audio backend probe in the background so Hello can report
    // audio availability without paying the (up to ~1.5s) probe inline.
    audio::ensure_probed();
    let port: u16 = env::var("EAZYSTREAM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(48079);
    let args: Vec<String> = env::args().collect();
    // Diagnostic one-shots (mirror the C# daemon's `wgctest`).
    if args.len() >= 2 && args[1] == "serve" || args.len() < 2 {
        // ONE LISTENER, ONE PROTOCOL.
        //
        // THIS USED TO ALSO START AN HTTP SERVER on 48078 that served the web UI, the `/est/ws`
        // byte pipe and the `/a11y/*` API. All of that is gone, and the reason is a division of
        // labour rather than a feature cut: `est://` already carries every one of those operations
        // as a message type (video 0x02, input 0x10-0x14, a11y 0x20-0x27, clipboard 0x30-0x32,
        // screenshot 0x40-0x41), so the HTTP surface was a SECOND way to say the same things - and
        // a second UI to keep in step with them.
        //
        // The browser belongs to whoever owns the machine, and here that is easyvm. This daemon's
        // job is to capture a screen and answer on one socket.
        if let Err(e) = est::serve(port) {
            eprintln!("easystream: {e}");
            std::process::exit(1);
        }
        return;
    }
    // One-shot CLI (debugging) is a follow-up; the daemon is the contract.
    eprintln!("usage: easystream [serve]");
    std::process::exit(2);
}
