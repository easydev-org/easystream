//! The system clipboard.
//!
//! IT LIVED IN `http.rs` AND THAT WAS AN ACCIDENT OF WHERE IT WAS FIRST NEEDED. Both functions shell
//! out to `xclip`/`xsel`/`wl-paste` - they have nothing to do with HTTP, and `est` carries clipboard
//! operations as messages (0x30-0x32). When the HTTP surface was removed the compiler pointed at the
//! only real dependency, and this module is the answer: the capability stays, the listener does not.

pub fn clipboard_get() -> Result<String, String> {
    // NOTE: an EMPTY clipboard makes wl-paste exit non-zero ("Nothing is
    // copied"); that is not an error. We accept any tool that RAN (we got a
    // process) — its stdout is the clipboard, empty included — and only report
    // an error when no tool exists at all (NotFound).
    let mut ran = false;
    for (cmd, args) in [
        // xclip first: it talks to the X server, which works even when the
        // compositor's seat has no keyboard (wl-clipboard refuses then).
        ("xclip", vec!["-selection", "clipboard", "-o"]),
        ("xsel", vec!["--clipboard", "--output"]),
        ("wl-paste", vec!["--no-newline"]),
    ] {
        match std::process::Command::new(cmd).args(&args).output() {
            Ok(out) => {
                ran = true;
                if out.status.success() {
                    return Ok(String::from_utf8_lossy(&out.stdout).to_string());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => ran = true,
        }
    }
    if ran {
        Ok(String::new()) // a clipboard tool exists; the clipboard is just empty
    } else {
        Err("clipboard get: no wl-paste/xclip/xsel available".into())
    }
}

pub fn clipboard_set(text: &str) -> Result<String, String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    for (cmd, args) in [
        // xclip first (X11 works without a keyboard-capable seat). Both xclip
        // and wl-copy STAY RESIDENT to serve the selection — they do not exit,
        // so we must NOT wait() on them or we hang forever. We detach (setsid)
        // and let the child keep the clipboard alive.
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
        ("wl-copy", vec!["--type", "text/plain;charset=utf-8"]),
    ] {
        let mut c = std::process::Command::new(cmd);
        c.args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        unsafe { c.pre_exec(|| { libc::setsid(); Ok(()) }); }
        if let Ok(mut child) = c.spawn() {
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(text.as_bytes());
                // closing stdin signals EOF to the tool
            }
            std::thread::spawn(move || { let _ = child.wait(); });
            return Ok("ok".into());
        }
    }
    Err("clipboard set: no wl-copy/xclip/xsel available".into())
}
