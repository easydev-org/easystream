//! The daemon's HTTP surface (A11Y_HTTP.md) — the ONE thing a viewer or the
//! worker connects to.
//!
//!   GET  /            the easystream web UI (index.html)
//!   GET  /app.js      the web UI client
//!   GET  /est/ws      a TRANSPARENT byte pipe (WebSocket <-> est TCP)
//!   GET  /a11y/*      the a11y tree/action/clipboard API
//!
//! Written on raw TcpStream (no HTTP/WS crates): the surface is tiny, and a
//! hand-rolled server avoids the tiny_http/tungstenite socket-ownership clash
//! while keeping the daemon dependency-light (libdbus for AT-SPI is the only
//! non-trivial dep).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;

const INDEX_HTML: &str = include_str!("../webui/index.html");
const APP_JS: &str = include_str!("../webui/app.js");

/// a11y is the shared platform backend, opened lazily on first /a11y request.
pub type A11y = Arc<Mutex<Option<crate::a11y::Backend>>>;

pub fn serve(port: u16, est_port: u16, a11y: A11y) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    println!("easystream http on 0.0.0.0:{port} (est pipe -> 127.0.0.1:{est_port})");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let a11y = Arc::clone(&a11y);
                thread::spawn(move || {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(e) = handle(stream, est_port, a11y) {
                            eprintln!("http: {e}");
                        }
                    }));
                });
            }
            Err(e) => eprintln!("http accept: {e}"),
        }
    }
    Ok(())
}

fn handle(stream: TcpStream, est_port: u16, a11y: A11y) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    // Request line.
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let _method = parts.next().unwrap_or("").to_string();
    let url = parts.next().unwrap_or("/").to_string();
    // Headers.
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let path = url.split('?').next().unwrap_or("/").to_string();
    let query = url.splitn(2, '?').nth(1).unwrap_or("").to_string();

    // Token: prefer the WebSocket subprotocol list ("est, <token>") — browsers
    // can set that without putting the token in the URL/logs. Fall back to
    // ?token= for plain HTTP (a11y) clients. Only enforced when auth is on.
    // View is the open default; only the control token upgrades to CONTROL.
    let token = token_from(&headers, &query);
    let mode = crate::auth::authorize(token.as_deref());
    // A11y mutations (action/click/type/key/...) are forbidden in View mode.
    if mode == crate::auth::Mode::View && a11y_is_mutation(&path, &query) {
        return respond_text(&mut reader, 403, "text/plain", "read-only");
    }

    match path.as_str() {
        "/" | "/index.html" => respond_text(&mut reader, 200, "text/html; charset=utf-8", INDEX_HTML),
        "/app.js" => respond_text(&mut reader, 200, "application/javascript; charset=utf-8", APP_JS),
        "/health" => respond_text(&mut reader, 200, "text/plain", "ok"),
        // Which path each capability resolved to, decided once at startup by
        // src/probe.rs. Makes "why is capture slow / which backend is in use"
        // answerable without reading logs.
        "/capabilities" => respond_text(
            &mut reader,
            200,
            "application/json",
            &crate::probe::capabilities().to_json(),
        ),
        "/est/ws" => ws_pipe(reader, headers, est_port, mode, query.contains("audio=1")),
        p if p.starts_with("/a11y/") => a11y_route(&mut reader, p, &query, a11y),
        _ => respond_text(&mut reader, 404, "text/plain", "not found"),
    }
}

fn respond_text(reader: &mut BufReader<TcpStream>, status: u16, ctype: &str, body: &str) -> Result<(), String> {
    let mut s = reader.get_ref();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())
}

fn respond_bytes(reader: &mut BufReader<TcpStream>, status: u16, ctype: &str, body: &[u8]) -> Result<(), String> {
    let mut s = reader.get_ref();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(body).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())
}

