//! C ABI for the emscripten build, driven by `docs/index.html`.
//! Call `s2p_add_font` for each font, then `s2p_convert`; read the result with the getters.
// The prebuilt Skia uses Emscripten's JS setjmp/longjmp; this provides it inside the module.
#[cfg(target_os = "emscripten")]
mod sjlj;

use std::cell::RefCell;

thread_local! {
    static FONTS: RefCell<Vec<Vec<u8>>> = RefCell::default();
    /// The PDF on success, the error message on failure.
    static OUT: RefCell<Vec<u8>> = RefCell::default();
    /// Warnings, one per line.
    static WARN: RefCell<Vec<u8>> = RefCell::default();
}

unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(ptr, len) } }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn s2p_add_font(ptr: *const u8, len: usize) {
    let font = unsafe { bytes(ptr, len) }.to_vec();
    FONTS.with_borrow_mut(|f| f.push(font));
}

/// Returns 0 on success, 1 on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn s2p_convert(ptr: *const u8, len: usize) -> i32 {
    let sketch = unsafe { bytes(ptr, len) };
    let res = FONTS.with_borrow(|f| sketch2pdf::convert_with_fonts(sketch, f));
    let (out, warn, code) = match res {
        Ok(o) => (o.pdf, o.warnings.join("\n"), 0),
        Err(e) => (e.to_string().into_bytes(), String::new(), 1),
    };
    OUT.set(out);
    WARN.set(warn.into_bytes());
    code
}

#[unsafe(no_mangle)]
pub extern "C" fn s2p_out_ptr() -> *const u8 { OUT.with_borrow(|v| v.as_ptr()) }
#[unsafe(no_mangle)]
pub extern "C" fn s2p_out_len() -> usize { OUT.with_borrow(Vec::len) }
#[unsafe(no_mangle)]
pub extern "C" fn s2p_warn_ptr() -> *const u8 { WARN.with_borrow(|v| v.as_ptr()) }
#[unsafe(no_mangle)]
pub extern "C" fn s2p_warn_len() -> usize { WARN.with_borrow(Vec::len) }

fn main() {}

