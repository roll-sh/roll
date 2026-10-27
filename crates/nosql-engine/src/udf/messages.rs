//! Every message of the UDF subsystem, per dialect.
//!
//! Cassandra's texts are ported from `CreateFunctionStatement`, `CreateAggregateStatement`,
//! `DropFunctionStatement`, `DropAggregateStatement` and `UDFunction` (Apache-2.0). Every text
//! here was verified on the references, with UDFs disabled (`docs/nosql/reference/errors.md`) and
//! enabled (`scylladb/scylla:2026.3.3` with Lua UDFs, `cassandra:5.0.9` with Java UDFs:
//! `docs/nosql/reference/capture/udf_cases.py`, replayed by `tests/captured_udf_harness.rs`).
//! ScyllaDB's Lua call failures are in `udf::lua` (`Failure`).

use nosql_types::Dialect;

use crate::error::Error;

/// UDF statements and calls while `user_defined_functions_enabled` is off.
pub fn disabled(d: Dialect) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => {
            "User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them"
        }
        Dialect::Cassandra => {
            "User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable"
        }
    })
}

/// A language the dialect doesn't run (`language` as written).
pub fn language(d: Dialect, language: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Language '{language}' is not supported"),
        Dialect::Cassandra => "Currently only Java UDFs are available in Cassandra. For more information - CASSANDRA-18252 and CASSANDRA-17281".to_string(),
    })
}

/// Schema changes in a system keyspace.
pub fn system_keyspace(d: Dialect, ks: &str) -> Error {
    match d {
        Dialect::Scylla => Error::unauthorized(format!("{ks} keyspace is not user-modifiable.")),
        Dialect::Cassandra => {
            Error::invalid(format!("System keyspace '{ks}' is not user-modifiable"))
        }
    }
}

pub fn both_directives() -> Error {
    Error::invalid("Cannot use both 'OR REPLACE' and 'IF NOT EXISTS' directives")
}

pub fn invalid_name(what: &str, name: &str) -> Error {
    Error::invalid(format!("{what} name '{name}' is invalid"))
}

/// `Duplicate argument names for given function f with argument names [a, a]`.
pub fn duplicate_arguments(name: &str, args: &[String]) -> Error {
    Error::invalid(format!(
        "Duplicate argument names for given function {name} with argument names [{}]",
        args.join(", ")
    ))
}

/// Cassandra; `what`: `Argument`, `Return type` or `State type`.
pub fn frozen(what: &str, ty: &str) -> Error {
    Error::invalid(format!(
        "{what} '{ty}' cannot be frozen; remove frozen<> modifier from '{ty}'"
    ))
}

/// ScyllaDB: a frozen user type as an argument or return type (frozen collections are fine).
pub fn frozen_udt() -> Error {
    Error::invalid("User defined argument and return types should not be frozen")
}

/// Cassandra's missing keyspace of CREATE FUNCTION.
pub fn keyspace_missing(ks: &str) -> Error {
    Error::invalid(format!("Keyspace '{ks}' doesn't exist"))
}

/// ScyllaDB's missing keyspace of CREATE FUNCTION (a server error).
pub fn keyspace_missing_scylla(ks: &str) -> Error {
    Error::server(format!("Can't find a keyspace {ks}"))
}

/// Cassandra's `CQL3Type.Raw.prepare(keyspace)` for a missing keyspace (CREATE AGGREGATE
/// authorization).
pub fn keyspace_missing_config(ks: &str) -> Error {
    Error::config(format!("Keyspace {ks} doesn't exist"))
}

pub fn function_exists(name: &str) -> Error {
    Error::invalid(format!("Function '{name}' already exists"))
}

pub fn aggregate_exists(name: &str) -> Error {
    Error::invalid(format!("Aggregate '{name}' already exists"))
}

/// ScyllaDB's CREATE of a function or aggregate whose signature is taken; `shown` is the
/// existing one ([`function_shown`] for a function, `ks.name` for an aggregate).
pub fn exists_scylla(shown: &str) -> Error {
    Error::invalid(format!("The function '{shown}' already exists"))
}

/// ScyllaDB's `ks.f : (int, int) -> int`.
pub fn function_shown(ks: &str, name: &str, args: &[String], ret: &str) -> String {
    format!("{ks}.{name} : ({}) -> {ret}", args.join(", "))
}