/// screenshot_png captures the screen and encodes it as a PNG. A fresh
/// capture handle is opened per call: screenshots are rare one-shots and this
/// avoids holding a capture session (and its X shm segment) open forever.
fn screenshot_png(region: &str) -> Result<Vec<u8>, String> {
    let cap = crate::est::Capture::open()?;
    // Capture::size() probes a real frame on Wayland, so it is authoritative
    // here; the grabbed buffer must match it exactly.
    let (mut w, mut h);
    let px = match cap.grab_ptr() {
        Some((p, gw, gh)) => {
            w = gw;
            h = gh;
            // SAFETY: the pointer is valid until the next grab on this handle,
            // and we copy immediately.
            unsafe { std::slice::from_raw_parts(p, (gw * gh * 4) as usize) }.to_vec()
        }
        None => {
            let (sw, sh) = cap.size();
            w = sw;
            h = sh;
            cap.grab()?
        }
    };
    // Last line of defence: never index past the buffer (this was the panic).
    if w == 0 || (w as usize * 4) == 0 || px.len() % (w as usize * 4) != 0 {
        return Err(format!("capture size mismatch: {}x{} vs {} bytes", w, h, px.len()));
    }
    h = (px.len() / (w as usize * 4)) as u32;
    let png = crate::png::encode_bgra(w, h, &px);
    if region.is_empty() {
        return Ok(png);
    }
    // A region was requested: decode nothing, just crop by re-encoding a
    // sub-rectangle. (Kept simple: re-encode the cropped BGRA.)
    let nums: Vec<i64> = region.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    if nums.len() != 4 {
        return Err("region must be X,Y,W,H".into());
    }
    let (rx, ry, rw, rh) = (nums[0].max(0) as u32, nums[1].max(0) as u32, nums[2].max(1) as u32, nums[3].max(1) as u32);
    let (rx, ry) = (rx.min(w.saturating_sub(1)), ry.min(h.saturating_sub(1)));
    let (rw, rh) = (rw.min(w - rx), rh.min(h - ry));
    let mut crop = Vec::with_capacity((rw * rh * 4) as usize);
    for y in ry..ry + rh {
        let off = ((y * w + rx) * 4) as usize;
        crop.extend_from_slice(&px[off..off + (rw * 4) as usize]);
    }
    Ok(crate::png::encode_bgra(rw, rh, &crop))
}

fn q(query: &str, key: &str) -> String {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return percent_encoding::percent_decode_str(it.next().unwrap_or(""))
                .decode_utf8_lossy()
                .to_string();
        }
    }
    String::new()
}

fn a11y_route(reader: &mut BufReader<TcpStream>, path: &str, query: &str, a11y: A11y) -> Result<(), String> {
    // A poisoned lock (a panic while a previous request held it) must NOT kill
    // every later request: recover the inner value and carry on.
    let mut guard = a11y.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(crate::a11y::Backend::open()?);
    }
    let be = guard.as_ref().unwrap();
    let pid: u32 = q(query, "pid").parse().unwrap_or(0);
    // Screenshot returns PNG bytes, not text: handle it before the text table.
    if path == "/a11y/screenshot" {
        return match screenshot_png(&q(query, "region")) {
            Ok(png) => respond_bytes(reader, 200, "image/png", &png),
            Err(e) => respond_text(reader, 500, "text/plain; charset=utf-8", &e),
        };
    }
    let result: Result<String, String> = match path {
        "/a11y/apps" => be.apps(),
        "/a11y/tree" => be.tree(pid, &q(query, "app"), q(query, "depth").parse().unwrap_or(0)),
        "/a11y/find" => {
            let o = match q(query, "o").as_str() {
                "bounds" => 1,
                "center" => 2,
                "value" => 3,
                _ => 0,
            };
            be.find(pid, &q(query, "app"), &q(query, "sel"), o)
        }
        "/a11y/action" => {
            let v = q(query, "value");
            be.action(pid, &q(query, "app"), &q(query, "sel"), &q(query, "action"), if v.is_empty() { None } else { Some(&v) })
        }
        // Clipboard: `?set=TEXT` writes, otherwise reads. The daemon's est
        // connection already owns a clipboard implementation (the real Win32
        // in-process), so share it.
        "/a11y/clipboard" => {
            let set = q(query, "set");
            if set.is_empty() { clipboard_get() } else { clipboard_set(&set) }
        }
        "/a11y/click" => input_click(query),
        "/a11y/type" => input_text(query),
        "/a11y/key" => input_key(query),
        "/a11y/scroll" => input_scroll(query),
        "/a11y/drag" => input_drag(query),
        _ => Err(format!("unknown a11y path {path}")),
    };
    match result {
        Ok(body) => respond_text(reader, 200, "text/plain; charset=utf-8", &body),
        Err(e) => respond_text(reader, 500, "text/plain; charset=utf-8", &e),
    }
}

// ── input (HTTP) ────────────────────────────────────────────────────────────

fn num(query: &str, key: &str) -> i32 {
    q(query, key).parse().unwrap_or(0)
}

fn with_input(f: impl FnOnce(&crate::est::Input)) -> Result<String, String> {
    match crate::est::shared_input() {
        Some(m) => {
            let g = m.lock().unwrap();
            f(&g);
            Ok("ok".into())
        }
        None => Err("no input backend".into()),
    }
}

