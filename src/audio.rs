//! Audio capture. Linux/Windows/macOS each have their own system-audio API; for
//! the Linux (Wayland/X11) sandbox we read from PipeWire via `pw-record`
//! (s16le, 48 kHz) — the only sink available is the virtual one the container
//! sets up, whose monitor carries the guest's audio output.
//!
//! The capture is deliberately best-effort: if no audio source exists (a
//! headless container with no sound card) `open()` fails and the daemon simply
//! sends no audio — video keeps working.
use std::io::Read;
use std::process::{Child, Command, Stdio};

pub struct Capture {
    child: Child,
    /// leftover bytes from the previous read (a frame boundary may fall mid-read)
    buf: Vec<u8>,
}

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_MS: u8 = 20;
/// samples per 20 ms frame, ALL channels interleaved (per-channel * channels).
pub const FRAME_SAMPLES: usize = (RATE as usize / 50) * CHANNELS as usize;

/// params reports the audio parameters advertised in Hello. `present` is true
/// only when a capture source actually opened (so the client knows whether to
/// expect Audio frames at all).
pub fn params() -> (bool, u32, u8, u8) {
    (probe(), RATE, CHANNELS as u8, FRAME_MS)
}

/// probe reports whether an audio capture tool EXISTS (non-blocking — it must
/// NOT read a frame, or a container with no audio source would block the whole
/// connection handler before Hello is even sent). Actual capture happens in the
/// audio pump, which tolerates silence.
fn probe() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let has = which("pw-record") || which("parec");
        eprintln!("audio: capture {}", if has { "tool present" } else { "unavailable" });
        has
    })
}

/// which reports whether a program is on PATH (no external `which` needed).
fn which(name: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':').any(|d| {
        let p = std::path::Path::new(d).join(name);
        p.is_file()
    })
}

impl Capture {
    /// open starts capturing system audio as s16le/48k/stereo on stdout.
    pub fn open() -> Result<Capture, String> {
        // pw-record writes raw PCM to stdout with --format; the target is the
        // default sink's monitor (empty target = default source).
        let child = Command::new("pw-record")
            .args([
                "--format", "s16",
                "--rate", "48000",
                "--channels", "2",
                "-", // stdout
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("pw-record: {e}"))?;
        Ok(Capture { child, buf: Vec::new() })
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
