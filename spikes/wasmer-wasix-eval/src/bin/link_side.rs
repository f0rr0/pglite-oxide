use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;
use wasmer::{
    Extern, ExternType, Function, Global, Imports, Instance, Memory, Module, Store, Table, Tag,
    Type, Value,
};
use wasmer_wasix_eval::{
    CacheMode, WasmerModuleCompiler, cranelift_engine, print_engine_report,
};

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;

    let engine = cranelift_engine();
    print_engine_report(&engine);
    let compiler = WasmerModuleCompiler::new(args.cache_dir.clone(), args.cache_mode)?;
    let mut store = Store::new(engine.clone());

    let main_bytes = fs::read(&args.main_path)?;
    let main_mod = compiler
        .load_or_compile(&engine, &store, "pglite-main", &main_bytes)?
        .module;

    let (main_imports, host_imports) = dummy_imports(&mut store, &main_mod)?;
    let started = Instant::now();
    let main = Instance::new(&mut store, &main_mod, &main_imports)?;
    println!("main instantiate ok {:?}; exports={}", started.elapsed(), main.exports.iter().count());

    let side_bytes = fs::read(&args.side_path)?;
    let side_mod = compiler
        .load_or_compile(&engine, &store, "extension-side", &side_bytes)?
        .module;

    let side_imports = linked_side_imports(&mut store, &side_mod, &main, &host_imports)?;
    let started = Instant::now();
    match Instance::new(&mut store, &side_mod, &side_imports) {
        Ok(side) => println!("side linked instantiate ok {:?}; exports={}", started.elapsed(), side.exports.iter().count()),
        Err(err) => println!("side linked instantiate failed {:?}: {err}", started.elapsed()),
    }

    Ok(())
}

struct Args {
    main_path: PathBuf,
    side_path: PathBuf,
    cache_dir: Option<PathBuf>,
    cache_mode: CacheMode,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut main_path = None;
        let mut side_path = None;
        let mut cache_dir = Some(PathBuf::from(
            "../wasix-postgres-build/build/wasmer-module-cache",
        ));
        let mut cache_mode = CacheMode::Use;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--cache-dir" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--cache-dir requires a path"))?;
                    cache_dir = Some(PathBuf::from(value));
                }
                "--cache-mode" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--cache-mode requires use|rebuild|off"))?;
                    cache_mode = CacheMode::parse(&value)?;
                }
                "--no-cache" => {
                    cache_dir = None;
                    cache_mode = CacheMode::Off;
                }
                "-h" | "--help" => {
                    println!(
                        "usage: cargo run --bin link_side -- [--cache-dir PATH] [--cache-mode use|rebuild|off] MAIN_WASM SIDE_WASM"
                    );
                    std::process::exit(0);
                }
                value if main_path.is_none() => main_path = Some(PathBuf::from(value)),
                value if side_path.is_none() => side_path = Some(PathBuf::from(value)),
                other => anyhow::bail!("unknown argument: {other}"),
            }
        }

        Ok(Self {
            main_path: main_path.ok_or_else(|| anyhow::anyhow!("main wasm path missing"))?,
            side_path: side_path.ok_or_else(|| anyhow::anyhow!("side wasm path missing"))?,
            cache_dir,
            cache_mode,
        })
    }
}