/// ScyllaDB's CREATE OR REPLACE over the other kind; `what`: `function` or `aggregate`.
pub fn cannot_replace(shown: &str, what: &str) -> Error {
    Error::invalid(format!(
        "Cannot replace '{shown}' which is not a user defined {what}"
    ))
}

pub fn function_replaces_aggregate(name: &str) -> Error {
    Error::invalid(format!("Function '{name}' cannot replace an aggregate"))
}

pub fn aggregate_replaces_function(name: &str) -> Error {
    Error::invalid(format!("Aggregate '{name}' cannot replace a function"))
}

pub fn null_input_directive(name: &str, called: bool) -> Error {
    Error::invalid(format!(
        "Function '{name}' must have {} directive",
        if called {
            "CALLED ON NULL INPUT"
        } else {
            "RETURNS NULL ON NULL INPUT"
        }
    ))
}

pub fn replace_return_type(name: &str, new: &str, old: &str) -> Error {
    Error::invalid(format!(
        "Cannot replace function '{name}', the new return type {new} is not compatible with the return type {old} of existing function"
    ))
}

pub fn replace_aggregate_return_type(name: &str, new: &str, old: &str) -> Error {
    Error::invalid(format!(
        "Cannot replace aggregate '{name}', the new return type {new} isn't compatible with the return type {old} of existing function"
    ))
}

/// `shown`: Cassandra `ks.f(int, int)`; ScyllaDB `f(int, int)`.
pub fn state_function_missing(d: Dialect, shown: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("State function {shown} not found"),
        Dialect::Cassandra => format!("State function {shown} doesn't exist"),
    })
}

/// `shown`: Cassandra `ks.f(int)`; ScyllaDB `f(int)`.
pub fn final_function_missing(d: Dialect, shown: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Final function {shown} not found"),
        Dialect::Cassandra => format!("Final function {shown} doesn't exist"),
    })
}

/// ScyllaDB only; `marshal`: the state type's marshal class.
pub fn reduce_function_missing(name: &str, marshal: &str) -> Error {
    Error::invalid(format!(
        "Scalar reduce function {name} for state type {marshal} not found."
    ))
}

/// ScyllaDB: the state function returns another type than the state.
pub fn state_not_returned(name: &str, stype: &str) -> Error {
    Error::invalid(format!(
        "State function '{name}' doesn't return state ({stype})"
    ))
}

/// Cassandra; `what`: `State` or `Final`.
pub fn not_scalar(what: &str, shown: &str) -> Error {
    Error::invalid(format!("{what} function {shown} isn't a scalar function"))
}

/// Cassandra.
pub fn state_return_type(shown: &str) -> Error {
    Error::invalid(format!(
        "State function {shown} return type must be the same as the first argument type - check STYPE, argument and return types"
    ))
}

pub fn initcond_invalid(ty: &str) -> Error {
    Error::invalid(format!("Invalid value for INITCOND of type {ty}"))
}

pub fn initcond_empty() -> Error {
    Error::invalid("INITCOND must not be empty for all types except TEXT, ASCII, BLOB")
}

/// Cassandra only (ScyllaDB creates the aggregate; it then returns null).
pub fn initcond_required(aggregate: &str, state_function: &str) -> Error {
    Error::invalid(format!(
        "Cannot create aggregate '{aggregate}' without INITCOND because state function {state_function} does not accept 'null' arguments"
    ))
}

/// A bind marker in INITCOND: both references fail with an internal error.
pub fn initcond_bind(d: Dialect) -> Error {
    Error::server(match d {
        Dialect::Scylla => "out of range small vector access",
        Dialect::Cassandra => "java.lang.IndexOutOfBoundsException: Index: 0",
    })
}

/// Cassandra.
pub fn function_missing(name: &str) -> Error {
    Error::invalid(format!("Function '{name}' doesn't exist"))
}

/// Cassandra.
pub fn aggregate_missing(name: &str) -> Error {
    Error::invalid(format!("Aggregate '{name}' doesn't exist"))
}

/// ScyllaDB: nothing named `ks.name` (DROP FUNCTION and AGGREGATE).
pub fn none_named(ks: &str, name: &str) -> Error {
    Error::invalid(format!("No function named {ks}.{name} found"))
}

