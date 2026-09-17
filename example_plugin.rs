//! Exemple de plugin écrit en Rust, respectant l'ABI v1 du proxy.
//!
//! Compilation (nécessite `rustup target add wasm32-unknown-unknown`) :
//!
//!   rustc --target wasm32-unknown-unknown -O --crate-type=cdylib \
//!         -o admin_blocker_rust.wasm example_plugin.rs
//!
//! Ou en tant que crate séparé avec Cargo.toml :
//!   [lib]
//!   crate-type = ["cdylib"]
//!   puis `cargo build --release --target wasm32-unknown-unknown`
//!
//! Contrairement au module écrit à la main en Python (build_admin_blocker.py),
//! ceci est du vrai Rust : plus lisible, plus sûr (bounds-checking sur le
//! slice), et bien plus simple à faire évoluer vers une logique complexe
//! (rate limiting, vérif JWT, etc.) que d'assembler du binaire wasm à la main.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// Allocateur "bump" ultra-naïf : on ne libère jamais la mémoire. C'est
// acceptable ici car chaque appel `filter_request` tourne dans une instance
// wasm fraîche (voir plugin.rs côté hôte), donc la mémoire est de toute façon
// jetée après chaque requête.
static mut BUMP_PTR: usize = 1024;
const BUFFER: [u8; 65536] = [0; 65536]; // réservé statiquement dans le module

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    unsafe {
        let ptr = BUMP_PTR;
        BUMP_PTR += len as usize;
        ptr as i32
    }
}

#[no_mangle]
pub extern "C" fn filter_request(ptr: i32, len: i32) -> i32 {
    let slice = unsafe {
        core::slice::from_raw_parts(ptr as *const u8, len as usize)
    };

    // Même règle que la version assemblée à la main : bloquer uniquement le
    // chemin exact "/admin". Facile à étendre : liste de préfixes interdits,
    // détection de motifs, etc. — tout ce que la logique Rust permet.
    if slice == b"/admin" {
        1
    } else {
        0
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

// Empêche le compilateur d'éliminer BUFFER en le "touchant" quelque part.
#[no_mangle]
pub extern "C" fn _keep_buffer() -> *const u8 {
    unsafe { BUFFER.as_ptr() }
}
