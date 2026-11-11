//! `amvalidate` for operator classes made by CREATE OPERATOR CLASS of the btree and hash
//! access methods (nbtvalidate.c's `btvalidate`, hashvalidate.c's `hashvalidate`): the
//! INFO messages and the verdict about the class's family. (Members' signatures are taken as
//! CREATE OPERATOR CLASS checked them.) Built-in classes, and those of the other access
//! methods, are valid.

use crate::catalog::Catalog;
use crate::catalog::userobj::MemberInfo;
use crate::error::code;
use crate::sql::func::FnCtx;

const BTREE: u32 = 403;
const HASH: u32 = 405;

/// identify_opfamily_groups: per (left, right) type pair, the strategies and support numbers.
struct Group {
    left: u32,
    right: u32,
    operators: u64,
    functions: u64,
}

fn groups(ops: &[MemberInfo], procs: &[MemberInfo]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut pairs: Vec<(u32, u32)> = ops.iter().chain(procs).map(|m| (m.left, m.right)).collect();
    pairs.sort_unstable();
    pairs.dedup();
    for (l, r) in pairs {
        let bits = |list: &[MemberInfo]| {
            list.iter()
                .filter(|m| m.left == l && m.right == r && (0..64).contains(&m.number))
                .fold(0u64, |b, m| b | (1u64 << m.number))
        };
        out.push(Group { left: l, right: r, operators: bits(ops), functions: bits(procs) });
    }
    out
}

pub fn amvalidate(oid: u32, c: &FnCtx) -> crate::Result<bool> {
    let cat = c.exec.catalog();
    let Some(class) = cat.opclass_info(oid) else {
        return Err(crate::error::internal(format!("cache lookup failed for operator class {oid}")));
    };
    if oid < crate::catalog::FIRST_USER_OID || !matches!(class.method, BTREE | HASH) {
        return Ok(true);
    }
    let family = cat.opfamily_info(class.family).map(|f| f.name).unwrap_or_default();
    let mut ops: Vec<MemberInfo> = cat.opmembers_all(false).into_iter().filter(|m| m.family == class.family).collect();
    let mut procs: Vec<MemberInfo> = cat.opmembers_all(true).into_iter().filter(|m| m.family == class.family).collect();
    // (The catcache lists' order: by types, then number.)
    ops.sort_by_key(|m| (m.left, m.right, m.number));
    procs.sort_by_key(|m| (m.left, m.right, m.number));
    let info = |msg: String| {
        let mut e = crate::session::notice(msg);
        e.code = code::INVALID_OBJECT_DEFINITION;
        e.severity = crate::error::Severity::Info;
        c.exec.notice(e);
    };
    let am = if class.method == BTREE { "btree" } else { "hash" };
    let ty = |t: u32| type_name(&cat, t, c);
    let mut ok = true;
    let (max_strategy, max_proc) = if class.method == BTREE { (5, 6) } else { (1, 3) };
    let mut hashable: Vec<u32> = Vec::new();
    for p in &procs {
        if class.method == HASH && p.left != p.right {
            info(format!("operator family \"{family}\" of access method {am} contains support function {} with different left and right input types", proc_name(&cat, p.object, c)));
            ok = false;
        }
        if p.number < 1 || p.number > max_proc {
            info(format!("operator family \"{family}\" of access method {am} contains function {} with invalid support number {}", proc_name(&cat, p.object, c), p.number));
            ok = false;
            continue;
        }
        if class.method == HASH && matches!(p.number, 1 | 2) && !hashable.contains(&p.left) {
            hashable.push(p.left);
        }
    }
    for o in &ops {
        if o.number < 1 || o.number > max_strategy {
            info(format!("operator family \"{family}\" of access method {am} contains operator {} with invalid strategy number {}", oper_name(o.object, c), o.number));
            ok = false;
        }
        if o.purpose != 's' || o.sortfamily != 0 {
            info(format!("operator family \"{family}\" of access method {am} contains invalid ORDER BY specification for operator {}", oper_name(o.object, c)));
            ok = false;
        }
        if class.method == HASH && (!hashable.contains(&o.left) || !hashable.contains(&o.right)) {
            info(format!("operator family \"{family}\" of access method {am} lacks support function for operator {}", oper_name(o.object, c)));
            ok = false;
        }
    }
    let gs = groups(&ops, &procs);
    let mut class_group = false;
    let mut types: Vec<u32> = Vec::new();
    let mut useful = 0;
    for g in &gs {
        if class.method == BTREE && g.operators == 0 && g.functions == 1 << 3 {
            continue;
        }
        useful += 1;
        if g.left == class.intype && g.right == class.intype {
            class_group = true;
        }
        for t in [g.left, g.right] {
            if !types.contains(&t) {
                types.push(t);
            }
        }
        let want = if class.method == BTREE { 0b11_1110 } else { 0b10 };
        if g.operators != want {
            info(format!("operator family \"{family}\" of access method {am} is missing operator(s) for types {} and {}", ty(g.left), ty(g.right)));
            ok = false;
        }
        if class.method == BTREE && g.functions & 0b10 == 0 {
            info(format!("operator family \"{family}\" of access method {am} is missing support function for types {} and {}", ty(g.left), ty(g.right)));
            ok = false;
        }
    }
    if !class_group {
        info(format!("operator class \"{}\" of access method {am} is missing operator(s)", class.name));
        ok = false;
    }
    let complete = if class.method == BTREE {
        useful == types.len() * types.len()
    } else {
        gs.len() == hashable.len() * hashable.len()
    };
    if !complete {
        info(format!("operator family \"{family}\" of access method {am} is missing cross-type operator(s)"));
        ok = false;
    }
    Ok(ok)
}

fn type_name(cat: &Catalog, t: u32, c: &FnCtx) -> String {
    super::system::format_type_oid(t, -1, cat, &c.exec.search_path).unwrap_or_else(|| t.to_string())
}

fn proc_name(cat: &Catalog, f: u32, c: &FnCtx) -> String {
    crate::catalog::depend::procedure_description(cat, f, &c.exec.search_path)
        .map(|d| d.trim_start_matches("function ").to_string())
        .unwrap_or_else(|| f.to_string())
}

fn oper_name(o: u32, c: &FnCtx) -> String {
    crate::types::text::format_value(&crate::types::Value::Oid(o), &crate::types::DataType::Regoperator, c.exec.fmt())
}
