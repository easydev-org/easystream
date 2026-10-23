//! libx264 (CPU) H.264 encoder via the tiny C shim. Accepts BGRA frames and
//! emits Annex-B access units. This is the default path (no GPU needed); VAAPI
//! zero-copy is a follow-up.
use std::ffi::c_void;

extern "C" {
    fn na_x264_open(w: libc::c_int, h: libc::c_int, fps: libc::c_int, bitrate: libc::c_int) -> *mut c_void;
    fn na_x264_encode_scaled(
        enc: *mut c_void,
        bgra: *const u8,
        sw: libc::c_int,
        sh: libc::c_int,
        out: *mut *mut u8,
        outlen: *mut libc::c_int,
    ) -> libc::c_int;
    fn na_x264_encode(
        enc: *mut c_void,
        bgra: *const u8,
        out: *mut *mut u8,
        outlen: *mut libc::c_int,
    ) -> libc::c_int;
    fn na_x264_close(enc: *mut c_void);
    fn na_x264_force_idr(enc: *mut c_void);
}

pub struct X264 {
    enc: *mut c_void,
}

// The encoder is used from a single task.
unsafe impl Send for X264 {}

impl X264 {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<X264, String> {
        unsafe {
            let enc = na_x264_open(
                width as i32,
                height as i32,
                fps as i32,
                bitrate as i32,
            );
            if enc.is_null() {
                return Err("x264 open failed".into());
            }
            Ok(X264 { enc })
        }
    }

    /// encode one BGRA frame; returns the Annex-B access unit (may be empty).
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<u8>, String> {
        self.encode_ptr(bgra.as_ptr(), bgra.len() as u32)
    }

    /// encode_ptr encodes a BGRA frame given a raw pointer + byte length. Lets
    /// the caller pass a pointer straight into the capture buffer (no Vec copy).
    /// Encode a frame whose captured geometry differs from the encoder's.
    ///
    /// libyuv scales and converts in one pass on the C side. When the sizes
    /// match this forwards to the plain path, so a caller can always use it
    /// without paying for a comparison-then-branch here.
    pub fn encode_ptr_scaled(
        &mut self,
        bgra: *const u8,
        len: u32,
        sw: u32,
        sh: u32,
    ) -> Result<Vec<u8>, String> {
        unsafe {
            let _ = len;
            let mut out: *mut u8 = std::ptr::null_mut();
            let mut outlen: libc::c_int = 0;
            let rc = na_x264_encode_scaled(
                self.enc,
                bgra,
                sw as libc::c_int,
                sh as libc::c_int,
                &mut out,
                &mut outlen,
            );
            if rc < 0 {
                return Err("x264 encode (scaled) failed".into());
            }
            if out.is_null() || outlen <= 0 {
                return Ok(Vec::new());
            }
            let v = std::slice::from_raw_parts(out, outlen as usize).to_vec();
            libc::free(out as *mut c_void);
            Ok(v)
        }
    }

    pub fn encode_ptr(&mut self, bgra: *const u8, len: u32) -> Result<Vec<u8>, String> {
        unsafe {
            let _ = len;
            let mut out: *mut u8 = std::ptr::null_mut();
            let mut outlen: libc::c_int = 0;
            let rc = na_x264_encode(self.enc, bgra, &mut out, &mut outlen);
            if rc < 0 {
                return Err("x264 encode failed".into());
            }
            if out.is_null() || outlen <= 0 {
                return Ok(Vec::new());
            }
            let v = std::slice::from_raw_parts(out, outlen as usize).to_vec();
            libc::free(out as *mut c_void);
            Ok(v)
        }
    }

    /// force_idr makes the next encoded frame a keyframe (for late joiners).
    pub fn force_idr(&mut self) {
        unsafe { na_x264_force_idr(self.enc) }
    }
}

impl Drop for X264 {
    fn drop(&mut self) {
        unsafe { na_x264_close(self.enc) }
    }
}
