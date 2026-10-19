//! The daemon's a11y surface, exposed over HTTP (A11Y_HTTP.md).
//!
//! AT-SPI2 over D-Bus. It produces the SAME text formats the one-shot CLI
//! prints, so the HTTP layer is identical to the other platforms'.

/// The a11y backend.
pub enum Backend {
    Atspi(crate::atspi::Atspi),
}

impl Backend {
    pub fn open() -> Result<Backend, String> {
        {
            Ok(Backend::Atspi(crate::atspi::Atspi::open()?))
        }
    }

    /// apps returns the tab-separated `pid\tname\tfocused` listing.
    pub fn apps(&self) -> Result<String, String> {
        match self {
            Backend::Atspi(a) => a.apps(),
        }
    }

    pub fn tree(&self, pid: u32, app: &str, depth: u32) -> Result<String, String> {
        match self {
            Backend::Atspi(a) => a.tree(pid, app, depth),
        }
    }

    pub fn find(&self, pid: u32, app: &str, sel: &str, out: u8) -> Result<String, String> {
        match self {
            Backend::Atspi(a) => a.find(pid, app, sel, out),
        }
    }

    pub fn action(&self, pid: u32, app: &str, sel: &str, action: &str, value: Option<&str>) -> Result<String, String> {
        match self {
            Backend::Atspi(a) => a.action(pid, app, sel, action, value),
        }
    }
}
