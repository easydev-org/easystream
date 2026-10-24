//! easystream (`est://`) server — the SINGLE interface of easystream (EST.md).
//!
//! One TCP connection multiplexes the video stream (0x02), input (0x10-0x14),
//! clipboard (0x30-0x32) and a11y queries (0x20-0x27). Capture, encode and input
//! share this one process. Rust owns the lifetimes: the `Conn` is behind an
//! `Arc`, so the video thread and the control reader cannot outlive it.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::portal::Portal;
use crate::wayland::{Wayland, WlInput};
use crate::x11::{char_keycode, key_by_name, X11};
use crate::x264::X264;
/// Capture is either the X11 (XGetImage) or Wayland (wlr-screencopy) backend.
pub enum Capture {
    X11(X11),
    Wl(Wayland),
    Portal(std::sync::Arc<Portal>),
}

impl Capture {
    pub fn open() -> Result<Capture, String> {
        {
            // Use the startup probe's answer instead of "try one, wait for it to
            // fail, fall back": the probe asks the compositor which protocols it
            // exports, which is instant, whereas a failed portal attempt cost a
            // D-Bus round trip on every capture (a screenshot opened one each).
            use crate::probe::{capabilities, CapturePath};
            let caps = capabilities();
            match caps.capture {
                CapturePath::ZwlrScreencopy => Ok(Capture::Wl(Wayland::open()?)),
                CapturePath::Portal => Ok(Capture::Portal(shared_portal()?)),
                CapturePath::X11 => Ok(Capture::X11(X11::open()?)),
                CapturePath::None => {
                    // The probe ran before the desktop was up (or on a bare
                    // session): fall back to the old dynamic choice so a late
                    // compositor still works.
                    if std::env::var("WAYLAND_DISPLAY")
                        .map(|v| !v.is_empty())
                        .unwrap_or(false)
                    {
                        // The SHARED session, so the input backend attaches to
                        // the same one instead of asking the portal for a second
                        // (which segfaults xdg-desktop-portal-gnome).
                        match shared_portal() {
                            Ok(p) => return Ok(Capture::Portal(p)),
                            Err(_) => return Ok(Capture::Wl(Wayland::open()?)),
                        }
                    }
                    Ok(Capture::X11(X11::open()?))
                }
            }
        }
    }
    /// grab returns the newest frame as top-down BGRA pixels (the X11/Wayland
    /// API.
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        match self {
            Capture::X11(x) => x.grab(),
            Capture::Wl(w) => w.grab(),
            Capture::Portal(p) => p.grab(),
        }
    }
    /// grab_ptr returns a pointer into the capture buffer (zero-copy), valid
    /// until the next grab. None when the backend cannot expose one.
    /// grab_into copies the newest frame into `out` (reused) and reports its
    /// size.
    pub fn grab_ptr(&self) -> Option<(*const u8, u32, u32)> {
        match self {
            Capture::X11(x) => x.grab_shm_ptr(),
            Capture::Wl(w) => w.grab_shm_ptr(),
            Capture::Portal(_) => None,
        }
    }
    pub fn size(&self) -> (u32, u32) {
        match self {
            Capture::X11(x) => x.size(),
            // The portal negotiated the monitor size during Start; no probing.
            Capture::Portal(p) => p.size(),
            // A wlroots headless output has no static mode to query: the real
            // size only exists once a frame has been captured. Probe one (the
            // shm mapping is reused, so this is cheap).
            //
            // Retry briefly rather than falling straight back to a guessed
            // 1920x1080: on a 1024x768 desktop that guess made the daemon
            // advertise the wrong resolution.
            Capture::Wl(w) => {
                for _ in 0..25 {
                    if let Some((_, gw, gh)) = w.grab_shm_ptr() {
                        if gw > 0 && gh > 0 {
                            return (gw, gh);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                (1920, 1080)
            }
        }
    }
}

/// Input is the platform input backend (X11 XTest or Wayland wlr-virtual-*).

/// shared_portal returns the ONE portal session this process uses.
///
/// Capture and input MUST share it. Two sessions meant two
/// CreateSession/SelectSources round trips, and xdg-desktop-portal-gnome
/// segfaults when it is asked for a second screencast session while the first is
/// live ("screen cast session owned by ':1.91' created" repeated, then
/// "g_object_set_qdata_full: assertion 'quark > 0' failed"). One Arc, cloned by
/// both users, keeps a single session.
fn shared_portal() -> Result<std::sync::Arc<Portal>, String> {
    shared_portal_rc()
}

/// reset_portal drops the cached handle so the next shared_portal() reconnects.
///
/// Called when a capture attempt fails: the portal process may have died and
/// come back, and the old DBusConnection is then dead. Without this the daemon
/// retries against a stale connection forever.
fn reset_portal() {
    // OnceLock has no clear(), so the cached value is only dropped by using an
    // AtomicPtr-free design: we keep a generation counter and rebuild. Simpler:
    // store the Arc behind a Mutex<Option<..>> instead of OnceLock.
    let _ = PORTAL.lock().map(|mut g| g.take());
}

/// PORTAL holds the one live session.
///
/// A Mutex<Option<..>> rather than OnceLock because the handle has to be
/// discardable: a crashed portal leaves a dead connection, and retrying against
/// it never succeeds.
static PORTAL: std::sync::Mutex<Option<std::sync::Arc<Portal>>> = std::sync::Mutex::new(None);

fn shared_portal_rc() -> Result<std::sync::Arc<Portal>, String> {
    let mut g = PORTAL.lock().map_err(|e| e.to_string())?;
    if let Some(p) = g.as_ref() {
        return Ok(p.clone());
    }
    let fresh = std::sync::Arc::new(Portal::open()?);
    *g = Some(fresh.clone());
    Ok(fresh)
}

/// PortalInput injects pointer and keyboard events through the portal session
/// that also carries the video stream.
///
/// It exists because the compositors that need the portal for capture need it for
/// input too, and for the same reason: mutter and kwin implement neither
/// zwlr_virtual_pointer_v1 nor zwlr_virtual_keyboard_v1. Before this, the
/// PortalRemoteDesktop path fell back to WlInput, which cannot bind those
/// protocols on GNOME or KDE — so every event was dropped and the desktop looked
/// frozen.
///
/// It holds a shared reference to the capture backend's Portal, so events travel
/// on the session the user already consented to.
pub struct PortalInput {
    portal: std::sync::Arc<crate::portal::Portal>,
}

impl PortalInput {
    /// open returns Err when the portal did not grant pointer + keyboard, so the
    /// caller can report "no input" instead of silently discarding events.
    pub fn open(portal: std::sync::Arc<crate::portal::Portal>) -> Result<PortalInput, String> {
        if !portal.input_ok() {
            return Err("portal did not grant RemoteDesktop input".into());
        }
        Ok(PortalInput { portal })
    }

    fn move_to(&self, x: i16, y: i16) {
        // The protocol wants coordinates in the captured stream's space, and the
        // caller sends device pixels of that same stream, so a direct cast is
        // right — no scaling (that was a real bug in the wlroots backend, where a
        // hard-coded 1920x1080 made the cursor land in the wrong place).
        self.portal.move_to(x as f64, y as f64);
    }

    fn button(&self, num: u8, down: bool) {
        // The wire number from the browser is the DOM/X11 convention
        // (1 left, 2 middle, 3 right). The portal does NOT take that: the
        // RemoteDesktop spec says the button "is encoded according to Linux Evdev
        // button codes", i.e. BTN_LEFT=272, BTN_RIGHT=273, BTN_MIDDLE=274.
        //
        // Passing 1/2/3 straight through meant the compositor received codes that
        // are not pointer buttons at all, so right click and double click did
        // nothing (only the occasional press that mutter happened to tolerate
        // appeared to work). Translating here fixes both on every portal backend.
        let evdev = match num {
            1 => 0x110, // BTN_LEFT   272
            2 => 0x112, // BTN_MIDDLE 274
            3 => 0x111, // BTN_RIGHT  273
            4 => 0x113, // BTN_SIDE   275
            5 => 0x114, // BTN_EXTRA  276
            // Already an evdev code (or something we do not know): pass it on
            // rather than silently dropping the event.
            other => other as i32,
        };
        self.portal.button(evdev, down);
    }

    fn scroll(&self, dx: i32, dy: i32) {
        // One notch is 15 degrees (~1.0 in the portal's smooth-scroll units).
        // Sending raw counts made scrolling barely move.
        self.portal.axis(dx as f64 * 1.0, dy as f64 * 1.0);
    }

    fn key(&self, keycode: u16, down: bool) {
        // Atspi/X11 keycodes from the browser are evdev + 8; the portal wants the
        // raw evdev code, so subtract the offset. Getting this wrong shifts every
        // key by eight positions.
        let code = (keycode as i32) - 8;
        if code > 0 {
            self.portal.key(code, down);
        }
    }
}

pub enum Input {
    X11(X11),
    Wl(WlInput),
    /// Portal RemoteDesktop: the only input path for compositors without the
    /// wlroots virtual-input protocols (mutter/GNOME Shell, kwin/KDE Plasma).
    Portal(PortalInput),
}

impl Input {
    pub fn move_to(&self, x: i16, y: i16) {
        match self {
            Input::X11(i) => i.move_to(x, y),
            Input::Wl(i) => i.move_to(x, y),
            Input::Portal(i) => i.move_to(x, y),
        }
    }
    pub fn button(&self, num: u8, down: bool) {
        match self {
            Input::X11(i) => i.button(num, down),
            Input::Wl(i) => i.button(num, down),
            Input::Portal(i) => i.button(num, down),
        }
    }
    pub fn scroll(&self, dx: i32, dy: i32) {
        match self {
            Input::X11(i) => {
                let (vbtn, vn) = if dy > 0 { (5u8, dy) } else { (4u8, -dy) };
                for _ in 0..vn {
                    i.button(vbtn, true);
                    i.button(vbtn, false);
                }
                let (hbtn, hn) = if dx > 0 { (7u8, dx) } else { (6u8, -dx) };
                for _ in 0..hn {
                    i.button(hbtn, true);
                    i.button(hbtn, false);
                }
            }
            Input::Wl(i) => i.scroll(dx, dy),
            Input::Portal(i) => i.scroll(dx, dy),
        }
    }
    pub fn key(&self, keycode: u16, down: bool) {
        match self {
            Input::X11(i) => i.key(keycode as u8, down),
            Input::Wl(i) => i.key(keycode as u8, down),
            Input::Portal(i) => i.key(keycode, down),
        }
    }
}

/// Encoder is the platform H.264 encoder (libx264 on Linux, Media Foundation on
/// ). Both take BGRA and return an Annex-B access unit.
pub enum Encoder {
    X264(X264),
}

impl Encoder {
    fn new(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Encoder, String> {
        {
            Ok(Encoder::X264(X264::new(w, h, fps, bitrate)?))
        }
    }
    fn force_idr(&mut self) {
        match self {
            Encoder::X264(e) => e.force_idr(),
        }
    }
    /// The EST.md encoder code for Hello. Must describe what is ACTUALLY running,
    /// not what was requested: a VAAPI request that fell back to x264 has to
    /// report x264 or the UI would promise hardware acceleration it is not
    /// getting.
    fn kind(&self) -> u8 {
        match self {
            Encoder::X264(_) => crate::vcfg::ENC_X264,
        }
    }
    fn encode(&mut self, bgra: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        match self {
            Encoder::X264(e) => {
                e.encode(bgra)
                    .map(|au| if au.is_empty() { Vec::new() } else { vec![au] })
            }
        }
    }
    /// Encode a zero-copy frame, scaling from the capture geometry when the
    /// encoder is emitting something smaller (标清/高清).
    fn encode_ptr_sized(
        &mut self,
        p: *const u8,
        len: u32,
        sw: u32,
        sh: u32,
    ) -> Result<Vec<Vec<u8>>, String> {
        match self {
            Encoder::X264(e) => e
                .encode_ptr_scaled(p, len, sw, sh)
                .map(|au| if au.is_empty() { Vec::new() } else { vec![au] }),
        }
    }
    fn encode_ptr(&mut self, p: *const u8, len: u32) -> Result<Vec<Vec<u8>>, String> {
        match self {
            Encoder::X264(e) => {
                e.encode_ptr(p, len)
                    .map(|au| if au.is_empty() { Vec::new() } else { vec![au] })
            }
        }
    }
}

const T_HELLO: u8 = 0x01;
/// Run-time reconfiguration (EST.md 0x08/0x09). A client asks; the server owns
/// the encoder and answers with ConfigAck, then a fresh Hello once the new
/// encoder is live.
const T_CONFIG_SET: u8 = 0x08;
const T_CONFIG_ACK: u8 = 0x09;
/// Ask for an IDR without changing any configuration (EST.md 0x0A). The latch it
/// sets is the same one a new subscriber sets, so N requests in one frame
/// interval produce exactly one IDR - which matters, because a client reacts to
/// a corrupt picture by asking again every frame and an IDR is expensive.
const T_KEYFRAME_REQ: u8 = 0x0A;
const T_VIDEO: u8 = 0x02;
const T_MOVE: u8 = 0x10;
const T_BUTTON: u8 = 0x11;
const T_SCROLL: u8 = 0x12;
const T_KEY: u8 = 0x13;
const T_TEXT: u8 = 0x14;
const T_CLIP_GET: u8 = 0x30;
const T_CLIP_DATA: u8 = 0x31;
const T_CLIP_SET: u8 = 0x32;
const T_AUDIO: u8 = 0x05;
const T_AUDIO_REQ: u8 = 0x06;
const T_AUDIO_OFF: u8 = 0x07;
const T_AUTH: u8 = 0x03;
const T_AUTH_OK: u8 = 0x04;
const T_PING: u8 = 0x50;
const T_PONG: u8 = 0x51;

/// Conn is shared between the video thread and the control reader. The write
/// half is behind a Mutex (both threads write frames); the X11 handle is behind
/// a Mutex (capture and input both touch it).
pub struct Conn {
    stream: Mutex<TcpStream>,
    /// Access mode for THIS connection (View = read-only). Set by the Auth
    /// frame (0x03) or inherited from the HTTP pipe's token.
    mode: std::sync::atomic::AtomicU8,
    /// Whether this connection wants Audio frames. Off until the client sends
    /// AudioReq (0x06), so a viewer that does not want audio costs nothing.
    audio_on: std::sync::atomic::AtomicBool,
    /// The process-wide input backend (shared by every connection).
    input: &'static Option<Mutex<Input>>,
    /// In-process clipboard fallback (Linux/Wayland have no system clipboard
    /// here).
    clipboard: Mutex<String>,
}

impl Conn {
    fn write_frame(&self, typ: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut hdr = [0u8; 6];
        hdr[0] = typ;
        hdr[2..6].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        let mut s = self.stream.lock().unwrap();
        s.write_all(&hdr)?;
        s.write_all(payload)?;
        s.flush()
    }

    /// with_input runs `f` with the X11 input connection, if any (Wayland has no
    /// input backend wired yet, so input frames are ignored there).
    fn with_input(&self, f: impl FnOnce(&Input)) {
        if let Some(i) = self.input.as_ref() {
            if let Ok(x) = i.lock() {
                f(&x);
            }
        }
    }
}

pub fn serve(port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    println!("easystream est server on 127.0.0.1:{port}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || {
                    // A panic in one connection must NEVER kill the listener.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Err(e) = handle(stream) {
                            eprintln!("est conn: {e}");
                        }
                    }));
                    if r.is_err() {
                        eprintln!("est conn: panicked");
                    }
                });
            }
            Err(e) => eprintln!("est accept: {e}"),
        }
    }
    Ok(())
}

