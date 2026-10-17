//! Audio capture for Linux — TWO backends, PipeWire FIRST, PulseAudio as the
//! fallback:
//!
//!   1. PipeWire (`pw-record`) — the native modern Linux audio graph.
//!   2. PulseAudio (`parec`)   — its `module-null-sink` carries its OWN timer
//!      clock, so a virtual sink actually RUNS on a machine with no sound card
//!      (PipeWire's null sink does not).
//!
//! Both emit s16le / 48 kHz / stereo on stdout, so the rest of the pipeline is
//! identical.
//!
//! PRE-DETECTION: at startup we don't merely check that the tool exists — we
//! OPEN it and verify a real frame of PCM flows (with a short timeout). The
//! first backend that actually produces data is chosen and cached; if neither
//! does, the daemon advertises no audio (video keeps working).
//!
//! The source name comes from EAZYSTREAM_AUDIO_SOURCE (default `vsink.monitor`,
//! the monitor of a null sink named `vsink`). Empty = let the tool pick the
//! default source.
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

pub struct Capture {
    child: Child,
    /// leftover bytes from the previous read (a frame boundary may fall mid-read)
    buf: Vec<u8>,
    backend: &'static str,
}

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_MS: u8 = 20;
/// samples per 20 ms frame, ALL channels interleaved (per-channel * channels).
pub const FRAME_SAMPLES: usize = (RATE as usize / 50) * CHANNELS as usize;

/// Backend order: PipeWire first, PulseAudio as the fallback.
const ORDER: [&str; 2] = ["pipewire", "pulse"];

fn source() -> String {
    std::env::var("EAZYSTREAM_AUDIO_SOURCE").unwrap_or_else(|_| "vsink.monitor".into())
}

fn prog(backend: &str) -> &'static str {
    match backend {
        "pipewire" => "pw-record",
        _ => "parec",
    }
}

/// which reports whether a program is on PATH (no external `which` needed).
fn which(name: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':').any(|d| std::path::Path::new(d).join(name).is_file())
}

/// params reports the audio parameters advertised in Hello. `present` is true
/// only when a backend was PROVEN to produce data.
///
/// The probe runs ONCE in the background (it takes up to ~1.5s per candidate);
/// Hello must not block on it, so until it finishes we report `false` and the
/// client simply gets no audio block. `ensure_probed()` is called on the audio
/// path so the work happens exactly once.
pub fn params() -> (bool, u32, u8, u8) {
    (probe_done() && chosen().is_some(), RATE, CHANNELS as u8, FRAME_MS)
}

/// ensure_probed starts the (once) background probe and returns immediately.
pub fn ensure_probed() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| {
            let _ = chosen();
            PROBED.store(true, std::sync::atomic::Ordering::Release);
        });
    });
}

pub fn probe_done_pub() -> bool { probe_done() }
fn probe_done() -> bool {
    PROBED.load(std::sync::atomic::Ordering::Acquire)
}

static PROBED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// chosen is the pre-detected backend, cached. It verifies each candidate by
/// actually reading a frame, so a tool that is installed but cannot capture
/// (e.g. PipeWire with a suspended null sink) is skipped.
fn chosen() -> Option<&'static str> {
    static B: OnceLock<Option<&'static str>> = OnceLock::new();
    *B.get_or_init(|| {
        for name in ORDER {
            if !which(prog(name)) {
                continue;
            }
            if verify(name) {
                eprintln!("audio: backend = {name} (verified)");
                return Some(name);
            }
            eprintln!("audio: backend {name} present but produced no data, trying next");
        }
        eprintln!("audio: no working capture backend");
        None
    })
}

/// verify opens `backend`, reads ONE frame with a short timeout, and kills it.
/// A backend that yields no data in time is rejected (this is what makes the
/// PipeWire→PulseAudio fallback work in a container with no sound card).
fn verify(name: &'static str) -> bool {
    let mut cap = match open_backend(name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("audio: {name} open failed: {e}");
            return false;
        }
    };
    let pid = cap.child.id() as i32;
    eprintln!("audio: {name} probing (pid={pid})");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let got = cap.read_frame().is_some();
        let _ = tx.send(got);
    });
    match rx.recv_timeout(Duration::from_millis(4000)) {
        Ok(got) => { eprintln!("audio: {name} probe -> {got}"); got }
        Err(_) => {
            eprintln!("audio: {name} probe TIMED OUT (4s)");
            unsafe { libc::kill(pid, libc::SIGKILL) };
            false
        }
    }
}

impl Capture {
    /// open starts capturing from the PRE-DETECTED backend as s16le/48k/stereo.
    pub fn open() -> Result<Capture, String> {
        let name = chosen().ok_or("no working audio backend")?;
        open_backend(name)
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// read_frame blocks until one 20 ms frame of s16le interleaved PCM is
    /// available. Returns None on EOF/error.
    pub fn read_frame(&mut self) -> Option<Vec<i16>> {
        let want = FRAME_SAMPLES * 2; // bytes (s16)
        let stdout = self.child.stdout.as_mut()?;
        while self.buf.len() < want {
            let mut chunk = [0u8; 4096];
            match stdout.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => return None,
            }
        }
        let frame_bytes: Vec<u8> = self.buf.drain(..want).collect();
        let pcm: Vec<i16> = frame_bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(pcm)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// open_backend spawns the capture tool for `backend`.
fn open_backend(backend: &str) -> Result<Capture, String> {
    let src = source();
    let args: Vec<String> = match backend {
        "pipewire" => {
            let mut a: Vec<String> = vec![
                "--format".into(), "s16".into(),
                "--rate".into(), RATE.to_string(),
                "--channels".into(), CHANNELS.to_string(),
            ];
            if !src.is_empty() {
                a.splice(0..0, ["--target".into(), src.clone()]);
            }
            a.push("-".into());
            a
        }
        _ => {
            // parec: --device is optional (default source when empty).
            let mut a: Vec<String> = vec![
                "--format=s16le".into(),
                format!("--rate={RATE}"),
                format!("--channels={CHANNELS}"),
                "--raw".into(),
            ];
            if !src.is_empty() {
                a.insert(0, format!("--device={src}"));
            }
            a
        }
    };
    let child = Command::new(prog(backend))
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", prog(backend)))?;
    // Sanity: keep the fd handle referenced (avoids a warning on some targets).
    let _ = child.stdout.as_ref().map(|s| s.as_raw_fd());
    Ok(Capture { child, buf: Vec::new(), backend: if backend == "pipewire" { "pipewire" } else { "pulse" } })
}
