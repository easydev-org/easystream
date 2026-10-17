//! Startup capability probe.
//!
//! Linux desktops differ wildly in HOW each capability is reachable, and the
//! paths have very different costs:
//!
//!   capture : zwlr_screencopy (wlroots, instant) | portal ScreenCast (needs a
//!             D-Bus handshake, may need a dialog) | X11 XGetImage
//!   input   : zwlr_virtual_pointer/keyboard | portal RemoteDesktop | X11 XTest
//!   a11y    : AT-SPI over the session bus (there is no portal for this)
//!   clipboard: wl-clipboard / xclip
//!
//! Probing once at startup — by asking the compositor which globals it exports,
//! and the portal which interfaces it exports — is both more accurate and much
//! cheaper than the old "try one path, wait for it to fail, fall back".
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Wayland,
    X11,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePath {
    /// zwlr_screencopy_manager_v1 — wlroots and the compositors that ship it
    /// (sway, labwc, cage, wayfire, hyprland, niri).
    ZwlrScreencopy,
    /// xdg-desktop-portal ScreenCast + PipeWire — kwin/mutter.
    Portal,
    /// XGetImage / XSHM.
    X11,
    Windows,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPath {
    /// zwlr_virtual_pointer_manager_v1 + zwp_virtual_keyboard_manager_v1.
    WlrVirtual,
    /// xdg-desktop-portal RemoteDesktop.
    PortalRemoteDesktop,
    /// XTest.
    X11,
    Windows,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A11yPath {
    /// org.a11y.Bus + org.a11y.atspi.Registry on the session bus.
    Atspi,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPath {
    Wl,
    X11,
    None,
}

/// Capabilities is the result of probing the running desktop once.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub session: Session,
    pub capture: CapturePath,
    pub input: InputPath,
    pub a11y: A11yPath,
    pub clipboard: ClipboardPath,
    /// Human-readable detail for logs / the /capabilities endpoint.
    pub notes: Vec<String>,
}

impl Capabilities {
    pub fn to_json(&self) -> String {
        let notes = self
            .notes
            .iter()
            .map(|n| format!("\"{}\"", n.replace('"', "'")))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"session\":\"{:?}\",\"capture\":\"{:?}\",\"input\":\"{:?}\",\
             \"a11y\":\"{:?}\",\"clipboard\":\"{:?}\",\"notes\":[{}]}}",
            self.session, self.capture, self.input, self.a11y, self.clipboard, notes
        )
    }
}

// ── C shims ─────────────────────────────────────────────────────────────────
#[cfg(not(windows))]
extern "C" {
    fn na_probe_wayland(out: *mut libc::c_int) -> libc::c_int;
}

#[cfg(not(windows))]
fn wayland_globals() -> Option<[i32; 8]> {
    let mut out = [0i32; 8];
    let rc = unsafe { na_probe_wayland(out.as_mut_ptr()) };
    if rc == 0 { Some(out) } else { None }
}

/// probe runs once and caches the result for the process lifetime.
pub fn capabilities() -> &'static Capabilities {
    static C: OnceLock<Capabilities> = OnceLock::new();
    C.get_or_init(detect)
}

#[cfg(windows)]
fn detect() -> Capabilities {
    Capabilities {
        session: Session::None,
        capture: CapturePath::Windows,
        input: InputPath::Windows,
        a11y: A11yPath::None,
        clipboard: ClipboardPath::None,
        notes: vec!["windows".into()],
    }
}

#[cfg(not(windows))]
fn detect() -> Capabilities {
    let mut notes = Vec::new();

    let wl_env = std::env::var("WAYLAND_DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let x_env = std::env::var("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false);

    // Ask the compositor directly. This is the reliable part: the globals are
    // exactly the protocols a client can use.
    let globals = if wl_env { wayland_globals() } else { None };

    let session = if globals.is_some() {
        Session::Wayland
    } else if wl_env {
        // WAYLAND_DISPLAY was set but we could not connect: fall through to X11.
        notes.push("WAYLAND_DISPLAY set but connect failed".into());
        if x_env { Session::X11 } else { Session::None }
    } else if x_env {
        Session::X11
    } else {
        Session::None
    };

    let mut capture = CapturePath::None;
    let mut input = InputPath::None;
    match session {
        Session::Wayland => {
            let g = globals.unwrap_or([0; 8]);
            let has_screencopy = g[0] == 1;
            let has_vptr = g[1] == 1;
            let has_vkbd = g[2] == 1;
            notes.push(format!(
                "wayland globals={} screencopy={} vptr={} vkbd={}",
                g[6], has_screencopy, has_vptr, has_vkbd
            ));

            if has_screencopy {
                capture = CapturePath::ZwlrScreencopy;
            } else if portal_has("ScreenCast") {
                capture = CapturePath::Portal;
                notes.push("portal ScreenCast".into());
            } else if x_env {
                capture = CapturePath::X11;
                notes.push("no screencopy, no portal: X11 fallback".into());
            }

            if has_vptr && has_vkbd {
                input = InputPath::WlrVirtual;
            } else if portal_has("RemoteDesktop") {
                input = InputPath::PortalRemoteDesktop;
            } else if x_env {
                input = InputPath::X11;
            }
        }
        Session::X11 => {
            capture = CapturePath::X11;
            input = InputPath::X11;
        }
        Session::None => {}
    }

    // AT-SPI has no portal interface: it is a session-bus service, and it is
    // LAZY — org.a11y.Bus only appears once a client asks for it. So probing it
    // at startup reports "None" even on desktops where it works perfectly
    // (observed on sway: /capabilities said None while /a11y/apps worked).
    // Report it as "probe at first use" rather than lying.
    let a11y = A11yPath::Atspi;
    notes.push("a11y: AT-SPI is lazy; checked on first use".into());

    let clipboard = if session == Session::Wayland && which("wl-copy") {
        ClipboardPath::Wl
    } else if which("xclip") || which("xsel") {
        ClipboardPath::X11
    } else {
        ClipboardPath::None
    };

    Capabilities { session, capture, input, a11y, clipboard, notes }
}

#[cfg(not(windows))]
fn which(bin: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| std::path::Path::new(d).join(bin).is_file())
}

// ── D-Bus probes ────────────────────────────────────────────────────────────
//
// We shell out to busctl (every desktop image has it) rather than linking
// libdbus, because this runs once.
//
// EVERY call is wrapped in `timeout`, and this is load-bearing: on an X11
// desktop the session bus address points at a socket that is not there, and
// busctl then sits in a connect/D-Bus timeout. capabilities() is a OnceLock, so
// a hung probe hangs the WHOLE daemon at startup — which is exactly what
// happened: the X11 images started easystream and it never listened.
//
// `timeout` is coreutils and is present in the base image; if it is somehow
// missing the command fails immediately, which is the safe direction.
#[cfg(not(windows))]
fn busctl(args: &[&str]) -> String {
    std::process::Command::new("timeout")
        .arg("3")
        .arg("busctl")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn dbus_name_has_owner(name: &str) -> bool {
    busctl(&["--user", "list", "--no-legend", "--no-pager"]).contains(name)
}

#[cfg(not(windows))]
fn portal_has(iface: &str) -> bool {
    busctl(&[
        "--user",
        "introspect",
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
    ])
    .contains(iface)
}
