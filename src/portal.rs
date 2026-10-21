//! Screen capture through the xdg-desktop-portal ScreenCast API + PipeWire.
//!
//! This is the capture path for compositors that do not implement the
//! wlroots-only `zwlr_screencopy_manager_v1` (kwin, mutter/GNOME). The heavy
//! lifting (D-Bus negotiation, PipeWire stream, BGRA copy) lives in
//! `portalshim.c`; this is the Rust side of that FFI.
use std::ffi::c_void;

extern "C" {
    fn na_portal_open() -> *mut c_void;
    fn na_portal_start(h: *mut c_void) -> libc::c_int;
    fn na_portal_grab(
        h: *mut c_void,
        out: *mut *mut u8,
        w: *mut libc::c_int,
        hh: *mut libc::c_int,
    ) -> libc::c_int;
    fn na_portal_size(h: *mut c_void, w: *mut libc::c_int, hh: *mut libc::c_int);
    fn na_portal_close(h: *mut c_void);

    // RemoteDesktop input. GNOME Shell and KDE Plasma have no wlroots virtual
    // input protocols, so this is the only way to move their pointer — the same
    // portal session that carries the video stream also injects events.
    fn na_portal_rd_ok(h: *mut c_void) -> libc::c_int;
    fn na_portal_rd_move(h: *mut c_void, x: f64, y: f64) -> libc::c_int;
    fn na_portal_rd_button(h: *mut c_void, button: libc::c_int, down: libc::c_int) -> libc::c_int;
    fn na_portal_rd_axis(h: *mut c_void, dx: f64, dy: f64) -> libc::c_int;
    fn na_portal_rd_key(h: *mut c_void, keycode: libc::c_int, down: libc::c_int) -> libc::c_int;
}

/// Portal is the portal/PipeWire capture backend.
pub struct Portal {
    handle: *mut c_void,
}

unsafe impl Send for Portal {}
unsafe impl Sync for Portal {}

impl Portal {
    pub fn open() -> Result<Portal, String> {
        unsafe {
            let h = na_portal_open();
            if h.is_null() {
                return Err("portal ScreenCast unavailable".into());
            }
            // Start the PipeWire stream immediately; frames arrive asynchronously.
            if na_portal_start(h) != 0 {
                na_portal_close(h);
                return Err("portal PipeWire stream failed to start".into());
            }
            Ok(Portal { handle: h })
        }
    }

    /// size reports the negotiated monitor size.
    pub fn size(&self) -> (u32, u32) {
        let (mut w, mut h) = (0i32, 0i32);
        unsafe { na_portal_size(self.handle, &mut w, &mut h) };
        (w.max(0) as u32, h.max(0) as u32)
    }

    /// grab returns the newest frame as top-down BGRA.
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        let (mut w, mut h) = (0i32, 0i32);
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let ok = unsafe { na_portal_grab(self.handle, &mut ptr, &mut w, &mut h) };
        if ok == 0 || ptr.is_null() {
            return Err("portal: no frame yet".into());
        }
        let len = (w as usize) * (h as usize) * 4;
        let v = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
        unsafe { libc::free(ptr as *mut c_void) };
        Ok(v)
    }

    /// input_ok reports whether the portal granted pointer + keyboard injection.
    ///
    /// False means SelectDevices was refused — an old xdg-desktop-portal, or a
    /// backend without the RemoteDesktop interface. Callers should fall back
    /// rather than sending events that go nowhere.
    pub fn input_ok(&self) -> bool {
        unsafe { na_portal_rd_ok(self.handle) == 1 }
    }

    /// move_to places the pointer at absolute (x, y) in the captured stream's
    /// coordinate space, which is the same width/height size() reports.
    pub fn move_to(&self, x: f64, y: f64) -> bool {
        unsafe { na_portal_rd_move(self.handle, x, y) == 0 }
    }

    /// button presses (down=true) or releases an evdev button code.
    pub fn button(&self, code: i32, down: bool) -> bool {
        unsafe { na_portal_rd_button(self.handle, code, down as libc::c_int) == 0 }
    }

    /// axis scrolls by (dx, dy) in smooth-scroll units.
    pub fn axis(&self, dx: f64, dy: f64) -> bool {
        unsafe { na_portal_rd_axis(self.handle, dx, dy) == 0 }
    }

    /// key presses (down=true) or releases an evdev keycode.
    pub fn key(&self, code: i32, down: bool) -> bool {
        unsafe { na_portal_rd_key(self.handle, code, down as libc::c_int) == 0 }
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        unsafe { na_portal_close(self.handle) };
    }
}