/// clipboard_get reads the system clipboard.
///
/// The system clipboard is read by shelling out: xclip/xsel on X11, wl-paste on Wayland (the
/// wlroots sandbox has no X clipboard tool). Best-effort: a missing tool is a
/// clear error, not a panic.

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

/// clipboard_set writes the system clipboard.

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

fn input_click(query: &str) -> Result<String, String> {
    let (x, y) = (num(query, "x") as i16, num(query, "y") as i16);
    let btn = match q(query, "button").as_str() {
        "right" => 3u8,
        "middle" => 2u8,
        _ => 1u8,
    };
    let count = num(query, "count").max(1);
    with_input(|i| {
        i.move_to(x, y);
        for _ in 0..count {
            i.button(btn, true);
            i.button(btn, false);
        }
    })
}

fn input_text(query: &str) -> Result<String, String> {
    let text = q(query, "text");
    with_input(|i| {
        for ch in text.chars() {
            if let Some((kc, need_shift)) = char_keycode(ch) {
                if need_shift {
                    i.key(50, true);
                }
                i.key(kc as u16, true);
                i.key(kc as u16, false);
                if need_shift {
                    i.key(50, false);
                }
            }
        }
    })
}

fn input_key(query: &str) -> Result<String, String> {
    let name = q(query, "name");
    with_input(|i| {
        if let Some(kc) = key_by_name(&name) {
            i.key(kc as u16, true);
            i.key(kc as u16, false);
        }
    })
}

// The Linux key map (X11 keycodes).
fn char_keycode(ch: char) -> Option<(u8, bool)> { crate::x11::char_keycode(ch) }
fn key_by_name(name: &str) -> Option<u8> { crate::x11::key_by_name(name) }

fn input_scroll(query: &str) -> Result<String, String> {
    let (x, y) = (num(query, "x") as i16, num(query, "y") as i16);
    let (dx, dy) = (num(query, "dx"), num(query, "dy"));
    with_input(|i| {
        i.move_to(x, y);
        i.scroll(dx, dy);
    })
}

fn input_drag(query: &str) -> Result<String, String> {
    let (x1, y1) = (num(query, "x1") as i16, num(query, "y1") as i16);
    let (x2, y2) = (num(query, "x2") as i16, num(query, "y2") as i16);
    let ms = num(query, "ms").max(1) as u64;
    with_input(|i| {
        i.move_to(x1, y1);
        i.button(1, true);
        let steps = 8u64;
        for s in 1..=steps {
            let nx = x1 as i32 + (x2 as i32 - x1 as i32) * s as i32 / steps as i32;
            let ny = y1 as i32 + (y2 as i32 - y1 as i32) * s as i32 / steps as i32;
            i.move_to(nx as i16, ny as i16);
            std::thread::sleep(std::time::Duration::from_millis(ms / steps));
        }
        i.button(1, false);
    })
}

// ── WebSocket (RFC 6455, server side, binary frames only) ───────────────────

