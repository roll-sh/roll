//! ScyllaDB's WASM UDFs (`docs/cql/wasm.rst`), on the `wasmi` interpreter.
//!
//! - **Source.** The body is WebAssembly text (WAT); `CREATE FUNCTION` compiles it and checks
//!   that the module exports the function's name as a function ([`compile`]). Signatures are
//!   checked when called, as ScyllaDB does.
//! - **ABI.** `_scylla_abi` (a global holding the memory offset of a 32-bit version, 1 when
//!   missing) picks the memory management: version 1 grows the memory for each serialized
//!   argument, version 2 allocates with the module's `_scylla_malloc` and frees the result with
//!   `_scylla_free`. `RETURNS NULL ON NULL INPUT` functions take and return `bigint` (i64),
//!   `int` / `smallint` / `tinyint` / `boolean` (i32), `float` (f32) and `double` (f64) natively;
//!   everything else, and every value of a `CALLED ON NULL INPUT` function, is its CQL
//!   serialization in the module's memory, passed as an i64 `size << 32 | pointer` (size -1:
//!   null).
//! - **Limits.** A call runs on [`FUEL`] units of fuel (about one per instruction; ScyllaDB
//!   bounds a call by time instead) and the memory is capped at [`MAX_MEMORY`]; running out is
//!   an `InvalidRequest`, as are traps and signature mismatches.
//! - **Instances.** A function keeps its instance between calls (a fresh one when the memory
//!   grew past [`RESET_MEMORY`]); [`instance_count`] is ScyllaDB's
//!   `scylla_user_functions_cache_instace_count_any` (instances of functions that still exist).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use nosql_types::CqlType;
use parking_lot::Mutex;
use wasmi::{
    Engine, ExternType, Instance as WasmInstance, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder, Val, ValType,
};

/// Fuel of one call.
pub const FUEL: u64 = 100_000_000;
/// The largest memory an instance may grow to.
pub const MAX_MEMORY: usize = 256 << 20;
/// An instance whose memory grew past this is not reused.
pub const RESET_MEMORY: usize = 32 << 20;

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut c = wasmi::Config::default();
        c.consume_fuel(true);
        Engine::new(&c)
    })
}

/// A compiled function body.
pub struct Compiled {
    module: Module,
    name: String,
}

/// Compiles a WAT body exporting function `name`; `Err` is ScyllaDB's message.
pub fn compile(name: &str, body: &str) -> Result<Compiled, String> {
    let wasm = wat::parse_str(body).map_err(|e| format!("Compilation failed: {e}"))?;
    let module = Module::new(engine(), &wasm).map_err(|e| format!("Compilation failed: {e}"))?;
    match module
        .exports()
        .find(|e| e.name() == name)
        .map(|e| e.ty().clone())
    {
        None => Err(format!(
            "Compilation failed: Function {name} was not found in given wasm source code"
        )),
        Some(ExternType::Func(_)) => Ok(Compiled {
            module,
            name: name.to_string(),
        }),
        Some(_) => Err(format!(
            "Compilation failed: Exported symbol {name} is not a function"
        )),
    }
}

/// The function an instance belongs to: `keyspace`, `name`, argument types.
type FunctionKey = (String, String, Vec<String>);

/// Live instances, by id.
fn registry() -> &'static Mutex<HashMap<u64, FunctionKey>> {
    static R: OnceLock<Mutex<HashMap<u64, FunctionKey>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Instances of functions that exist in `schema`.
pub fn instance_count(schema: &crate::schema::Schema) -> u64 {
    registry()
        .lock()
        .values()
        .filter(|(ks, name, types)| {
            schema.keyspace(ks).is_some_and(|k| {
                k.functions
                    .iter()
                    .any(|f| &f.name == name && &f.argument_types == types)
            })
        })
        .count() as u64
}

/// An instance of a function's module.
pub struct Instance {
    store: Store<StoreLimits>,
    instance: WasmInstance,
    abi: u32,
    id: u64,
}

