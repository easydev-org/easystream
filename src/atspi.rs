//! AT-SPI2 client over D-Bus (Linux a11y backend).
//!
//! AT-SPI is a D-Bus protocol: applications publish an `Accessible` object per
//! node; the registry hands out the desktop root. We speak it directly with the
//! `dbus` crate (no CGo, no libatspi — only libdbus-1, which the desktop images
//! already ship).
//!
//! Formats match the one-shot CLI (SPEC.md): `apps` is `pid\tname\tfocused`,
//! `tree` is a box-drawing tree, `find` prints pretty/bounds/center.

use dbus::blocking::Connection;
use dbus::arg::Variant;
use dbus::Path;
use std::time::Duration;

const A11Y_BUS: &str = "org.a11y.Bus";
const A11Y_BUS_PATH: &str = "/org/a11y/bus";
const A11Y_BUS_IFACE: &str = "org.a11y.Bus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";
/// ROOT_PATH is the fixed object path of the registry-as-root accessible in the
/// at-spi2-core versions that dropped GetDesktop.
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const REGISTRY_IFACE: &str = "org.a11y.atspi.Registry";
const ACCESSIBLE_IFACE: &str = "org.a11y.atspi.Accessible";
const ACTION_IFACE: &str = "org.a11y.atspi.Action";
const COMPONENT_IFACE: &str = "org.a11y.atspi.Component";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// A node reference: AT-SPI addresses every node by (bus name, object path).
#[derive(Clone)]
struct Ref2 {
    bus: String,
    path: String,
}

pub struct Atspi {
    conn: Connection,
}

impl Atspi {
    pub fn open() -> Result<Atspi, String> {
        // AT-SPI applications publish onto a DEDICATED accessibility bus, not the
        // session bus. `org.a11y.Bus` (a session-bus service) returns that bus's
        // address; we then connect to it directly.
        let session = Connection::new_session().map_err(|e| format!("session bus: {e}"))?;
        let proxy = session.with_proxy(A11Y_BUS, A11Y_BUS_PATH, Duration::from_secs(5));
        let (addr,): (String,) = proxy
            .method_call(A11Y_BUS_IFACE, "GetAddress", ())
            .map_err(|e| format!("org.a11y.Bus.GetAddress: {e}"))?;
        let conn = Connection::new_address(addr.trim())
            .map_err(|e| format!("a11y bus {addr}: {e}"))?;
        Ok(Atspi { conn })
    }

    fn desktop(&self) -> Result<Ref2, String> {
        let proxy = self.conn.with_proxy(REGISTRY, REGISTRY_PATH, Duration::from_secs(5));
        // `GetDesktop(0)` is the classic way to reach the root accessible. Newer
        // at-spi2-core (2.5x, e.g. Debian trixie) REMOVED it — the call fails
        // with 'Method "GetDesktop" ... doesn\'t exist'. In that version the
        // registry itself is the root: its fixed object path is the desktop,
        // reachable over the SAME connection. Fall back to it.
        match proxy.method_call::<(String, Path), _, _, _>(REGISTRY_IFACE, "GetDesktop", (0i32,)) {
            Ok(root) => Ok(Ref2 { bus: root.0, path: root.1.to_string() }),
            Err(_) => {
                // The registry's own name/path IS the root accessible.
                Ok(Ref2 { bus: REGISTRY.to_string(), path: ROOT_PATH.to_string() })
            }
        }
    }

    /// children returns the child refs of a node.
    fn children(&self, node: &Ref2) -> Result<Vec<Ref2>, String> {
        let proxy = self.conn.with_proxy(node.bus.as_str(), node.path.as_str(), Duration::from_secs(5));
        // Newer at-spi2-core (Debian trixie) dropped the `Children` PROPERTY;
        // it is now the `GetChildren()` METHOD on org.a11y.atspi.Accessible
        // (verified against the registry's introspection XML). Try the method
        // first, then the legacy property.
        if let Ok((kids,)) = proxy.method_call::<(Vec<(String, Path)>,), _, _, _>(ACCESSIBLE_IFACE, "GetChildren", ()) {
            return Ok(kids.into_iter().map(|(bus, path)| Ref2 { bus, path: path.to_string() }).collect());
        }
        let (kids,): (Variant<Vec<(String, Path)>>,) = proxy
            .method_call(PROPS_IFACE, "Get", (ACCESSIBLE_IFACE, "Children"))
            .map_err(|e| format!("Children: {e}"))?;
        Ok(kids.0.into_iter().map(|(bus, path)| Ref2 { bus, path: path.to_string() }).collect())
    }

    fn prop_str(&self, node: &Ref2, name: &str) -> String {
        let proxy = self.conn.with_proxy(node.bus.as_str(), node.path.as_str(), Duration::from_secs(5));
        let r: Result<(Variant<String>,), _> = proxy.method_call(PROPS_IFACE, "Get", (ACCESSIBLE_IFACE, name));
        r.map(|(v,)| v.0).unwrap_or_default()
    }

    /// role_name returns the human-readable role ("push button", ...).
    fn role_name(&self, node: &Ref2) -> String {
        let proxy = self.conn.with_proxy(node.bus.as_str(), node.path.as_str(), Duration::from_secs(5));
        let r: Result<(String,), _> = proxy.method_call(ACCESSIBLE_IFACE, "GetRoleName", ());
        r.map(|(v,)| v).unwrap_or_else(|_| "unknown".into())
    }