fn linked_side_imports(
    store: &mut Store,
    side: &Module,
    main: &Instance,
    host_imports: &HashMap<(String, String), Extern>,
) -> anyhow::Result<Imports> {
    let mut imports = Imports::new();
    let mut from_main = 0usize;
    let mut from_host = 0usize;
    let mut from_got_mem = 0usize;
    let mut from_got_func = 0usize;
    let mut synthetic_dummy = 0usize;
    let linked_table = if let Some(table) = main
        .exports
        .get_extern("__indirect_function_table")
        .and_then(|export| match export {
            Extern::Table(table) => Some(table.clone()),
            _ => None,
        }) {
        Some(table)
    } else {
        create_side_table(store, side)?
    };
    let mut fixed_table_slot = 0u32;
    let mut table_defs = 0usize;

    for import in side.imports() {
        if import.module() == "GOT.mem"
            && let Some(Extern::Global(global)) = main.exports.get_extern(import.name())
        {
            let value = global.get(store);
            let relocated = Global::new_mut(store, value);
            imports.define(import.module(), import.name(), relocated);
            from_main += 1;
            from_got_mem += 1;
            continue;
        }

        if import.module() == "GOT.func"
            && let Some(table) = &linked_table
            && let Some(Extern::Function(function)) = main.exports.get_extern(import.name())
        {
            let index = match table.grow(&mut *store, 1, Value::FuncRef(None)) {
                Ok(index) => index,
                Err(err) if fixed_table_slot < table.size(&mut *store) => {
                    let index = fixed_table_slot;
                    fixed_table_slot += 1;
                    eprintln!(
                        "reusing fixed table slot {index} for GOT.func.{} after grow failed: {err}",
                        import.name()
                    );
                    index
                }
                Err(err) => return Err(err.into()),
            };
            table.set(store, index, Value::FuncRef(Some(function.clone())))?;
            let relocated = Global::new_mut(store, Value::I32(index as i32));
            imports.define(import.module(), import.name(), relocated);
            from_main += 1;
            from_got_func += 1;
            continue;
        }

        if let ExternType::Table(_) = import.ty()
            && import.name() == "__indirect_function_table"
            && let Some(table) = &linked_table
        {
            imports.define(import.module(), import.name(), table.clone());
            table_defs += 1;
            continue;
        }

        if let Some(exported) = main.exports.get_extern(import.name()) {
            imports.define(import.module(), import.name(), exported.clone());
            from_main += 1;
            continue;
        }

        let value = if let Some(value) =
            host_imports.get(&(import.module().to_string(), import.name().to_string()))
        {
            from_host += 1;
            value.clone()
        } else {
            synthetic_dummy += 1;
            dummy_extern(store, import.ty())?
        };
        imports.define(import.module(), import.name(), value);
    }

    println!(
        "side imports resolved from main={from_main} host={from_host} got.mem={from_got_mem} got.func={from_got_func} tables={table_defs} synthetic_dummy={synthetic_dummy}"
    );
    Ok(imports)
}

fn create_side_table(store: &mut Store, side: &Module) -> anyhow::Result<Option<Table>> {
    for import in side.imports() {
        if import.name() == "__indirect_function_table"
            && let ExternType::Table(ty) = import.ty()
        {
            eprintln!(
                "main does not export __indirect_function_table; creating side table for {}.{} with type {ty}",
                import.module(),
                import.name()
            );
            return Ok(Some(Table::new(store, *ty, Value::FuncRef(None))?));
        }
    }
    Ok(None)
}

fn dummy_imports(store: &mut Store, module: &Module) -> anyhow::Result<(Imports, HashMap<(String, String), Extern>)> {
    let mut imports = Imports::new();
    let mut saved = HashMap::new();
    for import in module.imports() {
        let value = dummy_extern(store, import.ty())?;
        saved.insert((import.module().to_string(), import.name().to_string()), value.clone());
        imports.define(import.module(), import.name(), value);
    }
    Ok((imports, saved))
}

fn dummy_extern(store: &mut Store, ty: &ExternType) -> anyhow::Result<Extern> {
    Ok(match ty {
        ExternType::Function(ty) => {
            let ty = ty.clone();
            let results = ty.results().to_vec();
            Function::new(store, &ty, move |_values: &[Value]| Ok(results.iter().copied().map(zero_value).collect())).into()
        }
        ExternType::Global(ty) => {
            let value = zero_value(ty.ty);
            if ty.mutability.is_mutable() { Global::new_mut(store, value).into() } else { Global::new(store, value).into() }
        }
        ExternType::Memory(ty) => Memory::new(store, *ty)?.into(),
        ExternType::Table(ty) => Table::new(store, *ty, Value::FuncRef(None))?.into(),
        ExternType::Tag(ty) => Tag::new(store, ty.params().to_vec()).into(),
    })
}

fn zero_value(ty: Type) -> Value {
    match ty {
        Type::I32 => Value::I32(0),
        Type::I64 => Value::I64(0),
        Type::F32 => Value::F32(0.0),
        Type::F64 => Value::F64(0.0),
        Type::V128 => Value::V128(0),
        Type::ExternRef => Value::ExternRef(None),
        Type::FuncRef => Value::FuncRef(None),
        Type::ExceptionRef => Value::ExceptionRef(None),
    }
}