impl Drop for Instance {
    fn drop(&mut self) {
        registry().lock().remove(&self.id);
    }
}

/// Who an instance belongs to.
pub struct Owner<'a> {
    pub keyspace: &'a str,
    pub name: &'a str,
    pub argument_types: &'a [String],
}

fn failed(name: &str, why: impl std::fmt::Display) -> String {
    format!("wasm UDF {name} failed: {why}")
}

fn instantiate(c: &Compiled, owner: &Owner<'_>) -> Result<Instance, String> {
    let limits = StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build();
    let mut store = Store::new(engine(), limits);
    store.limiter(|l| l);
    store.set_fuel(FUEL).map_err(|e| failed(&c.name, e))?;
    let instance = Linker::new(engine())
        .instantiate_and_start(&mut store, &c.module)
        .map_err(|e| failed(&c.name, e))?;
    let abi = match instance
        .get_global(&store, "_scylla_abi")
        .map(|g| g.get(&store))
    {
        Some(Val::I32(offset)) => {
            let mem = instance
                .get_memory(&store, "memory")
                .ok_or_else(|| failed(&c.name, "the module exports no memory"))?;
            let data = mem.data(&store);
            let at = offset as u32 as usize;
            let v = data
                .get(at..at + 4)
                .ok_or_else(|| failed(&c.name, "_scylla_abi is out of the memory"))?;
            u32::from_le_bytes([v[0], v[1], v[2], v[3]])
        }
        _ => 1,
    };
    // Version 2 manages memory with the module's functions; anything else is version 1
    // (AssemblyScript exports `_scylla_abi = [1]`, a pointer to the array, and runs).
    let abi = if abi == 2 { 2 } else { 1 };
    static IDS: AtomicU64 = AtomicU64::new(0);
    let id = IDS.fetch_add(1, Ordering::Relaxed);
    registry().lock().insert(
        id,
        (
            owner.keyspace.to_string(),
            owner.name.to_string(),
            owner.argument_types.to_vec(),
        ),
    );
    Ok(Instance {
        store,
        instance,
        abi,
        id,
    })
}

/// The Wasm type of a natively passed CQL type.
fn native(ty: &CqlType) -> Option<ValType> {
    Some(match ty {
        CqlType::Bigint => ValType::I64,
        CqlType::Int | CqlType::Smallint | CqlType::Tinyint | CqlType::Boolean => ValType::I32,
        CqlType::Float => ValType::F32,
        CqlType::Double => ValType::F64,
        _ => return None,
    })
}

fn type_name(t: &ValType) -> &'static str {
    match t {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        _ => "reference",
    }
}

fn to_val(ty: &CqlType, b: &[u8]) -> Option<Val> {
    let arr = |n: usize| b.get(..n);
    Some(match ty {
        CqlType::Bigint => Val::I64(i64::from_be_bytes(arr(8)?.try_into().ok()?)),
        CqlType::Int => Val::I32(i32::from_be_bytes(arr(4)?.try_into().ok()?)),
        CqlType::Smallint => Val::I32(i16::from_be_bytes(arr(2)?.try_into().ok()?) as i32),
        CqlType::Tinyint => Val::I32(*b.first()? as i8 as i32),
        CqlType::Boolean => Val::I32(i32::from(*b.first()? != 0)),
        CqlType::Float => Val::F32(f32::from_be_bytes(arr(4)?.try_into().ok()?).into()),
        CqlType::Double => Val::F64(f64::from_be_bytes(arr(8)?.try_into().ok()?).into()),
        _ => return None,
    })
}

