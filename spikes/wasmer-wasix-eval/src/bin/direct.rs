use std::fs;
use std::time::Instant;
use wasmer::{
    Extern, ExternType, Function, Global, Imports, Instance, Memory, Module, Store, Table, Type,
    Value,
};
use wasmer_wasix_eval::{CacheMode, WasmerModuleCompiler, cranelift_engine, print_engine_report};

fn main() -> anyhow::Result<()> {
    let paths = std::env::args().skip(1).collect::<Vec<_>>();
    if paths.is_empty() {
        anyhow::bail!("usage: cargo run --bin direct -- <wasm-or-so>...");
    }

    for path in paths {
        println!("\n== {path} ==");
        let bytes = fs::read(&path)?;
        let engine = cranelift_engine();
        print_engine_report(&engine);
        let mut store = Store::new(engine.clone());
        let compiler = WasmerModuleCompiler::new(None, CacheMode::Off)?;

        let started = Instant::now();
        let module = compiler
            .load_or_compile(&engine, &store, path.clone(), &bytes)?
            .module;
        println!("compile ok {:?}", started.elapsed());

        let imports = dummy_imports(&mut store, &module)?;
        let started = Instant::now();
        match Instance::new(&mut store, &module, &imports) {
            Ok(instance) => {
                println!(
                    "instantiate ok {:?}; exports={}",
                    started.elapsed(),
                    instance.exports.iter().count()
                );
            }
            Err(err) => {
                println!("instantiate failed {:?}: {err}", started.elapsed());
            }
        }
    }
    Ok(())
}

fn dummy_imports(store: &mut Store, module: &Module) -> anyhow::Result<Imports> {
    let mut imports = Imports::new();
    for import in module.imports() {
        let value: Extern = match import.ty() {
            ExternType::Function(ty) => {
                let ty = ty.clone();
                let results = ty.results().to_vec();
                Function::new(store, &ty, move |_values: &[Value]| {
                    Ok(results.iter().copied().map(zero_value).collect())
                })
                .into()
            }
            ExternType::Global(ty) => {
                let value = zero_value(ty.ty);
                if ty.mutability.is_mutable() {
                    Global::new_mut(store, value).into()
                } else {
                    Global::new(store, value).into()
                }
            }
            ExternType::Memory(ty) => Memory::new(store, *ty)?.into(),
            ExternType::Table(ty) => Table::new(store, *ty, Value::FuncRef(None))?.into(),
            ExternType::Tag(_) => anyhow::bail!("tag imports not handled"),
        };
        imports.define(import.module(), import.name(), value);
    }
    Ok(imports)
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
