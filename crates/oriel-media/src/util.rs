use std::ffi::{CStr, CString, c_int};

use anyhow::{Result, anyhow};

use crate::ff;

pub(crate) fn err_str(code: c_int) -> String {
    let mut buf = [0 as std::ffi::c_char; 256];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

pub(crate) fn check(code: c_int, what: &str) -> Result<c_int> {
    if code < 0 {
        Err(anyhow!("{what}: {}", err_str(code)))
    } else {
        Ok(code)
    }
}

pub(crate) const fn averror(e: c_int) -> c_int {
    -e
}

pub(crate) fn is_eagain(code: c_int) -> bool {
    code == averror(ff::EAGAIN)
}

pub(crate) const AVERROR_EOF: c_int =
    -(((b'E' as u32) | ((b'O' as u32) << 8) | ((b'F' as u32) << 16) | ((b' ' as u32) << 24))
        as c_int);

/// An owned `AVDictionary` of options.
pub(crate) struct Dict(pub *mut ff::AVDictionary);

impl Dict {
    pub fn new() -> Self {
        Self(std::ptr::null_mut())
    }

    pub fn set(&mut self, key: &str, value: impl ToString) {
        let k = CString::new(key).unwrap();
        let v = CString::new(value.to_string()).unwrap();
        unsafe { ff::av_dict_set(&mut self.0, k.as_ptr(), v.as_ptr(), 0) };
    }

    /// Keys FFmpeg did not consume.
    pub fn leftover(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut entry: *const ff::AVDictionaryEntry = std::ptr::null();
        loop {
            entry = unsafe { ff::av_dict_iterate(self.0, entry) };
            if entry.is_null() {
                break;
            }
            out.push(
                unsafe { CStr::from_ptr((*entry).key) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        out
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        unsafe { ff::av_dict_free(&mut self.0) };
    }
}