fn from_val(ty: &CqlType, v: &Val) -> Option<Bytes> {
    Some(match (ty, v) {
        (CqlType::Bigint, Val::I64(x)) => Bytes::copy_from_slice(&x.to_be_bytes()),
        (CqlType::Int, Val::I32(x)) => Bytes::copy_from_slice(&x.to_be_bytes()),
        (CqlType::Smallint, Val::I32(x)) => Bytes::copy_from_slice(&(*x as i16).to_be_bytes()),
        (CqlType::Tinyint, Val::I32(x)) => Bytes::copy_from_slice(&[*x as i8 as u8]),
        (CqlType::Boolean, Val::I32(x)) => Bytes::copy_from_slice(&[u8::from(*x != 0)]),
        (CqlType::Float, Val::F32(x)) => Bytes::copy_from_slice(&x.to_float().to_be_bytes()),
        (CqlType::Double, Val::F64(x)) => Bytes::copy_from_slice(&x.to_float().to_be_bytes()),
        _ => return None,
    })
}

impl Instance {
    fn memory(&self, name: &str) -> Result<wasmi::Memory, String> {
        self.instance
            .get_memory(&self.store, "memory")
            .ok_or_else(|| failed(name, "the module exports no memory"))
    }

    fn call_export(&mut self, name: &str, export: &str, args: &[Val]) -> Result<Val, String> {
        let f = self
            .instance
            .get_func(&self.store, export)
            .ok_or_else(|| failed(name, format!("{export} is not exported")))?;
        let mut out: Vec<Val> = f
            .ty(&self.store)
            .results()
            .iter()
            .map(|t| Val::default_for_ty(*t))
            .collect();
        f.call(&mut self.store, args, &mut out)
            .map_err(|e| failed(name, e))?;
        Ok(out.into_iter().next().unwrap_or(Val::I32(0)))
    }

    /// Copies a serialized value into the module's memory: its pointer.
    fn place(&mut self, name: &str, b: &[u8]) -> Result<u32, String> {
        let mem = self.memory(name)?;
        let ptr = if self.abi == 2 {
            match self.call_export(name, "_scylla_malloc", &[Val::I32(b.len() as i32)])? {
                Val::I32(p) => p as u32,
                _ => return Err(failed(name, "_scylla_malloc must return an i32")),
            }
        } else {
            let pages = (b.len() as u64).div_ceil(65_536).max(1);
            let before = mem
                .grow(&mut self.store, pages)
                .map_err(|e| failed(name, e))?;
            (before * 65_536) as u32
        };
        mem.write(&mut self.store, ptr as usize, b)
            .map_err(|e| failed(name, e))?;
        Ok(ptr)
    }
}

/// Calls `name` of a compiled module (keeping its instance in `slot`) with typed arguments.
pub fn call(
    c: &Compiled,
    slot: &Mutex<Option<Instance>>,
    owner: &Owner<'_>,
    args: &[(&CqlType, Option<&[u8]>)],
    ret: &CqlType,
    called_on_null: bool,
) -> Result<Option<Bytes>, String> {
    let name = c.name.as_str();
    let mut guard = slot.lock();
    let inst = match guard.as_mut() {
        Some(i) => i,
        None => guard.insert(instantiate(c, owner)?),
    };
    let r = call_in(inst, name, args, ret, called_on_null);
    let grown = inst
        .memory(name)
        .map(|m| m.data(&inst.store).len() > RESET_MEMORY)
        .unwrap_or(false);
    // A failed call may leave the instance broken (and the fuel spent): start over.
    if r.is_err() || grown {
        *guard = None;
    }
    r
}