/// ScyllaDB: no overload with the named argument types (`types` as written).
pub fn overload_missing(ks: &str, name: &str, types: &[String]) -> Error {
    Error::invalid(format!(
        "User function {ks}.{name}({}) doesn't exist",
        types.join(", ")
    ))
}

/// ScyllaDB: DROP without argument types of an overloaded name.
pub fn multiple_named(ks: &str, name: &str) -> Error {
    Error::invalid(format!("There are multiple functions named {ks}.{name}"))
}

/// ScyllaDB: DROP FUNCTION of an aggregate (`what` = `function`) or DROP AGGREGATE of a
/// function (`aggregate`); `shown` as in [`exists_scylla`].
pub fn not_user_defined(shown: &str, what: &str) -> Error {
    Error::invalid(format!("'{shown}' is not a user defined {what}"))
}

/// ScyllaDB: dropping a function an aggregate uses.
pub fn used_by_aggregate_scylla(shown: &str, aggregate: &str) -> Error {
    Error::invalid(format!(
        "Cannot delete function {shown}, as it is used by user-defined aggregate {aggregate}"
    ))
}

/// Cassandra; `kind`: `FUNCTION` or `AGGREGATE`.
pub fn ambiguous_drop(kind: &str, name: &str) -> Error {
    Error::invalid(format!(
        "'DROP {kind} {name}' matches multiple function definitions; specify the argument types by issuing a statement like 'DROP {kind} {name} (type, type, ...)'. You can use cqlsh 'DESCRIBE {kind} {name}' command to find all overloads"
    ))
}

/// Cassandra.
pub fn used_by_aggregates(name: &str, aggregates: &[String]) -> Error {
    Error::invalid(format!(
        "Function '{name}' is still referenced by aggregates {}",
        aggregates.join(", ")
    ))
}

pub fn used_by_masks(name: &str, tables: &[String]) -> Error {
    Error::invalid(format!(
        "Function '{name}' is still referenced by column masks in tables {}",
        tables.join(", ")
    ))
}

/// `DropTypeStatement`.
pub fn type_used_by_functions(ks: &str, ty: &str, functions: &[String]) -> Error {
    Error::invalid(format!(
        "Cannot drop user type '{ks}.{ty}' as it is still used by functions {}",
        functions.join(", ")
    ))
}

/// ScyllaDB: a Lua body that doesn't compile (`detail`: Lua's message).
pub fn lua_compile(detail: &str) -> Error {
    Error::invalid(format!("could not compile: {detail}"))
}

/// ScyllaDB: a result that converted to an invalid value of the return type (a duration with
/// mixed signs); `signature` as [`function_shown`], `hex` the value.
pub fn invalid_return(signature: &str, hex: &str, ret: &str) -> String {
    format!(
        "runtime error: Return of function {signature} ({hex}) is not a valid value for its declared return type {ret}"
    )
}

/// What a Java UDF call answers (a `FUNCTION_FAILURE` whose message is the cause, as
/// Cassandra's `FunctionExecutionException.create` makes it): roll stores Java UDFs, so schemas
/// import, but can't run them (compatibility.md, "Out of scope").
pub const JAVA_UNSUPPORTED: &str =
    "java.lang.UnsupportedOperationException: Java UDFs are not supported by roll";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts() {
        assert_eq!(
            disabled(Dialect::Scylla).message,
            "User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them"
        );
        assert_eq!(
            system_keyspace(Dialect::Scylla, "system").code.code(),
            0x2100
        );
        assert_eq!(
            state_function_missing(Dialect::Cassandra, "e.nope(int, int)").message,
            "State function e.nope(int, int) doesn't exist"
        );
        assert_eq!(
            duplicate_arguments("f", &["a".into(), "a".into()]).message,
            "Duplicate argument names for given function f with argument names [a, a]"
        );
        assert_eq!(
            exists_scylla(&function_shown("ks", "f", &["int".into()], "int")).message,
            "The function 'ks.f : (int) -> int' already exists"
        );
        assert_eq!(keyspace_missing_scylla("x").code.code(), 0x0000);
        assert_eq!(keyspace_missing_config("x").code.code(), 0x2300);
        assert_eq!(initcond_bind(Dialect::Cassandra).code.code(), 0x0000);
    }
}