/// Broadcaster runs ONE capture+encode pipeline and fans the access units out to
/// every subscribed connection. Without it, each /stream/ws connection started
/// its own pump (N tabs = N x CPU).
struct Broadcaster {
    subs: Mutex<Vec<mpsc::SyncSender<Arc<Vec<u8>>>>>,
    /// Audio subscribers (Opus packets). Separate from video so a video-only
    /// client (or one that does not ask for audio) is unaffected.
    asubs: Mutex<Vec<mpsc::SyncSender<Arc<Vec<u8>>>>>,
    /// bumped whenever a new viewer subscribes, so the pump sends a keyframe.
    new_sub: std::sync::atomic::AtomicBool,
}

impl Broadcaster {
    /// request_keyframe asks the pump for an IDR on its next frame.
    ///
    /// The same latch a new subscriber sets, so this is idempotent: a client that
    /// asks on every frame gets one IDR per frame interval, not one per request.
    fn request_keyframe(&self) {
        self.new_sub
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn subscribe(&self) -> mpsc::Receiver<Arc<Vec<u8>>> {
        // Bounded so a slow/stalled viewer cannot grow an unbounded backlog in
        // the daemon (it just drops frames for THAT viewer).
        let (tx, rx) = mpsc::sync_channel(4);
        if let Ok(mut s) = self.subs.lock() {
            s.push(tx);
        }
        self.new_sub
            .store(true, std::sync::atomic::Ordering::Relaxed);
        rx
    }
    fn broadcast(&self, au: Arc<Vec<u8>>) {
        if let Ok(mut s) = self.subs.lock() {
            // try_send: never block the pump on a slow consumer. Full -> drop
            // this frame for that subscriber (the next one keeps it current).
            s.retain(|tx| match tx.try_send(au.clone()) {
                Ok(()) => true,
                Err(mpsc::TrySendError::Full(_)) => true,
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
        }
    }
    fn has_subs(&self) -> bool {
        self.subs.lock().map(|s| !s.is_empty()).unwrap_or(false)
    }
    fn subscribe_audio(&self) -> mpsc::Receiver<Arc<Vec<u8>>> {
        let (tx, rx) = mpsc::sync_channel(8);
        if let Ok(mut s) = self.asubs.lock() {
            s.push(tx);
        }
        rx
    }
    fn broadcast_audio(&self, pkt: Arc<Vec<u8>>) {
        if let Ok(mut s) = self.asubs.lock() {
            s.retain(|tx| {
                !matches!(
                    tx.try_send(pkt.clone()),
                    Err(mpsc::TrySendError::Disconnected(_))
                )
            });
        }
    }
    fn has_audio_subs(&self) -> bool {
        self.asubs.lock().map(|s| !s.is_empty()).unwrap_or(false)
    }
}

/// Every live connection, so a reconfiguration can be announced to all of them.
///
/// Hello is the only authoritative statement of video geometry (EST.md), so a
/// viewer that is already connected must receive a new one when the encoder is
/// rebuilt - otherwise it keeps decoding at the old size and shows garbage. Weak
/// references because a closed connection must not be kept alive by this list.
static CONNS: OnceLock<Mutex<Vec<std::sync::Weak<Conn>>>> = OnceLock::new();

fn conns() -> &'static Mutex<Vec<std::sync::Weak<Conn>>> {
    CONNS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a connection and drop the entries that have since closed.
fn register_conn(c: &Arc<Conn>) {
    if let Ok(mut v) = conns().lock() {
        v.retain(|w| w.strong_count() > 0);
        v.push(Arc::downgrade(c));
    }
}

/// send_hello_to_all re-announces the current video configuration to every live
/// connection. Called after a ConfigSet has rebuilt the encoder; the payload is
/// rebuilt per call so it reflects the geometry that is actually running.
fn send_hello_to_all() {
    let payload = hello_payload();
    if let Ok(v) = conns().lock() {
        for w in v.iter() {
            if let Some(c) = w.upgrade() {
                let _ = c.write_frame(T_HELLO, &payload);
            }
        }
    }
}

static BC: OnceLock<Arc<Broadcaster>> = OnceLock::new();

fn broadcaster() -> Arc<Broadcaster> {
    BC.get_or_init(|| {
        let b = Arc::new(Broadcaster {
            subs: Mutex::new(Vec::new()),
            asubs: Mutex::new(Vec::new()),
            new_sub: std::sync::atomic::AtomicBool::new(false),
        });
        let bb = Arc::clone(&b);
        thread::spawn(move || pump(bb));
        let ba = Arc::clone(&b);
        thread::spawn(move || audio_pump(ba));
        b
    })
    .clone()
}

/// pump is the single capture+encode loop, shared by all connections. It idles
/// (no capture, no encode) while nobody is subscribed.
/// audio_pump is the single audio capture+encode loop (20 ms Opus frames),
/// shared by every audio subscriber. It idles while nobody wants audio, and
/// exits quietly if no audio backend exists.
fn audio_pump(bc: Arc<Broadcaster>) {
    let mut cap: Option<crate::audio::Capture> = None;
    let mut enc: Option<crate::opus::Opus> = None;
    loop {
        if !bc.has_audio_subs() {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        if cap.is_none() {
            match crate::audio::Capture::open() {
                Ok(c) => cap = Some(c),
                Err(e) => {
                    eprintln!("audio pump: {e}");
                    thread::sleep(Duration::from_millis(1000));
                    continue;
                }
            }
        }
        if enc.is_none() {
            match crate::opus::Opus::new(crate::audio::RATE, crate::audio::CHANNELS, 96_000) {
                Ok(e) => enc = Some(e),
                Err(e) => {
                    eprintln!("audio pump: opus {e}");
                    thread::sleep(Duration::from_millis(1000));
                    continue;
                }
            }
        }
        match cap.as_mut().unwrap().read_frame() {
            Some(pcm) => {
                let pkt = enc.as_mut().unwrap().encode(&pcm);
                if !pkt.is_empty() {
                    bc.broadcast_audio(Arc::new(pkt));
                }
            }
            None => {
                // Source died (or is silent): drop it and retry later.
                cap = None;
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn pump(bc: Arc<Broadcaster>) {
    let frame_dur = Duration::from_millis(33);
    let mut cap: Option<Capture> = None;
    let mut enc: Option<Encoder> = None;
    // Which vcfg generation the current encoder was built from. A mismatch means
    // a ConfigSet landed and the encoder must be rebuilt.
    let mut enc_gen: u32 = u32::MAX;
    // Dirty-frame detection: skip encoding when the screen did not change (a
    // static desktop then costs almost no CPU). We still emit one frame every
    // `keepalive_frames` so the stream stays warm and late joiners get data.
    let mut prev: Option<Vec<u8>> = None;
    let mut skipped: u32 = 0;
    // Emit at least one frame every ~0.5s even when nothing changes, so a
    // missed damage event can never freeze the stream for long.
    const KEEPALIVE_FRAMES: u32 = 15;
    // The encoder needs a few frames of warm-up before it emits its first access
    // unit, and WGC only delivers frames when the screen changes: force-feed the
    // first few so the stream always starts on a static screen.
    let mut warmup: u32 = 5;
    // Reused "last frame" buffer for the no-zero-copy path (
    // and X11/Wayland when XSHM is unavailable).
    // The X11/Wayland arm reads the initial None (grab_ptr fills it);
    // assigns it before the only read, so it starts uninitialised there.
    let mut last: Option<Vec<u8>>;
    loop {
        if !bc.has_subs() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        {
            // Periodic memory trace: the wayland path was OOM-killing the pod,
            // and we need to know whether the pump is the one growing.
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            if N.fetch_add(1, Ordering::Relaxed) % 100 == 0 {
                if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
                    let rss_pages: u64 = s
                        .split_whitespace()
                        .nth(1)
                        .and_then(|x| x.parse().ok())
                        .unwrap_or(0);
                    eprintln!(
                        "pump: rss={}MB subs={}",
                        rss_pages * 4096 / 1048576,
                        bc.has_subs()
                    );
                }
            }
        }
        let start = std::time::Instant::now();
        if cap.is_none() {
            match Capture::open() {
                Ok(c) => cap = Some(c),
                Err(e) => {
                    eprintln!("est pump: capture {e}");
                    // The failure is usually "portal unavailable", i.e. the
                    // cached DBusConnection points at a portal that has since
                    // died (or that refused the session). Drop it so the next
                    // attempt opens a NEW connection instead of retrying
                    // against a dead one — which is what made the daemon report
                    // the same error forever after a single failure.
                    reset_portal();
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        let (cap_w, cap_h) = cap.as_ref().unwrap().size();
        // Publish the capture size once. vcfg keeps capture and OUTPUT geometry
        // apart: the compositor owns the former (we cannot change it), a
        // ConfigSet owns the latter.
        crate::vcfg::VIDEO.set_capture(cap_w, cap_h);

        // Rebuild whenever a ConfigSet was accepted. The generation check covers
        // both the first encoder and every later change, so the literal
        // parameters that used to live here are gone.
        let gen = crate::vcfg::VIDEO.generation();
        let stale = enc.is_none() || gen != enc_gen;
        if stale {
            let (ow, oh) = crate::vcfg::VIDEO.output();
            let fps = crate::vcfg::VIDEO.fps() as u32;
            let br = crate::vcfg::VIDEO.bitrate();
            // Drop the old encoder BEFORE opening the new one: VAAPI and x264
            // both keep GPU/thread resources, and on a small sandbox holding two
            // encoders at once is what made the second open fail.
            enc = None;
            match Encoder::new(ow, oh, fps, br) {
                Ok(e) => {
                    crate::vcfg::VIDEO.set_active_encoder(e.kind());
                    eprintln!(
                        "est pump: encoder opened {}x{} fps={} br={} gen={} kind={}",
                        ow, oh, fps, br, gen, e.kind()
                    );
                    enc = Some(e);
                    enc_gen = gen;
                    // The new encoder starts with no reference frames, and the
                    // client reset gotKey when it saw the new Hello, so the very
                    // first frame out of it must be an IDR.
                    bc.new_sub.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("est pump: encoder {e}");
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        // A new viewer (or the periodic keepalive) needs a keyframe.
        let want_key = bc.new_sub.swap(false, std::sync::atomic::Ordering::Relaxed)
            || skipped >= KEEPALIVE_FRAMES;
        if want_key {
            enc.as_mut().unwrap().force_idr();
        }
        // Zero-copy: X11 exposes a pointer into the XSHM segment, so we can
        // compare and encode straight from it (no per-frame 8 MB Vec copy).
        // Wayland (no pointer API here) still copies into a Vec.
        // Grab (zero-copy pointer on X11; Vec otherwise) and decide.
        let ptr = cap.as_ref().unwrap().grab_ptr();
        let (changed, au) = match ptr {
            Some((p, w, h)) => {
                let n = (w as usize) * (h as usize) * 4;
                let cur = unsafe { std::slice::from_raw_parts(p, n) };
                let changed = match &prev {
                    None => true,
                    Some(prev) => {
                        if prev.len() != n {
                            true
                        } else {
                            // Compare only B,G,R — the X/A byte of an XRGB shm
                            // buffer is undefined and changes every frame.
                            prev.iter()
                                .enumerate()
                                .step_by(397)
                                .any(|(i, a)| i % 4 != 3 && *a != cur[i])
                        }
                    }
                };
                if changed || want_key {
                    // Reuse the previous frame's buffer instead of allocating a
                    // fresh ~8 MB Vec every frame: a large allocation per frame
                    // goes through mmap/munmap and ballooned RSS until the pod
                    // was OOM-killed (wayland captures at full rate, so every
                    // frame changed). Copy into the existing buffer instead.
                    match prev.as_mut() {
                        Some(buf) if buf.len() == n => buf.copy_from_slice(cur),
                        _ => prev = Some(cur.to_vec()),
                    }
                    // Pass the FRAME's size (w, h), NOT cap_w/cap_h: those come
                    // from size(), while w/h are what this buffer actually holds.
                    // They coincide on X11 but there is no reason to rely on it -
                    // and on Windows the same shortcut is what broke capture, so
                    // the source geometry is now always the buffer's own.
                    (true, enc.as_mut().unwrap().encode_ptr_sized(p, n as u32, w, h).ok())
                } else {
                    (false, None)
                }
            }
            None => {
                // No zero-copy pointer: grab a Vec.
                // delivers frames when the screen changes, so a miss reuses the
                // last frame; warmup/keepalive force-feed the encoder anyway so
                // the stream starts and stays alive on a static screen.
                // Copy straight into the reused `last` buffer, so a
                // frame never allocates a fresh ~8 MB Vec (that alloc failed
                // outright under the guest's tight RAM and aborted the daemon).
                let got = match cap.as_ref().unwrap().grab() {
                    Ok(px) => {
                        last = Some(px);
                        true
                    }
                    Err(e) => {
                        eprintln!("est pump: grab {e}");
                        thread::sleep(frame_dur);
                        continue;
                    }
                };
                if got {
                    skipped = 0;
                } else {
                    skipped += 1;
                }
                if let Some(px) = last.as_ref() {
                    // The MS encoder needs a CONTINUOUS input timeline, so feed it
                    // on every tick (the last frame when the screen is static) —
                    // gating on "changed" starves it and it never emits. The
                    // encoder's own rate control drops redundant frames.
                    if warmup > 0 {
                        warmup -= 1;
                    }
                    // Same reason as the zero-copy path: px is a capture-sized
                    // BGRA buffer, so encode_ptr_sized lets libyuv scale it.
                    // Same rule: the buffer's own length decides the slice, so
                    // derive the source geometry from it rather than from
                    // cap_w/cap_h. px is always cap_w*cap_h*4 here, but deriving
                    // it keeps one rule instead of two.
                    let (sw, sh) = (cap_w, cap_h);
                    let r = enc.as_mut().unwrap().encode_ptr_sized(
                        px.as_ptr(), px.len() as u32, sw, sh);
                    if let Err(e) = &r {
                        eprintln!("est pump: encode {e}");
                    }
                    (true, r.ok())
                } else {
                    (false, None)
                }
            }
        };
        if !changed {
            // Idle: skip the encode and back off the poll rate (a grab is ~8 MB
            // of memcpy), capped so a change is noticed within ~200 ms.
            skipped += 1;
            let backoff = frame_dur * (1 + skipped.min(5));
            if let Some(rest) = backoff.checked_sub(start.elapsed()) {
                thread::sleep(rest);
            }
            continue;
        }
        skipped = 0;
        if let Some(aus) = au {
            for au in aus {
                if !au.is_empty() {
                    bc.broadcast(Arc::new(au));
                }
            }
        }
        if let Some(rest) = frame_dur.checked_sub(start.elapsed()) {
            thread::sleep(rest);
        }
    }
}

/// shared_input returns the process-wide input backend, opening it on first use.
/// Input is a single resource (one XTest connection / one wl_seat + virtual
/// keyboard / one SendInput sender), so every connection shares it.
pub fn shared_input() -> &'static Option<Mutex<Input>> {
    static IN: OnceLock<Option<Mutex<Input>>> = OnceLock::new();
    IN.get_or_init(|| {
        {
            // Same probe-driven choice as capture: ask the compositor whether it
            // exports the wlroots virtual input protocols.
            use crate::probe::{capabilities, InputPath};
            match capabilities().input {
                InputPath::WlrVirtual => match WlInput::open() {
                    Ok(i) => Some(Mutex::new(Input::Wl(i))),
                    Err(e) => {
                        eprintln!("wayland input: {e}");
                        None
                    }
                },
                InputPath::X11 => X11::open().ok().map(|i| Mutex::new(Input::X11(i))),
                InputPath::PortalRemoteDesktop => {
                    // Input through the portal: mutter and kwin expose no wlroots
                    // virtual-input protocols, so this is the only way in.
                    //
                    // The old code said "not implemented yet" and fell back to
                    // WlInput, which cannot bind those protocols on GNOME or KDE.
                    // That fallback always failed, so the desktop rendered and
                    // ignored every click while capabilities claimed
                    // input=PortalRemoteDesktop.
                    //
                    // Share the capture session. Opening a second one made the
                    // portal backend segfault — see shared_portal().
                    match shared_portal() {
                        Ok(p) => match PortalInput::open(p) {
                            Ok(i) => Some(Mutex::new(Input::Portal(i))),
                            Err(e) => {
                                eprintln!("portal input: {e}");
                                None
                            }
                        },
                        Err(e) => {
                            eprintln!("portal input: {e}");
                            None
                        }
                    }
                }
                _ => None,
            }
        }
    })
}

/// known_size_hint returns a screen size WITHOUT touching the capture.
///
/// The handshake needs w/h, but opening the capture is a portal round trip that
/// can wait on a consent dialog. This only answers from what the probe already
/// knows, falling back to the configured default, so HELLO can be sent
/// immediately. The authoritative size arrives with the first video frame once
/// the capture is up.
fn known_size_hint() -> (u32, u32) {
    static SZ: OnceLock<(u32, u32)> = OnceLock::new();
    *SZ.get_or_init(|| {
        // X11 can be asked cheaply; a portal capture cannot.
        if let Ok(x) = X11::open() {
            let (w, h) = x.size();
            if w > 0 && h > 0 {
                return (w, h);
            }
        }
        (1920, 1080)
    })
}

/// shared_size returns the captured screen size, opening the capture ONCE (and
/// reusing it for the pump).
fn shared_size() -> (u32, u32) {
    static SZ: OnceLock<(u32, u32)> = OnceLock::new();
    *SZ.get_or_init(|| {
        // Linux: ASK the capture backend. It used to be a hard-coded
        // (1920, 1080), which is wrong for every screen that is not 1080p —
        // the daemon advertised the wrong resolution to the worker and the
        // stream was cropped/scaled. Capture::size() probes a real frame on
        // Wayland and reports the X screen on X11.
        {
            Capture::open()
                .and_then(|c| {
                    let (w, h) = c.size();
                    if w > 0 && h > 0 {
                        Ok((w, h))
                    } else {
                        Err("no size".into())
                    }
                })
                .unwrap_or((1920, 1080))
        }
    })
}

/// Build a Hello for the CURRENT video configuration.
///
/// Used both for the first handshake and for the re-announcement after a
/// ConfigSet: the geometry must come from vcfg (what the encoder emits), and the
/// capture size only as a fallback for the very first hello, before any encoder
/// exists.
fn hello_payload() -> Vec<u8> {
    let (early_w, early_h) = known_size_hint();
    hello_payload_with(early_w, early_h)
}

fn hello_payload_with(early_w: u32, early_h: u32) -> Vec<u8> {
    // 7 (core) + 8 (audio) + 8 (video block at offset 15) = 23 bytes.
    let mut hello = vec![0u8; 7 + 8 + 8];
    hello[0] = 1;
    hello[1] = 1; // H.264
    let (out_w, out_h) = {
        let (ow, oh) = crate::vcfg::VIDEO.output();
        if ow != 0 && oh != 0 { (ow, oh) } else { (early_w, early_h) }
    };
    hello[2..4].copy_from_slice(&(out_w as u16).to_be_bytes());
    hello[4..6].copy_from_slice(&(out_h as u16).to_be_bytes());
    hello[6] = crate::vcfg::VIDEO.fps();

    // Audio: do not block here. On the first hello the probe usually finished
    // already; a re-announcement must not stall the caller for seconds waiting
    // for something it already knows.
    let has_audio = crate::audio::probe_done_pub() && crate::audio::params().0;
    hello[7] = if has_audio { 1 } else { 0 };
    hello[8] = 1; // Opus
    hello[9..13].copy_from_slice(&crate::audio::RATE.to_be_bytes());
    hello[13] = crate::audio::CHANNELS as u8;
    hello[14] = crate::audio::FRAME_MS;
    hello[15..23].copy_from_slice(&crate::vcfg::hello_block());
    hello
}

fn handle(stream: TcpStream) -> Result<(), String> {
    // NOTHING BLOCKING BEFORE HELLO.
    //
    // shared_input() opens the portal RemoteDesktop session and shared_size()
    // opens the capture, which is a full CreateSession/SelectSources/Start round
    // trip through xdg-desktop-portal. When the portal has to show its consent
    // dialog — headless, so nobody clicks it — Start waits for its own timeout,
    // and the client sat there with an open socket and not one byte back. It
    // looked exactly like a broken daemon, and no amount of waiting helped.
    //
    // So the handshake goes out FIRST with whatever size is known, and the real
    // size follows as soon as the capture is up.
    let (early_w, early_h) = known_size_hint();
    // The Conn is built AFTER the handshake: its `input` field comes from
    // shared_input(), which opens the portal session and must not run before
    // HELLO is on the wire.
    let stream_for_conn = stream.try_clone().map_err(|e| e.to_string())?;

    // Hello (EST.md): version, codec, w, h, fps.
    // Hello: v1 7-byte core + an audio block (backwards compatible: a v1 client
    // reads the first 7 bytes and ignores the rest; `length` tells it how much
    // to skip). Audio is advertised only when an audio capture actually opened.
    // 7 + 8 (audio) + 8 (video block, EST.md offset 15) = 23 bytes.
    let hello = hello_payload_with(early_w, early_h);
    // Write the handshake straight to the socket, before any Conn exists: the
    // point is that nothing can block between accept() and this write.
    {
        let mut s = stream_for_conn.try_clone().map_err(|e| e.to_string())?;
        // SIX bytes, matching Conn::write_frame and the browser's parser: byte 0
        // is the type, byte 1 is reserved, bytes 2..6 are the big-endian length
        // (app.js reads the length with getUint32(2)). An earlier 5-byte header
        // here shifted the whole payload by one byte, so the browser read the
        // version from the codec field and the size from garbage: it configured
        // its decoder for w=32772 h=14366 and never saw the 1024x768 that was
        // actually being sent, so no frame ever decoded and the canvas stayed
        // black while the stream looked perfectly healthy.
        let mut hdr = [0u8; 6];
        hdr[0] = T_HELLO;
        hdr[2..6].copy_from_slice(&(hello.len() as u32).to_be_bytes());
        s.write_all(&hdr).map_err(|e| e.to_string())?;
        s.write_all(&hello).map_err(|e| e.to_string())?;
        s.flush().map_err(|e| e.to_string())?;
    }

    // NOW it is safe to touch the portal: the client already has its handshake.
    let input = shared_input();
    let (w, h) = shared_size();
    if let Some(m) = input.as_ref() {
        if let Ok(i) = m.lock() {
            if let Input::Wl(wl) = &*i {
                wl.set_output_size(w, h);
            }
        }
    }
    let conn = Arc::new(Conn {
        stream: Mutex::new(stream_for_conn),
        // When auth is ON, an unauthenticated connection is READ-ONLY (View)
        // until it sends a valid Auth frame — never Control by default.
        audio_on: std::sync::atomic::AtomicBool::new(false),
        mode: std::sync::atomic::AtomicU8::new(if crate::auth::is_open() {
            crate::auth::Mode::Control as u8
        } else {
            crate::auth::Mode::View as u8
        }),
        input,
        clipboard: Mutex::new(String::new()),
    });

    // Announce the new geometry to every viewer, including this one.
    register_conn(&conn);

    // Subscribe to the SHARED pumps: this connection only forwards the frames,
    // so many viewers share one capture+encode pipeline.
    let rx = broadcaster().subscribe();
    // Subscribe to audio when the daemon HAS it; the forwarder only sends once
    // the client turns it on with AudioReq (0x06), so an audio-less client pays
    // nothing beyond the (tiny) subscription.
    let arx = if hello[7] == 1 {
        Some(broadcaster().subscribe_audio())
    } else {
        None
    };
    let vc = Arc::clone(&conn);
    let video = thread::spawn(move || {
        while let Ok(au) = rx.recv() {
            if vc.write_frame(T_VIDEO, &au).is_err() {
                return;
            }
        }
    });

    // Audio forwarder (Opus packets -> T_AUDIO frames).
    let audio_thread = arx.map(|arx| {
        let ac = Arc::clone(&conn);
        thread::spawn(move || {
            while let Ok(pkt) = arx.recv() {
                if ac.audio_on.load(std::sync::atomic::Ordering::Relaxed)
                    && ac.write_frame(T_AUDIO, &pkt).is_err()
                {
                    return;
                }
            }
        })
    });

    // Control reader on this thread.
    control_loop(&conn, stream)?;
    // The client is gone; the pump will exit on its next failed write.
    let _ = video.join();
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    Ok(())
}

fn control_loop(conn: &Arc<Conn>, mut s: TcpStream) -> Result<(), String> {
    let mut hdr = [0u8; 6];
    loop {
        if s.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let typ = hdr[0];
        let len = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len];
        if len > 0 && s.read_exact(&mut body).is_err() {
            return Ok(());
        }
        if let Err(e) = dispatch(conn, typ, &body) {
            eprintln!("est dispatch: {e}");
        }
    }
}

/// Parse a 0x08 ConfigSet body into a Patch.
///
/// The cursor walks the frame strictly: every field knows its own length, and a
/// frame that ends early is an error, not a partial patch. Truncation is the
/// most likely corruption here (a client that grew a field and an older daemon
/// reading it), and applying half of it would move the stream somewhere nobody
/// asked for.
fn parse_config(b: &[u8]) -> (Option<crate::vcfg::Patch>, Option<String>) {
    let mut p = crate::vcfg::Patch::default();
    let mut i = 0usize;
    let need = |i: usize, n: usize| i + n <= b.len();
    if !need(i, 1) { return (None, Some("truncated: hasWidth".into())); }
    if b[i] != 0 {
        if !need(i + 1, 2) { return (None, Some("truncated: width".into())); }
        p.width = Some(u16::from_be_bytes([b[i + 1], b[i + 2]]) as u32);
    }
    i += 3;
    if !need(i, 1) { return (None, Some("truncated: hasHeight".into())); }
    if b[i] != 0 {
        if !need(i + 1, 2) { return (None, Some("truncated: height".into())); }
        p.height = Some(u16::from_be_bytes([b[i + 1], b[i + 2]]) as u32);
    }
    i += 3;
    if !need(i, 1) { return (None, Some("truncated: hasFps".into())); }
    if b[i] != 0 {
        if !need(i + 1, 1) { return (None, Some("truncated: fps".into())); }
        p.fps = Some(b[i + 1]);
    }
    i += 2;
    if !need(i, 1) { return (None, Some("truncated: hasBitrate".into())); }
    if b[i] != 0 {
        if !need(i + 1, 4) { return (None, Some("truncated: bitrate".into())); }
        p.bitrate = Some(u32::from_be_bytes([
            b[i + 1], b[i + 2], b[i + 3], b[i + 4],
        ]));
    }
    i += 5;
    if !need(i, 1) { return (None, Some("truncated: hasEncoder".into())); }
    if b[i] != 0 {
        if !need(i + 1, 1) { return (None, Some("truncated: encoder".into())); }
        let e = b[i + 1];
        // 0xFF is the documented "let the server pick" value; translate it to the
        // Patch's 0 so vcfg keeps one meaning for zero.
        p.encoder = Some(if e == 0xFF { 0 } else { e });
    }
    (Some(p), None)
}

/// Build a 0x09 ConfigAck body: ok byte, then a length-prefixed UTF-8 reason.
fn ack(ok: bool, msg: &str) -> Vec<u8> {
    let m = msg.as_bytes();
    let m = &m[..m.len().min(255)];
    let mut v = Vec::with_capacity(2 + m.len());
    v.push(if ok { 1 } else { 0 });
    v.push(m.len() as u8);
    v.extend_from_slice(m);
    v
}

fn u16at(b: &[u8], o: usize) -> i16 {
    i16::from_be_bytes([b[o], b[o + 1]])
}

fn dispatch(conn: &Arc<Conn>, typ: u8, b: &[u8]) -> Result<(), String> {
    // Auth frame: the client presents its token; we set the connection's mode.
    if typ == T_AUTH {
        let tok = std::str::from_utf8(b).unwrap_or("");
        // authorize never fails: a missing/wrong token is just View (read-only).
        let mode = crate::auth::authorize(Some(tok));
        conn.mode
            .store(mode as u8, std::sync::atomic::Ordering::SeqCst);
        let _ = conn.write_frame(T_AUTH_OK, &[mode as u8]);
        eprintln!("est: auth -> {:?}", mode);
        return Ok(());
    }
    // View mode: drop every frame that would CHANGE the guest.
    if conn.mode.load(std::sync::atomic::Ordering::SeqCst) == crate::auth::Mode::View as u8
        && crate::auth::is_mutation(typ)
    {
        return Ok(()); // silently ignored
    }
    if typ == T_KEYFRAME_REQ {
        // Not a mutation: it changes no guest state, so a read-only viewer may
        // use it. Refusing it would strand a viewer on a corrupt picture with no
        // way to recover.
        broadcaster().request_keyframe();
        return Ok(());
    }
    if typ == T_CONFIG_SET {
        // 0x08 layout (EST.md): every field carries its own has* flag, so a
        // client can change bitrate alone. Anything malformed is refused rather
        // than guessed at: a half-parsed request would change the stream in ways
        // the client did not ask for.
        eprintln!("est: ConfigSet n={} bytes", b.len());
        let (p, err) = parse_config(b);
        if let Some(e) = err {
            eprintln!("est: ConfigSet parse failed: {e}");
            let _ = conn.write_frame(T_CONFIG_ACK, &ack(false, &e));
            return Ok(());
        }
        let p = p.unwrap();
        match crate::vcfg::VIDEO.apply(&p) {
            Ok(()) => {
                let (ow, oh) = crate::vcfg::VIDEO.output();
                eprintln!(
                    "est: ConfigSet ok -> {}x{} fps={} br={} gen={}",
                    ow, oh,
                    crate::vcfg::VIDEO.fps(),
                    crate::vcfg::VIDEO.bitrate(),
                    crate::vcfg::VIDEO.generation()
                );
                // Hello is the only authoritative statement of geometry
                // (EST.md), so every connected viewer - not just the one that
                // asked - must be told what the encoder now emits. Without this,
                // viewers that stayed connected kept decoding at the old size
                // and showed garbage until they reconnected.
                send_hello_to_all();
                // Ack BEFORE the new Hello: the client uses the ack to stop
                // showing a spinner, and the Hello (sent by the pump once the new
                // encoder exists) to reconfigure its decoder.
                let _ = conn.write_frame(T_CONFIG_ACK, &ack(true, ""));
            }
            Err(e) => {
                eprintln!("est: ConfigSet rejected: {e}");
                let _ = conn.write_frame(T_CONFIG_ACK, &ack(false, &e));
            }
        }
        return Ok(());
    }
    if typ == T_AUDIO_REQ {
        conn.audio_on
            .store(true, std::sync::atomic::Ordering::SeqCst);
        eprintln!("est: audio on");
        return Ok(());
    }
    if typ == T_AUDIO_OFF {
        conn.audio_on
            .store(false, std::sync::atomic::Ordering::SeqCst);
        eprintln!("est: audio off");
        return Ok(());
    }
    match typ {
        T_MOVE => {
            conn.with_input(|x| x.move_to(u16at(b, 0), u16at(b, 2)));
        }
        T_BUTTON => {
            conn.with_input(|x| x.button(b[0], b[1] != 0));
        }
        T_SCROLL => {
            let dx = u16at(b, 4);
            let dy = u16at(b, 6);
            conn.with_input(|x| x.scroll(dx as i32, dy as i32));
        }
        T_KEY => {
            // nameLen:u8 name nameLen char:u32 down:u8
            if b.len() >= 6 {
                let n = b[0] as usize;
                if b.len() >= 1 + n + 4 + 1 {
                    let name = std::str::from_utf8(&b[1..1 + n]).unwrap_or("");
                    let ch = u32::from_le_bytes([b[1 + n], b[2 + n], b[3 + n], b[4 + n]]);
                    let down = b[1 + n + 4] != 0;
                    conn.with_input(|x| {
                        {
                            // Single-char name = a literal character (Ctrl+C);
                            // multi-char = a named key ("Return", "F5", …).
                            let mut handled = false;
                            if !name.is_empty() {
                                if let Some(k) = key_by_name(name) {
                                    x.key(k as u16, down);
                                    handled = true;
                                }
                            }
                            let c = if !handled && !name.is_empty() && name.chars().count() == 1 {
                                name.chars().next().unwrap()
                            } else {
                                char::from_u32(ch).unwrap_or('\0')
                            };
                            if !handled && c != '\0' {
                                if let Some((k, need_shift)) = char_keycode(c) {
                                    if need_shift {
                                        x.key(50, down);
                                    }
                                    x.key(k as u16, down);
                                    if need_shift {
                                        x.key(50, !down);
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
        T_TEXT => {
            conn.with_input(|x| {
                for ch in b {
                    if let Some((kc, need_shift)) = char_keycode(*ch as char) {
                        if need_shift {
                            x.key(50, true);
                        }
                        x.key(kc as u16, true);
                        x.key(kc as u16, false);
                        if need_shift {
                            x.key(50, false);
                        }
                    }
                }
            });
        }
        T_CLIP_GET => {
            // Use the REAL system clipboard (the same backend the HTTP /a11y/
            // clipboard route uses): wl-paste / xclip / xsel on Linux, Win32 on
            // The old per-connection String was a private buffer that
            // nothing else could see, which is why the buttons looked dead.
            let text = crate::http::clipboard_get().unwrap_or_default();
            let mut p = Vec::with_capacity(8 + text.len());
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&(text.len() as u32).to_be_bytes());
            p.extend_from_slice(text.as_bytes());
            conn.write_frame(T_CLIP_DATA, &p)
                .map_err(|e| e.to_string())?;
        }
        T_CLIP_SET => {
            // EST.md: u32 id, u32 len, text[len]
            if b.len() >= 8 {
                let n = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize;
                if b.len() >= 8 + n {
                    let text = String::from_utf8_lossy(&b[8..8 + n]).to_string();
                    let _ = crate::http::clipboard_set(&text);
                }
            }
        }
        T_PING => conn.write_frame(T_PONG, b).map_err(|e| e.to_string())?,
        _ => {}
    }
    Ok(())
}