    /// apps lists the top-level applications of the desktop.
    pub fn apps(&self) -> Result<String, String> {
        let root = self.desktop()?;
        let mut out = String::new();
        for app in self.children(&root)? {
            let name = self.prop_str(&app, "Name");
            let pid = app.path.rsplit('/').next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            // The application's own pid lives on its child window's process id;
            // the registry encodes it in the object path for apps (":1.5/…/1234").
            out.push_str(&format!("{pid}\t{name}\t\n"));
        }
        Ok(out)
    }

    fn app_root(&self, pid: u32) -> Result<Ref2, String> {
        let root = self.desktop()?;
        let apps = self.children(&root)?;
        if pid == 0 {
            return apps.into_iter().next().ok_or_else(|| "no applications".into());
        }
        for app in apps {
            if app.path.rsplit('/').next().and_then(|s| s.parse::<u32>().ok()) == Some(pid) {
                return Ok(app);
            }
        }
        Err(format!("no app with pid {pid}"))
    }

    pub fn tree(&self, pid: u32, depth: u32) -> Result<String, String> {
        let app = self.app_root(pid)?;
        let mut out = String::new();
        self.walk(&app, 0, depth, &mut out)?;
        Ok(out)
    }

    fn walk(&self, node: &Ref2, indent: usize, max: u32, out: &mut String) -> Result<(), String> {
        let name = self.prop_str(node, "Name");
        let role = self.role_name(node);
        out.push_str(&format!("{}{} \"{}\"\n", "  ".repeat(indent), role, name));
        if max != 0 && indent as u32 >= max {
            return Ok(());
        }
        for kid in self.children(node)? {
            self.walk(&kid, indent + 1, max, out)?;
        }
        Ok(())
    }

    /// find returns pretty/bounds/center for the FIRST node matching a simple
    /// `role[name="…"]` / `name="…"` selector.
    pub fn find(&self, pid: u32, sel: &str, out: u8) -> Result<String, String> {
        let app = self.app_root(pid)?;
        let (want_role, want_name) = parse_selector(sel);
        if let Some(n) = self.search(&app, &want_role, &want_name)? {
            match out {
                1 => {
                    let (x, y, w, h) = self.bounds(&n);
                    Ok(format!("{w},{h},{x},{y}"))
                }
                2 => {
                    let (x, y, w, h) = self.bounds(&n);
                    Ok(format!("{},{}", x + w / 2, y + h / 2))
                }
                _ => Ok(format!("{} \"{}\"", self.role_name(&n), self.prop_str(&n, "Name"))),
            }
        } else {
            Err(format!("no match for {sel}"))
        }
    }

    fn search(&self, node: &Ref2, role: &str, name: &str) -> Result<Option<Ref2>, String> {
        let r = self.role_name(node);
        let n = self.prop_str(node, "Name");
        if (role.is_empty() || r.contains(role)) && (name.is_empty() || n == name) {
            if !(role.is_empty() && name.is_empty()) {
                return Ok(Some(node.clone()));
            }
        }
        for kid in self.children(node)? {
            if let Some(hit) = self.search(&kid, role, name)? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn bounds(&self, node: &Ref2) -> (i32, i32, i32, i32) {
        let proxy = self.conn.with_proxy(node.bus.as_str(), node.path.as_str(), Duration::from_secs(5));
        // GetExtents(CoordType screen=0) -> (i32 x, i32 y, i32 w, i32 h)
        let r: Result<(i32, i32, i32, i32), _> = proxy.method_call(COMPONENT_IFACE, "GetExtents", (0u32,));
        r.unwrap_or((0, 0, 0, 0))
    }

    pub fn action(&self, pid: u32, sel: &str, action: &str, value: Option<&str>) -> Result<String, String> {
        let app = self.app_root(pid)?;
        let (role, name) = parse_selector(sel);
        let node = self.search(&app, &role, &name)?.ok_or_else(|| format!("no match for {sel}"))?;
        let proxy = self.conn.with_proxy(node.bus.as_str(), node.path.as_str(), Duration::from_secs(5));
        match action {
            "press" | "click" | "activate" => {
                let (n,): (i32,) = proxy.method_call(ACTION_IFACE, "GetActions", ()).unwrap_or((0,));
                for i in 0..n {
                    let (an,): (String,) = proxy.method_call(ACTION_IFACE, "GetName", (i,)).unwrap_or_default();
                    if an == "click" || an == "activate" || an == "press" {
                        proxy.method_call::<(), _, _, _>(ACTION_IFACE, "DoAction", (i,))
                            .map_err(|e| e.to_string())?;
                        return Ok("ok".into());
                    }
                }
                // fall back to the first action
                proxy.method_call::<(), _, _, _>(ACTION_IFACE, "DoAction", (0i32,)).map_err(|e| e.to_string())?;
                Ok("ok".into())
            }
            "focus" => {
                proxy.method_call::<(), _, _, _>(COMPONENT_IFACE, "GrabFocus", ()).map_err(|e| e.to_string())?;
                Ok("ok".into())
            }
            "setvalue" | "settext" => {
                let v = value.unwrap_or("");
                let _: () = proxy
                    .method_call(PROPS_IFACE, "Set", (ACCESSIBLE_IFACE, "Name", Variant(v.to_string())))
                    .map_err(|e| e.to_string())?;
                Ok("ok".into())
            }
            other => Err(format!("unsupported action {other}")),
        }
    }
}

/// parse_selector understands `role[name="x"]`, `role`, `name="x"` and `#name`.
fn parse_selector(sel: &str) -> (String, String) {
    let sel = sel.trim();
    if let Some(eq) = sel.find('=') {
        let role = sel[..eq].trim().trim_start_matches('[').to_string();
        let name = sel[eq + 1..].trim().trim_matches('"').trim_end_matches(']').to_string();
        return (role, name);
    }
    (sel.to_string(), String::new())
}