fn call_in(
    inst: &mut Instance,
    name: &str,
    args: &[(&CqlType, Option<&[u8]>)],
    ret: &CqlType,
    called_on_null: bool,
) -> Result<Option<Bytes>, String> {
    inst.store.set_fuel(FUEL).map_err(|e| failed(name, e))?;
    let f = inst
        .instance
        .get_func(&inst.store, name)
        .ok_or_else(|| failed(name, "not exported"))?;
    let ty = f.ty(&inst.store);
    let mut vals = Vec::with_capacity(args.len());
    for (t, v) in args {
        match (native(t), called_on_null, v) {
            (Some(_), false, Some(b)) => {
                vals.push(to_val(t, b).ok_or_else(|| failed(name, "malformed argument"))?)
            }
            (_, _, None) => vals.push(Val::I64(-1i64 << 32)),
            (_, _, Some(b)) => {
                let ptr = inst.place(name, b)?;
                vals.push(Val::I64(((b.len() as i64) << 32) | i64::from(ptr)));
            }
        }
    }
    let given: Vec<ValType> = vals.iter().map(Val::ty).collect();
    if ty.params() != given.as_slice() {
        return Err(failed(name, "type mismatch with parameters"));
    }
    let serialized = called_on_null || native(ret).is_none();
    let want = if serialized {
        ValType::I64
    } else {
        native(ret).unwrap_or(ValType::I64)
    };
    match ty.results() {
        [got] if *got == want => {}
        [got] => {
            return Err(failed(
                name,
                format!("Expected {}, got {}", type_name(&want), type_name(got)),
            ));
        }
        _ => return Err(failed(name, "the function must return one value")),
    }
    let mut out = [Val::I32(0)];
    f.call(&mut inst.store, &vals, &mut out).map_err(|e| {
        let text = e.to_string();
        if text.contains("fuel") {
            failed(name, "all fuel consumed by WebAssembly")
        } else {
            failed(name, text)
        }
    })?;
    if !serialized {
        return from_val(ret, &out[0])
            .map(Some)
            .ok_or_else(|| failed(name, "malformed result"));
    }
    let Val::I64(packed) = out[0] else {
        return Err(failed(name, "malformed result"));
    };
    let size = (packed >> 32) as i32;
    if size == -1 {
        return Ok(None);
    }
    let ptr = packed as u32 as usize;
    let mem = inst.memory(name)?;
    let data = mem.data(&inst.store);
    let bytes = data
        .get(ptr..ptr + size.max(0) as usize)
        .map(Bytes::copy_from_slice)
        .ok_or_else(|| failed(name, "the result is out of the memory"))?;
    if inst.abi == 2 {
        inst.call_export(name, "_scylla_free", &[Val::I32(ptr as i32)])?;
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: Owner<'static> = Owner {
        keyspace: "ks",
        name: "f",
        argument_types: &[],
    };

    fn run(
        name: &str,
        body: &str,
        args: &[(&CqlType, Option<&[u8]>)],
        ret: &CqlType,
        on_null: bool,
    ) -> Result<Option<Bytes>, String> {
        let c = compile(name, body)?;
        call(&c, &Mutex::new(None), &OWNER, args, ret, on_null)
    }

    const FIB: &str = r#"(module
      (func $fib (param $n i64) (result i64)
        (if (i64.lt_s (local.get $n) (i64.const 2)) (then (return (local.get $n))))
        (i64.add (call $fib (i64.sub (local.get $n) (i64.const 1)))
                 (call $fib (i64.sub (local.get $n) (i64.const 2)))))
      (memory (;0;) 17)
      (export "memory" (memory 0))
      (export "fib" (func $fib)))"#;

    #[test]
    fn native_calls_and_limits() {
        let ten = 10i64.to_be_bytes();
        let r = run(
            "fib",
            FIB,
            &[(&CqlType::Bigint, Some(&ten))],
            &CqlType::Bigint,
            false,
        );
        assert_eq!(r.unwrap().as_deref(), Some(&55i64.to_be_bytes()[..]));
        let big = 997i64.to_be_bytes();
        let e = run(
            "fib",
            FIB,
            &[(&CqlType::Bigint, Some(&big))],
            &CqlType::Bigint,
            false,
        );
        assert_eq!(
            e.unwrap_err(),
            "wasm UDF fib failed: all fuel consumed by WebAssembly"
        );
        // Signatures are checked at the call.
        let i = 1i32.to_be_bytes();
        let e = run(
            "fib",
            FIB,
            &[(&CqlType::Int, Some(&i))],
            &CqlType::Bigint,
            false,
        );
        assert!(e.unwrap_err().contains("type mismatch"));
        let e = run(
            "fib",
            FIB,
            &[(&CqlType::Bigint, Some(&ten))],
            &CqlType::Int,
            false,
        );
        assert!(e.unwrap_err().contains("Expected i32, got i64"));
    }

    #[test]
    fn compile_errors() {
        assert!(
            compile("f", "not wat")
                .err()
                .unwrap()
                .starts_with("Compilation failed")
        );
        assert_eq!(
            compile("f", "(module)").err().unwrap(),
            "Compilation failed: Function f was not found in given wasm source code"
        );
        let e = compile(
            "memory",
            "(module (memory 1) (export \"memory\" (memory 0)))",
        );
        assert!(e.err().unwrap().contains("not a function"));
    }

    #[test]
    fn serialized_values_and_abi_2() {
        // ABI 1 with a null-accepting function: the argument is in memory; 42 for null.
        let body = r#"(module
          (memory (;0;) 1)
          (export "memory" (memory 0))
          (global (;0;) i32 (i32.const 1024))
          (export "_scylla_abi" (global 0))
          (data (i32.const 1024) "\01")
          (func $f (param i64) (result i64)
            (if (result i64) (i64.eq (i64.shr_s (local.get 0) (i64.const 32)) (i64.const -1))
              (then (i64.const -4294967296))
              (else (local.get 0))))
          (export "f" (func $f)))"#;
        let v = 7i32.to_be_bytes();
        let r = run("f", body, &[(&CqlType::Int, Some(&v))], &CqlType::Int, true);
        assert_eq!(r.unwrap().as_deref(), Some(&v[..]));
        let r = run("f", body, &[(&CqlType::Int, None)], &CqlType::Int, true);
        assert_eq!(r.unwrap(), None);
        let r = run(
            "f",
            body,
            &[(&CqlType::Text, Some(b"dog"))],
            &CqlType::Text,
            false,
        );
        assert_eq!(r.unwrap().as_deref(), Some(&b"dog"[..]));
        // ABI 2: the host allocates with `_scylla_malloc` and frees the result.
        let body2 = r#"(module
          (memory (;0;) 1)
          (export "memory" (memory 0))
          (global (;0;) i32 (i32.const 1024))
          (global $next (mut i32) (i32.const 2048))
          (global $freed (mut i32) (i32.const 0))
          (export "_scylla_abi" (global 0))
          (data (i32.const 1024) "\02")
          (func (export "_scylla_malloc") (param i32) (result i32)
            (global.get $next)
            (global.set $next (i32.add (global.get $next) (local.get 0))))
          (func (export "_scylla_free") (param i32)
            (global.set $freed (i32.add (global.get $freed) (i32.const 1))))
          (func (export "f") (param i64) (result i64) (local.get 0)))"#;
        let r = run(
            "f",
            body2,
            &[(&CqlType::Text, Some(b"cat"))],
            &CqlType::Text,
            false,
        );
        assert_eq!(r.unwrap().as_deref(), Some(&b"cat"[..]));
    }

    #[test]
    fn instances_are_counted_while_their_function_exists() {
        let c = compile("fib", FIB).unwrap();
        let slot = Mutex::new(None);
        let types = vec!["bigint".to_string()];
        let owner = Owner {
            keyspace: "wasm_count_ks",
            name: "fib",
            argument_types: &types,
        };
        let one = 1i64.to_be_bytes();
        call(
            &c,
            &slot,
            &owner,
            &[(&CqlType::Bigint, Some(&one))],
            &CqlType::Bigint,
            false,
        )
        .unwrap();
        assert!(slot.lock().is_some());
        let n = registry()
            .lock()
            .values()
            .filter(|(ks, ..)| ks == "wasm_count_ks")
            .count();
        assert_eq!(n, 1);
        *slot.lock() = None;
        assert!(
            !registry()
                .lock()
                .values()
                .any(|(ks, ..)| ks == "wasm_count_ks")
        );
    }
}
