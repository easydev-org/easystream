//! X11 capture + input via a tiny libxcb shim (x11shim.c). X11 is a pure
//! protocol (libxcb is just its codec); the shim uses the real xcb headers, so
//! Rust never hand-rolls xcb struct layouts.
use std::ffi::c_void;


extern "C" {
    fn na_x11_open() -> *mut c_void;
    fn na_x11_size(h: *mut c_void, w: *mut libc::c_int, hh: *mut libc::c_int);
    fn na_x11_grab(
        h: *mut c_void,
        out: *mut *mut u8,
        w: *mut libc::c_int,
        hh: *mut libc::c_int,
        stride: *mut libc::c_int,
    ) -> libc::c_int;
    fn na_x11_move(h: *mut c_void, x: libc::c_int, y: libc::c_int);
    fn na_x11_button(h: *mut c_void, num: libc::c_int, down: libc::c_int);
    fn na_x11_key(h: *mut c_void, keycode: libc::c_int, down: libc::c_int);
    fn na_x11_close(h: *mut c_void);
    fn na_x11_grab_shm(h: *mut c_void, out: *mut *mut u8, w: *mut libc::c_int, hh: *mut libc::c_int) -> libc::c_int;
    fn na_x11_grab_shm_ptr(h: *mut c_void, w: *mut libc::c_int, hh: *mut libc::c_int) -> *const u8;
}

pub struct X11 {
    handle: *mut c_void,
    width: u32,
    height: u32,
}

unsafe impl Send for X11 {}

