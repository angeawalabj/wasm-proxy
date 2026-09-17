//! Équivalent Rust du plugin ABI v2 (voir admin_guard.c pour la version C).
//! Compilation :
//!
//!   rustup target add wasm32-unknown-unknown
//!   rustc --target wasm32-unknown-unknown -O --crate-type=cdylib \
//!         -o admin_guard_rust.wasm example_plugin_v2.rs
//!
//! Sans allocateur global ni serde (no_std + no libc), le JSON est construit
//! et lu "à la main" avec des opérations sur slices de bytes — même logique
//! que la version C, juste plus sûre grâce aux bounds-checks de Rust.
//! Pour un vrai projet, une crate comme `serde-json-core` (compatible no_std,
//! sans allocation) simplifierait grandement ce code.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

static mut IN_BUF: [u8; 4096] = [0; 4096];
static mut OUT_BUF: [u8; 512] = [0; 512];

/// Cherche `needle` dans `hay`. Retourne l'index de début du match, ou None.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Extrait la valeur associée à une clé JSON simple `"key":"value"`.
/// Ne gère pas l'échappement JSON (l'hôte contrôle l'encodage en amont).
fn extract_json_string<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let key_pos = find(buf, key)?;
    let after_key = &buf[key_pos + key.len()..];
    let quote_start = after_key.iter().position(|&b| b == b'"')?;
    let rest = &after_key[quote_start + 1..];
    let quote_end = rest.iter().position(|&b| b == b'"')?;
    Some(&rest[..quote_end])
}

#[no_mangle]
pub extern "C" fn alloc(_len: i32) -> i32 {
    unsafe { IN_BUF.as_ptr() as i32 }
}

#[no_mangle]
pub extern "C" fn filter_request(ptr: i32, len: i32) -> i64 {
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };

    let path = extract_json_string(input, b"\"path\":");
    let is_admin = path == Some(b"/admin");

    let out: &[u8] = if is_admin {
        let api_key = extract_json_string(input, b"\"x-api-key\":");
        if api_key == Some(b"secret123") {
            b"{\"action\":\"continue\",\"add_headers\":{\"x-plugin-checked\":\"admin_guard\"}}"
        } else {
            b"{\"action\":\"block\",\"status\":403,\"body\":\"missing or invalid api key\"}"
        }
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
