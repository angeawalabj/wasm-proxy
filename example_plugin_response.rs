//! Équivalent Rust du plugin error_masker.c — hook filter_response
//! uniquement (aucun filter_request exporté : hook symétrique et optionnel).
//!
//! Compilation : rustup target add wasm32-unknown-unknown
//!   rustc --target wasm32-unknown-unknown -O --crate-type=cdylib \
//!         -o error_masker_rust.wasm example_plugin_response.rs

#![no_std]
#![no_main]

use core::panic::PanicInfo;

static mut IN_BUF: [u8; 2048] = [0; 2048];
static mut OUT_BUF: [u8; 256] = [0; 256];

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Extrait un entier positionné juste après la clé (ex: `"status":503`).
fn extract_json_int(buf: &[u8], key: &[u8]) -> Option<i32> {
    let key_pos = find(buf, key)?;
    let mut rest = &buf[key_pos + key.len()..];
    let mut value = 0i32;
    let mut any_digit = false;
    while let Some(&b) = rest.first() {
        if !b.is_ascii_digit() {
            break;
        }
        value = value * 10 + (b - b'0') as i32;
        any_digit = true;
        rest = &rest[1..];
    }
    any_digit.then_some(value)
}

#[no_mangle]
pub extern "C" fn alloc(_len: i32) -> i32 {
    unsafe { IN_BUF.as_ptr() as i32 }
}

#[no_mangle]
pub extern "C" fn filter_response(ptr: i32, len: i32) -> i64 {
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let status = extract_json_int(input, b"\"status\":").unwrap_or(0);

    let out: &[u8] = if status >= 500 {
        b"{\"action\":\"block\",\"status\":502,\"body\":\"upstream error\"}"
    } else {
        b"{\"action\":\"continue\"}"
    };

    unsafe {
        OUT_BUF[..out.len()].copy_from_slice(out);
        let out_ptr = OUT_BUF.as_ptr() as u64;
        ((out_ptr << 32) | (out.len() as u64)) as i64
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