impl X11 {
    pub fn open() -> Result<X11, String> {
        unsafe {
            let handle = na_x11_open();
            if handle.is_null() {
                return Err("xcb_connect failed".into());
            }
            let (mut w, mut h) = (0i32, 0i32);
            na_x11_size(handle, &mut w, &mut h);
            Ok(X11 {
                handle,
                width: w as u32,
                height: h as u32,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// grab returns the newest frame as top-down BGRA pixels (stride = w*4).
    pub fn grab(&self) -> Result<Vec<u8>, String> {
        // Prefer XSHM (shared memory: no per-frame X socket copy); fall back to
        // XGetImage when XSHM is unavailable.
        unsafe {
            let mut out: *mut u8 = std::ptr::null_mut();
            let (mut w, mut h) = (0i32, 0i32);
            if na_x11_grab_shm(self.handle, &mut out, &mut w, &mut h) == 0 {
                let len = (w as usize) * (h as usize) * 4;
                let px = std::slice::from_raw_parts(out, len).to_vec();
                libc::free(out as *mut c_void);
                return Ok(px);
            }
        }
        unsafe {
            let mut out: *mut u8 = std::ptr::null_mut();
            let (mut w, mut h, mut stride) = (0i32, 0i32, 0i32);
            if na_x11_grab(self.handle, &mut out, &mut w, &mut h, &mut stride) != 0 {
                return Err("XGetImage failed".into());
            }
            let len = (w as usize) * (h as usize) * 4;
            let px = std::slice::from_raw_parts(out, len).to_vec();
            libc::free(out as *mut c_void);
            Ok(px)
        }
    }

    /// grab_shm_ptr returns a pointer into the XSHM segment (zero-copy). Valid
    /// until the next grab. None when XSHM is unavailable.
    pub fn grab_shm_ptr(&self) -> Option<(*const u8, u32, u32)> {
        unsafe {
            let (mut w, mut h) = (0i32, 0i32);
            let p = na_x11_grab_shm_ptr(self.handle, &mut w, &mut h);
            if p.is_null() {
                None
            } else {
                Some((p, w as u32, h as u32))
            }
        }
    }

    pub fn move_to(&self, x: i16, y: i16) {
        unsafe { na_x11_move(self.handle, x as i32, y as i32) }
    }

    pub fn button(&self, num: u8, down: bool) {
        unsafe { na_x11_button(self.handle, num as i32, down as i32) }
    }

    pub fn key(&self, keycode: u8, down: bool) {
        unsafe { na_x11_key(self.handle, keycode as i32, down as i32) }
    }


}

impl Drop for X11 {
    fn drop(&mut self) {
        unsafe { na_x11_close(self.handle) }
    }
}

pub fn key_by_name(name: &str) -> Option<u8> {
    // Normalise (case-insensitive, ignore '_'/spaces) so every spelling of a
    // key resolves: the frontend sends "Backspace"/"PageUp"/"Shift"/"Control",
    // older callers used "BackSpace"/"Page_Up"/"Shift_L".
    let n = name.to_lowercase().replace(['_', ' '], "");
    match n.as_str() {
        "return" | "enter" => Some(36),
        "escape" | "esc" => Some(9),
        "tab" => Some(23),
        "backspace" => Some(22),
        "space" => Some(65),
        "shift" => Some(50),
        "control" | "ctrl" => Some(37),
        "alt" => Some(64),
        "meta" | "super" => Some(133),
        "delete" | "del" => Some(119),
        "left" => Some(113),
        "right" => Some(114),
        "up" => Some(111),
        "down" => Some(116),
        "home" => Some(110),
        "end" => Some(115),
        "pageup" => Some(112),
        "pagedown" => Some(117),
        "insert" | "ins" => Some(118),
        "f1" => Some(67), "f2" => Some(68), "f3" => Some(69), "f4" => Some(70),
        "f5" => Some(71), "f6" => Some(72), "f7" => Some(73), "f8" => Some(74),
        "f9" => Some(75), "f10" => Some(76), "f11" => Some(95), "f12" => Some(96),
        _ => None,
    }
}

/// char_keycode maps a character to the X11 keycode + whether Shift is needed
/// (US layout). The X11 letter keycodes are NOT sequential: they follow the
/// physical rows (a=38 s=39 d=40 ... q=24 w=25 ... z=52 x=53 ...).
pub fn char_keycode(ch: char) -> Option<(u8, bool)> {
    let c = ch as u32;
    if c > 0x7f {
        return None; // non-ASCII needs an input method
    }
    let b = c as u8;
    let plain = |k: u8| Some((k, false));
    let shift = |k: u8| Some((k, true));
    match b {
        // letters: physical rows
        b'a' => plain(38), b'b' => plain(56), b'c' => plain(54), b'd' => plain(40),
        b'e' => plain(26), b'f' => plain(41), b'g' => plain(42), b'h' => plain(43),
        b'i' => plain(31), b'j' => plain(44), b'k' => plain(45), b'l' => plain(46),
        b'm' => plain(58), b'n' => plain(57), b'o' => plain(32), b'p' => plain(33),
        b'q' => plain(24), b'r' => plain(27), b's' => plain(39), b't' => plain(28),
        b'u' => plain(30), b'v' => plain(55), b'w' => plain(25), b'x' => plain(53),
        b'y' => plain(29), b'z' => plain(52),
        b'A' => shift(38), b'B' => shift(56), b'C' => shift(54), b'D' => shift(40),
        b'E' => shift(26), b'F' => shift(41), b'G' => shift(42), b'H' => shift(43),
        b'I' => shift(31), b'J' => shift(44), b'K' => shift(45), b'L' => shift(46),
        b'M' => shift(58), b'N' => shift(57), b'O' => shift(32), b'P' => shift(33),
        b'Q' => shift(24), b'R' => shift(27), b'S' => shift(39), b'T' => shift(28),
        b'U' => shift(30), b'V' => shift(55), b'W' => shift(25), b'X' => shift(53),
        b'Y' => shift(29), b'Z' => shift(52),
        // digits + shifted symbols (X11: 1=10, 2=11, ... 9=18, 0=19)
        b'1' => plain(10), b'2' => plain(11), b'3' => plain(12), b'4' => plain(13),
        b'5' => plain(14), b'6' => plain(15), b'7' => plain(16), b'8' => plain(17),
        b'9' => plain(18), b'0' => plain(19),
        b')' => shift(11), b'!' => shift(10), b'@' => shift(11), b'#' => shift(12),
        b'$' => shift(13), b'%' => shift(14), b'^' => shift(15), b'&' => shift(16),
        b'*' => shift(17), b'(' => shift(18),
        // punctuation
        b'-' => plain(20), b'_' => shift(20),
        b'=' => plain(21), b'+' => shift(21),
        b'[' => plain(34), b'{' => shift(34),
        b']' => plain(35), b'}' => shift(35),
        b';' => plain(47), b':' => shift(47),
        b'\'' => plain(48), b'"' => shift(48),
        b'`' => plain(49), b'~' => shift(49),
        b'\\' => plain(51), b'|' => shift(51),
        b',' => plain(59), b'<' => shift(59),
        b'.' => plain(60), b'>' => shift(60),
        b'/' => plain(61), b'?' => shift(61),
        b' ' => plain(65),
        b'\n' | b'\r' => plain(36),
        b'\t' => plain(23),
        _ => None,
    }
}

