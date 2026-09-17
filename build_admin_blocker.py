"""
Génère à la main un module WebAssembly binaire implémentant l'ABI du proxy :

  memory (export "memory")
  alloc(len: i32) -> i32           : bump allocator naïf
  filter_request(ptr: i32, len: i32) -> i32
        retourne 1 si le chemin lu en mémoire vaut exactement "/admin", sinon 0

Pas de dépendance à rustc/wasm32 ni à un assembleur wat->wasm : on écrit
directement le format binaire WASM (sections + LEB128), sans passer par un
toolchain wasm32. En pratique, ce module serait plutôt écrit en Rust et
compilé avec `cargo build --target wasm32-unknown-unknown` (voir
example_plugin.rs) — celui-ci reste comme exercice sur le format binaire.
"""

import struct


def uleb128(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def sleb128(value: int) -> bytes:
    out = bytearray()
    more = True
    while more:
        byte = value & 0x7F
        value >>= 7
        if (value == 0 and not (byte & 0x40)) or (value == -1 and (byte & 0x40)):
            more = False
        else:
            byte |= 0x80
        out.append(byte)
    return bytes(out)


def vec(items: bytes, count: int) -> bytes:
    return uleb128(count) + items


def section(section_id: int, payload: bytes) -> bytes:
    return bytes([section_id]) + uleb128(len(payload)) + payload


MAGIC = b"\x00asm"
VERSION = b"\x01\x00\x00\x00"

# --- Section Type (id 1) ---
# type0 : (i32) -> (i32)                 pour alloc
# type1 : (i32, i32) -> (i32)            pour filter_request
type0 = b"\x60" + vec(b"\x7f", 1) + vec(b"\x7f", 1)  # func (i32)->(i32)
type1 = b"\x60" + vec(b"\x7f\x7f", 2) + vec(b"\x7f", 1)  # func (i32,i32)->(i32)
type_section = section(1, vec(type0 + type1, 2))

# --- Section Function (id 3) : indices de type pour chaque fonction déclarée ---
function_section = section(3, vec(uleb128(0) + uleb128(1), 2))

# --- Section Memory (id 5) : 1 mémoire, min 1 page (64KiB), pas de max ---
memory_section = section(5, vec(b"\x00" + uleb128(1), 1))

# --- Section Global (id 6) : global i32 mutable, init = 1024 (zone libre après nos constantes) ---
global_init = b"\x41" + sleb128(1024) + b"\x0b"  # i32.const 1024 ; end
global_section = section(6, vec(b"\x7f\x01" + global_init, 1))

# --- Section Export (id 7) ---
def export_entry(name: bytes, kind: int, index: int) -> bytes:
    return uleb128(len(name)) + name + bytes([kind]) + uleb128(index)

exports = (
    export_entry(b"memory", 0x02, 0)
    + export_entry(b"alloc", 0x00, 0)
    + export_entry(b"filter_request", 0x00, 1)
)
export_section = section(7, vec(exports, 3))

# --- Section Code (id 10) ---

def func_body(local_decls: bytes, instrs: bytes) -> bytes:
    body = local_decls + instrs + b"\x0b"  # end
    return uleb128(len(body)) + body

# alloc(len) -> i32
#   global.get 0        ; ancien pointeur (valeur de retour)
#   global.get 0
#   local.get 0
#   i32.add
#   global.set 0
alloc_body = func_body(
    uleb128(0),  # 0 groupes de locals
    b"\x23\x00"          # global.get 0
    b"\x23\x00"          # global.get 0
    b"\x20\x00"          # local.get 0
    b"\x6a"               # i32.add
    b"\x24\x00",         # global.set 0
)

# filter_request(ptr, len) -> i32
#   local.get 1          ; len
#   i32.const 6
#   i32.eq
#   local.get 0          ; ptr
#   i32.load  align=0 offset=0
#   i32.const 0x6D64612F   ; "/adm" en little-endian
#   i32.eq
#   i32.and
#   local.get 0
#   i32.load16_u align=0 offset=4
#   i32.const 0x6E69        ; "in" en little-endian
#   i32.eq
#   i32.and
MAGIC_ADM = struct.unpack("<i", b"/adm")[0]
MAGIC_IN = struct.unpack("<H", b"in")[0]

filter_body = func_body(
    uleb128(0),
    b"\x20\x01"                      # local.get 1 (len)
    b"\x41\x06"                       # i32.const 6
    b"\x46"                            # i32.eq
    b"\x20\x00"                       # local.get 0 (ptr)
    b"\x28\x00\x00"                   # i32.load align=0 offset=0
    b"\x41" + sleb128(MAGIC_ADM) +
    b"\x46"                            # i32.eq
    b"\x71"                            # i32.and
    b"\x20\x00"                       # local.get 0 (ptr)
    b"\x2f\x00\x04"                   # i32.load16_u align=0 offset=4
    b"\x41" + sleb128(MAGIC_IN) +
    b"\x46"                            # i32.eq
    b"\x71",                           # i32.and
)

code_section = section(10, uleb128(2) + alloc_body + filter_body)

module = MAGIC + VERSION + type_section + function_section + memory_section + global_section + export_section + code_section

with open("admin_blocker.wasm", "wb") as f:
    f.write(module)

print(f"admin_blocker.wasm écrit ({len(module)} octets)")
