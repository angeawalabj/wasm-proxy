fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: test_plugin <path.wasm> [v2]");
    let mode_v2 = std::env::args().nth(2).as_deref() == Some("v2");
    let bytes = std::fs::read(&path)?;

    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &bytes[..])?;
    let mut store = wasmi::Store::new(&engine, ());
    let linker = wasmi::Linker::new(&engine);
    let instance = linker
        .instantiate(&mut store, &module)?
        .start(&mut store)?;

    let memory = match instance.get_export(&mut store, "memory") {
        Some(wasmi::Extern::Memory(m)) => m,
        _ => panic!("pas de mémoire exportée"),
    };
    let alloc = instance.get_typed_func::<i32, i32>(&mut store, "alloc")?;

    if mode_v2 {
        let filter = instance.get_typed_func::<(i32, i32), i64>(&mut store, "filter_request")?;
        let inputs = [
            r#"{"method":"GET","path":"/admin","headers":{"x-api-key":"secret123"}}"#,
            r#"{"method":"GET","path":"/admin","headers":{"x-api-key":"wrong"}}"#,
            r#"{"method":"GET","path":"/admin","headers":{}}"#,
            r#"{"method":"GET","path":"/api/users","headers":{}}"#,
        ];
        for input in inputs {
            let bytes = input.as_bytes();
            let ptr = alloc.call(&mut store, bytes.len() as i32)?;
            memory.write(&mut store, ptr as usize, bytes).unwrap();
            let packed = filter.call(&mut store, (ptr, bytes.len() as i32))?;
            let out_ptr = ((packed as u64) >> 32) as usize;
            let out_len = ((packed as u64) & 0xFFFF_FFFF) as usize;
            let mut out = vec![0u8; out_len];
            memory.read(&store, out_ptr, &mut out).unwrap();
            let out_str = String::from_utf8_lossy(&out);
            println!("input={input}\n  -> {out_str}\n");
        }
    } else {
        let filter = instance.get_typed_func::<(i32, i32), i32>(&mut store, "filter_request")?;
        for test_path in ["/admin", "/api/users", "/adminx", "admin"] {
            let bytes = test_path.as_bytes();
            let ptr = alloc.call(&mut store, bytes.len() as i32)?;
            memory.write(&mut store, ptr as usize, bytes).unwrap();
            let result = filter.call(&mut store, (ptr, bytes.len() as i32))?;
            println!("path={test_path:?} -> filter_request={result} ({})",
                if result != 0 { "BLOQUÉ" } else { "autorisé" });
        }
    }

    Ok(())
}
