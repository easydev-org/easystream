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
    fn na_portal_grab(h: *mut c_void, out: *mut *mut u8, w: *mut libc::c_int, hh: *mut libc::c_int) -> libc::c_int;
    fn na_portal_size(h: *mut c_void, w: *mut libc::c_int, hh: *mut libc::c_int);
    fn na_portal_close(h: *mut c_void);
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
}

impl Drop for Portal {
    fn drop(&mut self) {
        unsafe { na_portal_close(self.handle) };
    }
}
