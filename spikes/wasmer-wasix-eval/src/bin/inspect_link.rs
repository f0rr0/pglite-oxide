use std::collections::HashSet;
use std::fs;
use wasmparser::{Parser, Payload, TypeRef};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let main_path = args.next().ok_or_else(|| anyhow::anyhow!("main wasm path missing"))?;
    let side_path = args.next().ok_or_else(|| anyhow::anyhow!("side wasm path missing"))?;

    let main_exports = exports(&main_path)?;
    let mut from_main = 0usize;
    let mut got_mem = 0usize;
    let mut got_func = 0usize;
    let mut tables = 0usize;
    let mut host_abi = 0usize;
    let mut unresolved = Vec::new();
    let host_abi_imports = HashSet::from([
        "memory".to_owned(),
        "__stack_pointer".to_owned(),
        "__memory_base".to_owned(),
        "__table_base".to_owned(),
    ]);

    for import in imports(&side_path)? {
        let (module, name, ty) = import;
        if module == "GOT.mem" && main_exports.contains(&name) {
            got_mem += 1;
        } else if module == "GOT.func" && main_exports.contains(&name) {
            got_func += 1;
        } else if matches!(ty, TypeRef::Table(_)) && name == "__indirect_function_table" {
            tables += 1;
        } else if module == "env" && host_abi_imports.contains(&name) {
            host_abi += 1;
        } else if main_exports.contains(&name) {
            from_main += 1;
        } else {
            unresolved.push((module, name, format!("{ty:?}")));
        }
    }

    println!(
        "side imports: main={from_main} got.mem={got_mem} got.func={got_func} tables={tables} host_abi={host_abi} unresolved={}",
        unresolved.len()
    );
    for (module, name, ty) in unresolved {
        println!("unresolved {module}.{name}: {ty}");
    }

    Ok(())
}

fn exports(path: &str) -> anyhow::Result<HashSet<String>> {
    let bytes = fs::read(path)?;
    let mut names = HashSet::new();
    for payload in Parser::new(0).parse_all(&bytes) {
        if let Payload::ExportSection(exports) = payload? {
            for export in exports {
                let export = export?;
                names.insert(export.name.to_string());
            }
        }
    }
    Ok(names)
}

fn imports(path: &str) -> anyhow::Result<Vec<(String, String, TypeRef)>> {
    let bytes = fs::read(path)?;
    let mut imports = Vec::new();
    for payload in Parser::new(0).parse_all(&bytes) {
        if let Payload::ImportSection(section) = payload? {
            for import in section.into_imports() {
                let import = import?;
                imports.push((import.module.to_string(), import.name.to_string(), import.ty));
            }
        }
    }
    Ok(imports)
}
