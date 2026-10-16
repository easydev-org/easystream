//! Wayland capture via the wlr-screencopy shim (wlshim.c). Pure protocol.
use std::ffi::c_void;


extern "C" {
    fn na_wl_open() -> *mut c_void;
    fn na_wl_grab(h: *mut c_void, out: *mut *mut u8, w: *mut libc::c_int, hh: *mut libc::c_int) -> libc::c_int;
    fn na_wl_close(h: *mut c_void);
    // input
    fn na_wlin_open() -> *mut c_void;
    fn na_wlin_move(h: *mut c_void, x: libc::c_int, y: libc::c_int);
    fn na_wlin_button(h: *mut c_void, button: libc::c_int, down: libc::c_int);
    fn na_wlin_axis(h: *mut c_void, dx: libc::c_int, dy: libc::c_int);
    fn na_wlin_key(h: *mut c_void, keycode: u32, down: libc::c_int);
    fn na_wlin_mods(h: *mut c_void, depressed: u32, latched: u32, locked: u32, group: u32);
    fn na_wlin_close(h: *mut c_void);
    fn na_wlin_set_size(h: *mut c_void, w: libc::c_int, hh: libc::c_int);
}

/// WlInput is the Wayland input backend (wlr-virtual-pointer/keyboard).
pub struct WlInput {
    handle: *mut c_void,
    /// evdev keycodes of the modifier keys currently held (ctrl/shift/alt/meta).
    /// wlroots virtual-keyboard needs these as the `depressed` bitmask on every
    /// modifiers event, or Ctrl+C / Ctrl+D etc. are seen as plain keys.
    mods: std::sync::Mutex<std::collections::HashSet<u32>>,
}

unsafe impl Send for WlInput {}

impl WlInput {
    pub fn open() -> Result<WlInput, String> {
        unsafe {
            let h = na_wlin_open();
            if h.is_null() {
                return Err("wayland input unavailable".into());
            }
            Ok(WlInput { handle: h, mods: std::sync::Mutex::new(std::collections::HashSet::new()) })
        }
    }
    pub fn move_to(&self, x: i16, y: i16) {
        unsafe { na_wlin_move(self.handle, x as i32, y as i32) }
    }
    /// set_output_size tells the backend the extent absolute motion is
    /// normalised against. Without it the shim falls back to a guessed
    /// 1920x1080 and the cursor lands in the wrong place on other screens.
    pub fn set_output_size(&self, w: u32, h: u32) {
        unsafe { na_wlin_set_size(self.handle, w as i32, h as i32) }
    }
    pub fn button(&self, num: u8, down: bool) {
        unsafe { na_wlin_button(self.handle, num as i32, down as i32) }
    }
    pub fn scroll(&self, dx: i32, dy: i32) {
        unsafe { na_wlin_axis(self.handle, dx, dy) }
    }
    pub fn key(&self, keycode: u8, down: bool) {
        // wlroots virtual-keyboard wants evdev keycodes: evdev = X11 keycode - 8.
        let ev = keycode.saturating_sub(8) as u32;
        // Track modifier state and push it to the compositor BEFORE the key, so
        // Ctrl+C is delivered as (ctrl held + c) rather than a bare `c`.
        // evdev keycode -> XKB modifier index bit (the `depressed` mask the
        // compositor expects is keyed on the modifier INDEX, not the evdev code).
        //   Shift=0, CapsLock=1, Ctrl=2, Mod1(Alt)=3, NumLock=4, Mod3=5, Mod4(Meta)=6
        let xkb_bit = match ev {
            42 | 54 => Some(0u32),  // LEFTSHIFT / RIGHTSHIFT
            29 | 97 => Some(2),      // LEFTCTRL / RIGHTCTRL
            56 | 100 => Some(3),     // LEFTALT / RIGHTALT (Mod1)
            125 | 126 => Some(6),    // LEFTMETA / RIGHTMETA (Mod4)
            _ => None,
        };
        if let Some(bit) = xkb_bit {
            let mut m = self.mods.lock().unwrap();
            if down { m.insert(bit); } else { m.remove(&bit); }
            let mut mask: u32 = 0;
            for &b in m.iter() {
                mask |= 1u32 << b;
            }
            unsafe { na_wlin_mods(self.handle, mask, 0, 0, 0) };
        }
        unsafe { na_wlin_key(self.handle, ev, down as i32) }
    }
}

impl Drop for WlInput {
    fn drop(&mut self) {
        unsafe { na_wlin_close(self.handle) }
    }
}

pub struct Wayland {
    handle: *mut c_void,
}

unsafe impl Send for Wayland {}

impl Wayland {
    pub fn open() -> Result<Wayland, String> {
        unsafe {
            let handle = na_wl_open();
            if handle.is_null() {
                return Err("wl_display_connect / screencopy failed".into());
            }
            Ok(Wayland { handle })
        }
    }

    /// grab returns the newest frame as top-down BGRA pixels (stride = w*4).
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        unsafe {
            let mut out: *mut u8 = std::ptr::null_mut();
            let (mut w, mut h) = (0i32, 0i32);
            if na_wl_grab(self.handle, &mut out, &mut w, &mut h) != 0 {
                return Err("screencopy failed".into());
            }
            let len = (w as usize) * (h as usize) * 4;
            let px = std::slice::from_raw_parts(out, len).to_vec();
            Ok(px)
        }
    }

    /// grab_ptr returns a pointer into the shim's REUSED shm mapping (valid until
    /// the next grab), so the pump can compare/encode without copying 8 MB per
    /// frame — the same zero-copy path X11 uses.
    pub fn grab_shm_ptr(&self) -> Option<(*const u8, u32, u32)> {
        unsafe {
            let mut out: *mut u8 = std::ptr::null_mut();
            let (mut w, mut h) = (0i32, 0i32);
            if na_wl_grab(self.handle, &mut out, &mut w, &mut h) != 0 || out.is_null() {
                return None;
            }
            Some((out as *const u8, w as u32, h as u32))
        }
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        unsafe { na_wl_close(self.handle) }
    }
}