/// ws_pipe upgrades the request and relays binary frames to/from a fresh est TCP
/// connection — a transparent byte pipe (the daemon does not parse the frames).
fn ws_pipe(mut reader: BufReader<TcpStream>, headers: Vec<(String, String)>, est_port: u16, mode: crate::auth::Mode, want_audio: bool) -> Result<(), String> {
    let key = headers
        .iter()
        .find(|(k, _)| k == "sec-websocket-key")
        .map(|(_, v)| v.clone())
        .ok_or("no Sec-WebSocket-Key")?;
    // Accept = base64(sha1(key + GUID)).
    let accept = {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(key.as_bytes());
        h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        base64::engine::general_purpose::STANDARD.encode(h.finalize())
    };
    let mut sock = reader.get_ref().try_clone().map_err(|e| e.to_string())?;
    // Browsers REQUIRE the server to echo a subprotocol when the client offered
    // one, or they fail the connection. We echo "est" (never the token).
    let proto = headers
        .iter()
        .find(|(k, _)| k == "sec-websocket-protocol")
        .and_then(|(_, v)| v.split(',').map(|s| s.trim()).find(|p| !p.is_empty()))
        .unwrap_or("est");
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: {proto}\r\n\r\n"
    );
    sock.write_all(resp.as_bytes()).map_err(|e| e.to_string())?;

    let tcp = TcpStream::connect(("127.0.0.1", est_port)).map_err(|e| e.to_string())?;
    // Propagate the HTTP-layer mode to the est connection with an Auth frame,
    // so est itself also enforces View (defence in depth: even if a mutation
    // frame slipped past the WS filter, est would drop it).
    {
        let tok = match mode {
            crate::auth::Mode::Control => std::env::var("EAZYSTREAM_CONTROL_TOKEN").unwrap_or_default(),
            crate::auth::Mode::View => std::env::var("EAZYSTREAM_VIEW_TOKEN").unwrap_or_default(),
        };
        let mut w = tcp.try_clone().map_err(|e| e.to_string())?;
        if !crate::auth::is_open() {
            let mut hdr = [0u8; 6];
            hdr[0] = 0x03; // T_AUTH
            hdr[2..6].copy_from_slice(&(tok.len() as u32).to_be_bytes());
            let _ = w.write_all(&hdr);
            let _ = w.write_all(tok.as_bytes());
            let _ = w.flush();
        }
    }
    // Ask est for audio if the browser wants it (?audio=1).
    if want_audio {
        let mut w = tcp.try_clone().map_err(|e| e.to_string())?;
        let mut hdr = [0u8; 6];
        hdr[0] = 0x06; // T_AUDIO_REQ
        let _ = w.write_all(&hdr);
        let _ = w.flush();
    }
    let tcp = Arc::new(tcp);
    let tcp_r = Arc::clone(&tcp);
    let mut ws_w = sock.try_clone().map_err(|e| e.to_string())?;

    // est -> browser
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 1 << 16];
        let mut conn = &*tcp_r;
        loop {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_write_binary(&mut ws_w, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tcp_r.shutdown(std::net::Shutdown::Both);
    });

    // browser -> est. In View mode, drop every mutation frame (input, a11y
    // action, clipboard write) BEFORE it reaches the est daemon.
    let mut tcp_w = &*tcp;
    loop {
        match ws_read(&mut reader) {
            Ok(Some(frame)) => {
                if mode == crate::auth::Mode::View && frame.len() >= 1
                    && crate::auth::is_mutation(frame[0])
                {
                    continue; // silently ignored
                }
                if tcp_w.write_all(&frame).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    let _ = tcp.shutdown(std::net::Shutdown::Both);
    let _ = reader_thread.join();
    Ok(())
}

/// ws_write_binary writes one unmasked, unfragmented binary frame.
fn ws_write_binary(w: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let mut hdr = vec![0x82u8];
    let n = payload.len();
    if n < 126 {
        hdr.push(n as u8);
    } else if n <= u16::MAX as usize {
        hdr.push(126);
        hdr.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        hdr.push(127);
        hdr.extend_from_slice(&(n as u64).to_be_bytes());
    }
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    w.flush()
}

/// ws_read reads one client frame (masked). Returns None for control frames.
fn ws_read(r: &mut BufReader<TcpStream>) -> Result<Option<Vec<u8>>, String> {
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b2).map_err(|e| e.to_string())?;
    let opcode = b2[0] & 0x0f;
    let masked = b2[1] & 0x80 != 0;
    let mut len = (b2[1] & 0x7f) as usize;
    if len == 126 {
        let mut e = [0u8; 2];
        r.read_exact(&mut e).map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(e) as usize;
    } else if len == 127 {
        let mut e = [0u8; 8];
        r.read_exact(&mut e).map_err(|e| e.to_string())?;
        len = u64::from_be_bytes(e) as usize;
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).map_err(|e| e.to_string())?;
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).map_err(|e| e.to_string())?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    match opcode {
        0x2 => Ok(Some(payload)),       // binary
        0x8 => Err("close".into()),     // close
        0x9 | 0xa => Ok(None),          // ping/pong: ignore
        _ => Ok(None),                  // text/continuation: ignore (est is binary)
    }
}

/// token_from extracts the access token: the 2nd item of the Sec-WebSocket-
/// Protocol header ("est, <token>"), else ?token= in the query string.
fn token_from(headers: &[(String, String)], query: &str) -> Option<String> {
    if let Some((_, v)) = headers.iter().find(|(k, _)| k == "sec-websocket-protocol") {
        for part in v.split(',') {
            let p = part.trim();
            if !p.is_empty() && p != "est" {
                return Some(p.to_string());
            }
        }
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("token=") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// a11y_is_mutation reports whether an /a11y/* request CHANGES the guest.
/// `/a11y/clipboard` is a READ unless it carries `?set=` (a clipboard write).
fn a11y_is_mutation(path: &str, query: &str) -> bool {
    match path {
        "/a11y/action" | "/a11y/click" | "/a11y/type" | "/a11y/key"
        | "/a11y/scroll" | "/a11y/drag" => true,
        "/a11y/clipboard" => query.split('&').any(|kv| kv.starts_with("set=")),
        _ => false,
    }
}
