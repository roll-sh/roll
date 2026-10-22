//! `java.util` collections: `Iterable`, `Collection`, `List`, `Set`, `Map` and their
//! implementations, `Iterator`, `Collections`, `Arrays`, `Objects`, plus the Painless
//! augmentations Elasticsearch adds (`findAll`, `collect`, `groupBy`, `getByPath`, …).
//!
//! Interface methods accept every runtime receiver that implements the interface (lists,
//! sets, map views, read-only doc-values lists), the way Java's virtual dispatch would; failure
//! messages and the JDK frames above the script follow `java.util` of the JDK Elasticsearch
//! 9.5 ships.

use std::cell::RefCell;
use std::rc::Rc;

use crate::func::{self, call, call_bool, compare_with};
use crate::iter::{entry_values, JIter, Source};
use crate::lib_lang::{double, int, string};
use crate::lookup::Builder;
use crate::types::{cls, ClassId, Ty};
use crate::value::{ArrayData, JArray, JEntry, JList, JMap, MapOrder, Mutability, Obj, Value, ViewKind};
use crate::vm::{iae, npe, uoe, Throw, Thrown, Vm, VR};

fn jframes(f: &[&str]) -> Vec<String> {
    f.iter().map(|s| format!("java.base/{s}")).collect()
}

const CHECK_INDEX: &[&str] = &[
    "jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:100)",
    "jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)",
    "jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)",
    "java.util.Objects.checkIndex(Objects.java:365)",
];

fn ioobe(i: i64, len: usize, method: &str) -> Thrown {
    let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {i} out of bounds for length {len}")));
    let mut f = CHECK_INDEX.to_vec();
    f.push(method);
    t.frames = jframes(&f);
    t
}

fn uoe_frames(f: &[&str]) -> Thrown {
    let mut t = uoe();
    t.frames = jframes(f);
    t
}

/// `Collections.unmodifiable*` frames by operation (`@` marks the index-taking overloads).
fn unmodifiable_frame(op: &str) -> Option<&'static str> {
    Some(match op {
        "add" => "java.util.Collections$UnmodifiableCollection.add(Collections.java:1095)",
        "remove" => "java.util.Collections$UnmodifiableCollection.remove(Collections.java:1098)",
        "addAll" => "java.util.Collections$UnmodifiableCollection.addAll(Collections.java:1105)",
        "removeAll" => "java.util.Collections$UnmodifiableCollection.removeAll(Collections.java:1108)",
        "retainAll" => "java.util.Collections$UnmodifiableCollection.retainAll(Collections.java:1111)",
        "clear" => "java.util.Collections$UnmodifiableCollection.clear(Collections.java:1114)",
        "removeIf" => "java.util.Collections$UnmodifiableCollection.removeIf(Collections.java:1124)",
        "set" => "java.util.Collections$UnmodifiableList.set(Collections.java:1509)",
        "add@" => "java.util.Collections$UnmodifiableList.add(Collections.java:1512)",
        "remove@" => "java.util.Collections$UnmodifiableList.remove(Collections.java:1515)",
        "addAll@" => "java.util.Collections$UnmodifiableList.addAll(Collections.java:1520)",
        "replaceAll" => "java.util.Collections$UnmodifiableList.replaceAll(Collections.java:1525)",
        "sort" => "java.util.Collections$UnmodifiableList.sort(Collections.java:1529)",
        _ => return None,
    })
}

/// `IngestDocument.unmodifiableException()` thrown by the views of an ingest condition, with
/// the frames of the view method that threw (Elasticsearch 9.5.5's line numbers).
pub fn ingest_view_error(op: &str) -> Thrown {
    let m = |f: &str| format!("ingest.IngestDocument$UnmodifiableIngestData.{f}");
    let l = |f: &str| format!("ingest.IngestDocument$UnmodifiableIngestList.{f}");
    let it = |f: &str| format!("ingest.IngestDocument$UnmodifiableIngestList$UnmodifiableListIterator.{f}");
    let put = [m("put(IngestDocument.java:1685)"), m("put(IngestDocument.java:1650)")];
    let mut es: Vec<String> = vec!["ingest.IngestDocument.unmodifiableException(IngestDocument.java:1647)".into()];
    let mut jdk: Option<&str> = None;
    match op {
        // map operations
        "put" => es.extend(put.clone()),
        "remove" => es.push(m("remove(IngestDocument.java:1690)")),
        "putAll" => es.push(m("putAll(IngestDocument.java:1695)")),
        "clear" => es.push(m("clear(IngestDocument.java:1700)")),
        "setValue" => es.push("ingest.IngestDocument$UnmodifiableIngestData$1.setValue(IngestDocument.java:1728)".into()),
        "replaceAll" => {
            es.push("ingest.IngestDocument$UnmodifiableIngestData$1.setValue(IngestDocument.java:1728)".into());
            jdk = Some("java.util.Map.replaceAll(Map.java:793)");
        }
        "remove2" => {
            es.push(m("remove(IngestDocument.java:1690)"));
            jdk = Some("java.util.Map.remove(Map.java:891)");
        }
        "putIfAbsent" | "computeIfAbsent" | "computeIfPresent" | "compute" | "merge" | "replace" | "replace3" => {
            es.extend(put.clone());
            jdk = Some(match op {
                "putIfAbsent" => "java.util.Map.putIfAbsent(Map.java:845)",
                "computeIfAbsent" => "java.util.Map.computeIfAbsent(Map.java:1072)",
                "computeIfPresent" => "java.util.Map.computeIfPresent(Map.java:1149)",
                "compute" => "java.util.Map.compute(Map.java:1245)",
                "merge" => "java.util.Map.merge(Map.java:1334)",
                "replace" => "java.util.Map.replace(Map.java:987)",
                _ => "java.util.Map.replace(Map.java:942)",
            });
        }
        // list operations
        "add" => es.push(l("add(IngestDocument.java:1794)")),
        "removeObj" => es.push(l("remove(IngestDocument.java:1799)")),
        "addAll" => es.push(l("addAll(IngestDocument.java:1809)")),
        "addAll@" => es.push(l("addAll(IngestDocument.java:1814)")),
        "removeAll" => es.push(l("removeAll(IngestDocument.java:1819)")),
        "retainAll" => es.push(l("retainAll(IngestDocument.java:1824)")),
        "listClear" => es.push(l("clear(IngestDocument.java:1829)")),
        "set" => es.push(l("set(IngestDocument.java:1839)")),
        "add@" => es.push(l("add(IngestDocument.java:1844)")),
        "remove@" => es.push(l("remove(IngestDocument.java:1849)")),
        "removeIf" | "iteratorRemove" => {
            es.push("ingest.IngestDocument$UnmodifiableIterator.remove(IngestDocument.java:2024)".into());
            if op == "removeIf" {
                jdk = Some("java.util.Collection.removeIf(Collection.java:582)");
            }
        }
        "listReplaceAll" | "sort" | "iteratorSet" => {
            es.push(it("set(IngestDocument.java:1922)"));
            jdk = match op {
                "listReplaceAll" => Some("java.util.List.replaceAll(List.java:446)"),
                "sort" => Some("java.util.List.sort(List.java:516)"),
                _ => None,
            };
        }
        "iteratorAdd" => es.push(it("add(IngestDocument.java:1927)")),
        _ => {}
    }
    let mut t = Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, Some("Mutating ingest documents in conditionals is not supported".into()));
    let es: Vec<&str> = es.iter().map(String::as_str).collect();
    t.frames = crate::es::server_frames(&es);
    if let Some(j) = jdk {
        t.frames.push(format!("java.base/{j}"));
    }
    t
}

/// Fails a mutation through a view (`keySet()`, `values()`, `entrySet()`) of an immutable map:
/// `Collections.unmodifiableMap` and the ingest-condition views return unmodifiable
/// collections; `values()` of the latter is an `UnmodifiableIngestList`.
fn check_view_mutable(map: &JMap, kind: ViewKind, op: &str) -> Result<(), Thrown> {
    if map.mutability() != Mutability::Immutable {
        return Ok(());
    }
    if map.class == cls::INGEST_DATA && kind == ViewKind::Values {
        return Err(ingest_view_error(match op {
            "remove" => "removeObj",
            "clear" => "listClear",
            other => other,
        }));
    }
    match unmodifiable_frame(op).filter(|_| matches!(map.class, cls::UNMODIFIABLE_MAP | cls::INGEST_DATA)) {
        Some(f) => Err(uoe_frames(&[f])),
        None => Err(uoe()),
    }
}

fn unsupported_list(l: &JList, op: &str) -> Thrown {
    match l.class {
        cls::INGEST_LIST => ingest_view_error(match op {
            "remove" => "removeObj",
            "clear" => "listClear",
            "replaceAll" => "listReplaceAll",
            other => other,
        }),
        cls::UNMODIFIABLE_LIST => match unmodifiable_frame(op) {
            Some(f) => uoe_frames(&[f]),
            None => uoe(),
        },
        _ if op == "add" => uoe_frames(&["java.util.AbstractList.add(AbstractList.java:155)", "java.util.AbstractList.add(AbstractList.java:113)"]),
        _ => uoe(),
    }
}

fn nsee(frames: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::NO_SUCH_ELEMENT_EXCEPTION, None);
    t.frames = jframes(frames);
    t
}

// ------------------------------------------------------------------------------ views


fn as_map(v: &Value) -> Option<&Rc<JMap>> {
    match v {
        Value::Map(m) => Some(m),
        _ => None,
    }
}


/// Elements of anything iterable, or `None` if the value is not iterable.
pub fn elements_opt(vm: &mut Vm, v: &Value) -> Result<Option<Vec<Value>>, Thrown> {
    Ok(Some(match v {
        Value::List(l) => l.v.borrow().clone(),
        Value::Set(s) => s.keys(),
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind } => match kind {
                ViewKind::Keys => map.keys(),
                ViewKind::Values => map.values(),
                ViewKind::Entries => entry_values(map),
            },
            Obj::Es(e) => match e.iter_values(vm)? {
                Some(v) => v,
                None => return Ok(None),
            },
            Obj::Host(h) => match h.iter() {
                Some(v) => v,
                None => return Ok(None),
            },
            Obj::Iterator(it) => {
                let mut out = Vec::new();
                while let Some(x) = it.borrow_mut().next()? {
                    out.push(x);
                }
                out
            }
            _ => return Ok(None),
        },
        Value::Null => return Err(npe("Cannot invoke \"java.lang.Iterable.iterator()\" because \"receiver\" is null")),
        _ => return Ok(None),
    }))
}

/// Elements of an `Iterable`/`Collection`.
pub fn elements(vm: &mut Vm, v: &Value) -> Result<Vec<Value>, Thrown> {
    match elements_opt(vm, v)? {
        Some(x) => Ok(x),
        None => Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(v.class()), "java.lang.Iterable"))),
    }
}

/// `Collection.size()`.
pub fn size_of(vm: &mut Vm, v: &Value) -> Result<i32, Thrown> {
    Ok(match v {
        Value::List(l) => l.len() as i32,
        Value::Set(s) | Value::Map(s) => s.len() as i32,
        Value::Obj(o) => match &**o {
            Obj::MapView { map, .. } => map.len() as i32,
            Obj::Es(e) => e.size(vm)? as i32,
            _ => elements(vm, v)?.len() as i32,
        },
        Value::Null => return Err(npe("Cannot invoke \"java.util.Collection.size()\"")),
        _ => 0,
    })
}

/// Values of a Java array argument (`def[]` varargs-style parameters).
pub fn array_values(v: &Value) -> Vec<Value> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => a.data.borrow().to_values(),
            _ => vec![v.clone()],
        },
        Value::Null => Vec::new(),
        _ => vec![v.clone()],
    }
}

fn check_mutable(l: &JList, structural: bool, op: &str) -> Result<(), Thrown> {
    match l.mutability() {
        Mutability::Mutable => Ok(()),
        Mutability::FixedSize if !structural => Ok(()),
        _ => Err(unsupported_list(l, op)),
    }
}

fn check_map_mutable(m: &JMap) -> Result<(), Thrown> {
    check_map_op(m, "put")
}

/// Fails a mutation `op` of an immutable map with the JDK's frames.
fn check_map_op(m: &JMap, op: &str) -> Result<(), Thrown> {
    if m.mutability() == Mutability::Immutable {
        return Err(match m.class {
            cls::INGEST_DATA => ingest_view_error(op),
            cls::UNMODIFIABLE_MAP => {
                let line = match op {
                    "remove" => "remove(Collections.java:1669)",
                    "putAll" => "putAll(Collections.java:1672)",
                    "clear" => "clear(Collections.java:1675)",
                    "replaceAll" => "replaceAll(Collections.java:1719)",
                    "putIfAbsent" => "putIfAbsent(Collections.java:1724)",
                    "remove2" => "remove(Collections.java:1729)",
                    "replace3" => "replace(Collections.java:1734)",
                    "replace" => "replace(Collections.java:1739)",
                    "computeIfAbsent" => "computeIfAbsent(Collections.java:1744)",
                    "computeIfPresent" => "computeIfPresent(Collections.java:1750)",
                    "compute" => "compute(Collections.java:1756)",
                    "merge" => "merge(Collections.java:1762)",
                    _ => "put(Collections.java:1666)",
                };
                uoe_frames(&[&format!("java.util.Collections$UnmodifiableMap.{line}")])
            }
            _ => uoe_frames(&["java.util.AbstractMap.put(AbstractMap.java:215)"]),
        });
    }
    Ok(())
}

// ------------------------------------------------------------------------------ shared list/map helpers

/// `LinkedList`'s `checkElementIndex` failure from `method` (`get(LinkedList.java:488)`).
fn linked_index(i: i32, len: usize, method: &str) -> Thrown {
    Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index: {i}, Size: {len}")))
        .with_frames(&["java.base/java.util.LinkedList.checkElementIndex(LinkedList.java:567)", &format!("java.base/java.util.LinkedList.{method}")])
}

/// `ArrayList.get`'s out-of-bounds exception.
pub fn index_error(i: i32, len: usize) -> Thrown {
    ioobe(i as i64, len, "java.util.ArrayList.get(ArrayList.java:428)")
}

/// `List.get(int)` for any list-like receiver.
pub fn list_get(vm: &mut Vm, recv: &Value, i: i32) -> VR {
    match recv {
        Value::List(l) => {
            let v = l.v.borrow();
            if i < 0 || i as usize >= v.len() {
                if l.class == cls::LINKED_LIST {
                    let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index: {i}, Size: {}", v.len())));
                    t.frames = jframes(&["java.util.LinkedList.checkElementIndex(LinkedList.java:567)", "java.util.LinkedList.get(LinkedList.java:488)"]);
                    return Err(t);
                }
                let mut t = ioobe(i as i64, v.len(), "java.util.ArrayList.get(ArrayList.java:428)");
                if l.class == cls::INGEST_LIST {
                    t.frames.extend(crate::es::server_frames(&["ingest.IngestDocument$UnmodifiableIngestList.get(IngestDocument.java:1834)"]));
                }
                return Err(t);
            }
            Ok(v[i as usize].clone())
        }
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.list_get(vm, i),
            _ => {
                let items = elements(vm, recv)?;
                items.get(i as usize).cloned().ok_or_else(|| ioobe(i as i64, items.len(), "java.util.AbstractList.get(AbstractList.java:0)"))
            }
        },
        Value::Null => Err(npe("Cannot invoke \"java.util.List.get(int)\"")),
        _ => Err(crate::vm::cce("not a list")),
    }
}

/// `List.set(int, Object)`.
pub fn list_set(vm: &mut Vm, recv: &Value, i: i32, x: Value) -> VR {
    let _ = vm;
    match recv {
        Value::List(l) => {
            check_mutable(l, false, "set")?;
            let mut v = l.v.borrow_mut();
            if i < 0 || i as usize >= v.len() {
                if l.class == cls::LINKED_LIST {
                    return Err(linked_index(i, v.len(), "set(LinkedList.java:502)"));
                }
                return Err(ioobe(i as i64, v.len(), "java.util.ArrayList.set(ArrayList.java:471)"));
            }
            Ok(std::mem::replace(&mut v[i as usize], x))
        }
        Value::Null => Err(npe("Cannot invoke \"java.util.List.set(int, Object)\"")),
        _ => Err(uoe()),
    }
}

/// A `TreeMap` operation, for the JDK frames of its failures.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeOp {
    Get,
    ContainsKey,
    Remove,
    Put,
}

/// Appends the frames of `TreeMap` (and of the `TreeSet` method wrapping it) to a failure of
/// a key check or comparison at `inner` (TreeMap.java line).
fn tree_error(m: &JMap, mut e: Thrown, op: TreeOp, inner: &[&str]) -> Thrown {
    if e.offset.is_some() {
        return e;
    }
    let mut f: Vec<&str> = inner.to_vec();
    match op {
        TreeOp::Get => f.push("java.util.TreeMap.get(TreeMap.java:291)"),
        TreeOp::ContainsKey => f.push("java.util.TreeMap.containsKey(TreeMap.java:245)"),
        TreeOp::Remove => f.push("java.util.TreeMap.remove(TreeMap.java:915)"),
        TreeOp::Put => f.push("java.util.TreeMap.put(TreeMap.java:568)"),
    }
    if m.class == cls::TREE_SET {
        f.push(match op {
            TreeOp::Put => "java.util.TreeSet.add(TreeSet.java:261)",
            TreeOp::ContainsKey | TreeOp::Get => "java.util.TreeSet.contains(TreeSet.java:240)",
            TreeOp::Remove => "java.util.TreeSet.remove(TreeSet.java:282)",
        });
    }
    e.frames.extend(jframes(&f));
    e
}

/// Natural ordering's checks on a key before any comparison (`Objects.requireNonNull`, the cast
/// to `Comparable`), at `TreeMap.java` lines `null_line` / `cast_line` of `method`.
fn tree_key_check(m: &JMap, k: &Value, op: TreeOp, method: &str, null_line: u32, cast_line: u32) -> Result<(), Thrown> {
    if k.is_null() {
        let mut t = npe("");
        t.message = None;
        t.frames = jframes(&["java.util.Objects.requireNonNull(Objects.java:220)"]);
        return Err(tree_error(m, t, op, &[&format!("java.util.TreeMap.{method}(TreeMap.java:{null_line})")]));
    }
    let comparable = match k {
        Value::List(_) | Value::Map(_) | Value::Set(_) => false,
        Value::Obj(o) => !matches!(&**o, crate::value::Obj::Array(_) | crate::value::Obj::Function(_) | crate::value::Obj::Iterator(_) | crate::value::Obj::Pattern(_) | crate::value::Obj::Matcher(_)),
        _ => true,
    };
    if !comparable {
        let t = crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(k.class()), "java.lang.Comparable"));
        return Err(tree_error(m, t, op, &[&format!("java.util.TreeMap.{method}(TreeMap.java:{cast_line})")]));
    }
    Ok(())
}

/// Inserts into a sorted map/set at the comparator's position.
fn sorted_insert(vm: &mut Vm, m: &Rc<JMap>, k: Value, v: Value) -> Result<Option<Value>, Thrown> {
    let cmp = m.comparator.clone().unwrap_or(Value::Null);
    let n = m.len();
    if n == 0 {
        // `addEntryToEmptyMap` compares the key with itself (a type and null check)
        compare_with(vm, &cmp, &k, &k).map_err(|e| {
            tree_error(m, e, TreeOp::Put, &["java.util.TreeMap.compare(TreeMap.java:1605)", "java.util.TreeMap.addEntryToEmptyMap(TreeMap.java:810)", "java.util.TreeMap.put(TreeMap.java:819)"])
        })?;
        m.insert_at(0, k, v);
        return Ok(None);
    }
    if cmp.is_null() {
        tree_key_check(m, &k, TreeOp::Put, "put", 843, 845)?;
    }
    // binary search over the sorted keys; the comparator may call back into the script
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let key = m.data.borrow().m.get_index(mid).unwrap().0 .0.clone();
        let o = compare_with(vm, &cmp, &k, &key).map_err(|e| tree_error(m, e, TreeOp::Put, &["java.util.TreeMap.put(TreeMap.java:848)"]))?;
        match o {
            std::cmp::Ordering::Less => hi = mid,
            std::cmp::Ordering::Greater => lo = mid + 1,
            std::cmp::Ordering::Equal => {
                let mut d = m.data.borrow_mut();
                let (_, slot) = d.m.get_index_mut(mid).unwrap();
                return Ok(Some(std::mem::replace(slot, v)));
            }
        }
    }
    m.insert_at(lo, k, v);
    Ok(None)
}

/// Finds a key in a sorted map by comparator (TreeMap uses compareTo, not equals).
fn sorted_find(vm: &mut Vm, m: &Rc<JMap>, k: &Value, op: TreeOp) -> Result<Option<usize>, Thrown> {
    let cmp = m.comparator.clone().unwrap_or(Value::Null);
    if cmp.is_null() {
        tree_key_check(m, k, op, "getEntry", 381, 383)?;
    }
    let (mut lo, mut hi) = (0usize, m.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let key = m.data.borrow().m.get_index(mid).unwrap().0 .0.clone();
        let o = compare_with(vm, &cmp, k, &key).map_err(|e| tree_error(m, e, op, &["java.util.TreeMap.getEntry(TreeMap.java:386)"]))?;
        match o {
            std::cmp::Ordering::Less => hi = mid,
            std::cmp::Ordering::Greater => lo = mid + 1,
            std::cmp::Ordering::Equal => return Ok(Some(mid)),
        }
    }
    Ok(None)
}

/// `Map.get(Object)` for any map-like receiver.
pub fn map_get(vm: &mut Vm, recv: &Value, k: &Value) -> VR {
    match recv {
        Value::Map(m) => {
            if m.order() == MapOrder::Sorted {
                return Ok(match sorted_find(vm, m, k, TreeOp::Get)? {
                    Some(i) => m.data.borrow().m.get_index(i).unwrap().1.clone(),
                    None => Value::Null,
                });
            }
            match m.get(k) {
                Some(v) if m.lazy_host => crate::es::resolve_param(vm, v),
                Some(v) => Ok(v),
                None => Ok(Value::Null),
            }
        }
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.map_get(vm, k),
            _ => Err(crate::vm::cce("not a map")),
        },
        Value::Null => Err(npe("Cannot invoke \"java.util.Map.get(Object)\"")),
        _ => Err(crate::vm::cce("not a map")),
    }
}

/// `Map.put(Object, Object)`.
pub fn map_put(vm: &mut Vm, recv: &Value, k: Value, v: Value) -> VR {
    match recv {
        Value::Map(m) => {
            check_map_mutable(m)?;
            // an entry holds a key and a value
            crate::guard::check_elements(2 * (m.len() + 1))?;
            if m.order() == MapOrder::Sorted {
                return Ok(sorted_insert(vm, m, k, v)?.unwrap_or(Value::Null));
            }
            Ok(m.insert_raw(k, v).unwrap_or(Value::Null))
        }
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.map_put(vm, k, v),
            _ => Err(uoe()),
        },
        Value::Null => Err(npe("Cannot invoke \"java.util.Map.put(Object, Object)\"")),
        _ => Err(uoe()),
    }
}

fn map_contains(vm: &mut Vm, m: &Rc<JMap>, k: &Value) -> Result<bool, Thrown> {
    if m.order() == MapOrder::Sorted {
        return Ok(sorted_find(vm, m, k, TreeOp::ContainsKey)?.is_some());
    }
    Ok(m.contains(k))
}

fn map_remove_key(vm: &mut Vm, m: &Rc<JMap>, k: &Value) -> VR {
    if m.order() == MapOrder::Sorted {
        if let Some(i) = sorted_find(vm, m, k, TreeOp::Remove)? {
            let r = m.data.borrow_mut().m.shift_remove_index(i).map(|(_, v)| v);
            m.bump();
            return Ok(r.unwrap_or(Value::Null));
        }
        return Ok(Value::Null);
    }
    Ok(m.remove(k).unwrap_or(Value::Null))
}

/// Adds to a set (sorted sets keep order); returns whether it was added.
fn set_add(vm: &mut Vm, s: &Rc<JMap>, x: Value) -> Result<bool, Thrown> {
    if s.mutability() == Mutability::Immutable {
        if s.class == cls::UNMODIFIABLE_SET {
            return Err(uoe_frames(&["java.util.Collections$UnmodifiableCollection.add(Collections.java:1095)"]));
        }
        return Err(uoe());
    }
    if s.order() == MapOrder::Sorted {
        return Ok(sorted_insert(vm, s, x, Value::Null)?.is_none());
    }
    if s.contains(&x) {
        return Ok(false);
    }
    crate::guard::check_elements(2 * (s.len() + 1))?;
    s.insert_raw(x, Value::Null);
    Ok(true)
}

fn set_contains(vm: &mut Vm, s: &Rc<JMap>, x: &Value) -> Result<bool, Thrown> {
    map_contains(vm, s, x)
}

fn new_set(class: ClassId) -> Rc<JMap> {
    Rc::new(JMap::new(class))
}

fn bool_v(b: bool) -> VR {
    Ok(Value::Bool(b))
}

// ------------------------------------------------------------------------------ Iterable

fn it_for_each(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    // iterate live for lists so concurrent modification is detected like Java
    let mut it = crate::iter::iter_of(vm, &a[0])?;
    while let Some(x) = it.next()? {
        call(vm, &f, &[x])?;
    }
    Ok(Value::Null)
}
fn it_iterator(vm: &mut Vm, a: &mut [Value]) -> VR {
    crate::iter::iterator_of(vm, &a[0])
}
fn it_any(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    for x in elements(vm, &a[0])? {
        if call_bool(vm, &f, &[x])? {
            return bool_v(true);
        }
    }
    bool_v(false)
}
fn it_as_collection(vm: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::List(_) | Value::Set(_) => Ok(a[0].clone()),
        Value::Obj(o) if matches!(&**o, Obj::MapView { .. } | Obj::Es(_)) => Ok(a[0].clone()),
        v => Ok(JList::array_list(elements(vm, v)?)),
    }
}
fn it_as_list(vm: &mut Vm, a: &mut [Value]) -> VR {
    if crate::dynamic::is_list(vm.reg, &a[0]) {
        return Ok(a[0].clone());
    }
    Ok(JList::array_list(elements(vm, &a[0])?))
}
fn it_count(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let mut n = 0;
    for x in elements(vm, &a[0])? {
        if call_bool(vm, &f, &[x])? {
            n += 1;
        }
    }
    Ok(Value::Int(n))
}
fn it_each(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    for x in elements(vm, &a[0])? {
        call(vm, &f, &[x])?;
    }
    Ok(a[0].clone())
}
fn it_each_with_index(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    for (i, x) in elements(vm, &a[0])?.into_iter().enumerate() {
        call(vm, &f, &[x, Value::Int(i as i32)])?;
    }
    Ok(a[0].clone())
}
fn it_every(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    for x in elements(vm, &a[0])? {
        if !call_bool(vm, &f, &[x])? {
            return bool_v(false);
        }
    }
    bool_v(true)
}
fn it_find_results(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let mut out = Vec::new();
    for x in elements(vm, &a[0])? {
        let r = call(vm, &f, &[x])?;
        if !r.is_null() {
            out.push(r);
        }
    }
    Ok(JList::array_list(out))
}
fn it_group_by(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let m = Rc::new(JMap::linked());
    for x in elements(vm, &a[0])? {
        let k = call(vm, &f, std::slice::from_ref(&x))?;
        let list = match m.get(&k) {
            Some(l) => l,
            None => {
                let l = JList::array_list(Vec::new());
                m.insert_raw(k, l.clone());
                l
            }
        };
        if let Value::List(l) = list {
            l.v.borrow_mut().push(x);
            l.bump();
        }
    }
    Ok(Value::Map(m))
}
fn it_join(vm: &mut Vm, a: &mut [Value]) -> VR {
    let sep = a[1].to_java_string();
    let mut out = String::new();
    for (i, x) in elements(vm, &a[0])?.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        x.write_java_string(&mut out, 0);
    }
    Ok(Value::string(out))
}
fn it_sum(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut s = 0.0;
    for x in elements(vm, &a[0])? {
        match x.as_f64() {
            Some(d) if !matches!(x, Value::Char(_)) => s += d,
            _ => {
                return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(x.class()), "java.lang.Number")));
            }
        }
    }
    Ok(Value::Double(s))
}
fn it_sum_fn(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let mut s = 0.0;
    for x in elements(vm, &a[0])? {
        s += func::call_double(vm, &f, &[x])?;
    }
    Ok(Value::Double(s))
}

// ------------------------------------------------------------------------------ Collection

fn coll_add(vm: &mut Vm, a: &mut [Value]) -> VR {
    let x = std::mem::take(&mut a[1]);
    match &a[0] {
        Value::List(l) => {
            check_mutable(l, true, "add")?;
            if l.class == cls::PRIORITY_QUEUE {
                return pq_offer(vm, l, x).map_err(|e| pq_frames(e, true));
            }
            if x.is_null() && l.class == cls::ARRAY_DEQUE {
                return Err(deque_null(false, &["java.util.ArrayDeque.add(ArrayDeque.java:492)"]));
            }
            crate::guard::check_elements(l.len() + 1)?;
            l.v.borrow_mut().push(x);
            l.bump();
            bool_v(true)
        }
        Value::Set(s) => bool_v(set_add(vm, s, x)?),
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind } => {
                check_view_mutable(map, *kind, "add")?;
                Err(uoe())
            }
            _ => Err(uoe()),
        },
        _ => Err(uoe()),
    }
}

fn coll_add_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[1])?;
    match &a[0] {
        Value::List(l) => {
            check_mutable(l, true, "addAll")?;
            let changed = !items.is_empty();
            if l.class == cls::PRIORITY_QUEUE {
                for x in items {
                    pq_offer(vm, l, x)?;
                }
            } else {
                crate::guard::check_elements(l.len().saturating_add(items.len()))?;
                l.v.borrow_mut().extend(items);
                l.bump();
            }
            bool_v(changed)
        }
        Value::Set(s) => {
            let mut changed = false;
            for x in items {
                changed |= set_add(vm, s, x)?;
            }
            bool_v(changed)
        }
        _ => Err(uoe()),
    }
}

fn coll_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::List(l) => {
            check_mutable(l, true, "clear")?;
            l.v.borrow_mut().clear();
            l.bump();
        }
        Value::Set(s) | Value::Map(s) => {
            if s.mutability() == Mutability::Immutable {
                return Err(uoe());
            }
            s.clear();
        }
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind } => {
                check_view_mutable(map, *kind, "clear")?;
                map.clear()
            }
            _ => return Err(uoe()),
        },
        _ => return Err(uoe()),
    }
    Ok(Value::Null)
}

fn contains_value(vm: &mut Vm, recv: &Value, x: &Value) -> Result<bool, Thrown> {
    Ok(match recv {
        Value::List(l) => l.v.borrow().iter().any(|e| e.java_equals(x)),
        Value::Set(s) => set_contains(vm, s, x)?,
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind: ViewKind::Keys } => map_contains(vm, map, x)?,
            Obj::MapView { map, kind: ViewKind::Values } => map.values().iter().any(|e| e.java_equals(x)),
            _ => elements(vm, recv)?.iter().any(|e| e.java_equals(x)),
        },
        _ => elements(vm, recv)?.iter().any(|e| e.java_equals(x)),
    })
}

fn coll_contains(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, x) = (a[0].clone(), a[1].clone());
    bool_v(contains_value(vm, &r, &x)?)
}

fn coll_contains_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    // `UnmodifiableIngestList.containsAll` asks whether the list contains the collection itself
    if let Value::List(l) = &r
        && l.class == cls::INGEST_LIST
    {
        let x = a[1].clone();
        return bool_v(contains_value(vm, &r, &x)?);
    }
    for x in elements(vm, &a[1])? {
        if !contains_value(vm, &r, &x)? {
            return bool_v(false);
        }
    }
    bool_v(true)
}

fn coll_is_empty(vm: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(size_of(vm, &a[0])? == 0)
}

fn coll_size(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(size_of(vm, &a[0])?))
}

/// Removes every element matching `pred` from a mutable collection; returns whether any was.
fn remove_where(vm: &mut Vm, recv: &Value, op: &str, mut pred: impl FnMut(&mut Vm, &Value) -> Result<bool, Thrown>) -> Result<bool, Thrown> {
    match recv {
        Value::List(l) => {
            check_mutable(l, true, op)?;
            let items = l.v.borrow().clone();
            let mut keep = Vec::with_capacity(items.len());
            let mut changed = false;
            for x in items {
                if pred(vm, &x)? {
                    changed = true;
                } else {
                    keep.push(x);
                }
            }
            if changed {
                *l.v.borrow_mut() = keep;
                l.bump();
            }
            Ok(changed)
        }
        Value::Set(s) => {
            let mut changed = false;
            for k in s.keys() {
                if pred(vm, &k)? {
                    s.remove(&k);
                    changed = true;
                }
            }
            Ok(changed)
        }
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind } => {
                check_view_mutable(map, *kind, op)?;
                let mut changed = false;
                for (k, v) in map.entries() {
                    let item = match kind {
                        ViewKind::Keys => k.clone(),
                        ViewKind::Values => v.clone(),
                        ViewKind::Entries => Value::obj(Obj::Entry(JEntry { class: cls::HASH_MAP_NODE, key: k.clone(), value: RefCell::new(v.clone()), map: Some(map.clone()), ctx: None })),
                    };
                    if pred(vm, &item)? {
                        map.remove(&k);
                        changed = true;
                    }
                }
                Ok(changed)
            }
            _ => Err(uoe()),
        },
        _ => Err(uoe()),
    }
}

fn coll_remove_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let other = a[1].clone();
    let r = a[0].clone();
    bool_v(remove_where(vm, &r, "removeAll", |vm, x| contains_value(vm, &other, x))?)
}

fn coll_retain_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let other = a[1].clone();
    let r = a[0].clone();
    bool_v(remove_where(vm, &r, "retainAll", |vm, x| Ok(!contains_value(vm, &other, x)?))?)
}

fn coll_remove_if(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let r = a[0].clone();
    bool_v(remove_where(vm, &r, "removeIf", |vm, x| call_bool(vm, &f, std::slice::from_ref(x)))?)
}

fn coll_stream(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    Ok(crate::stream::stream_of(items))
}

fn coll_to_array(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::OBJECT, ArrayData::Ref(items)))))
}

fn coll_to_array2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    let elem = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => arr.elem,
            _ => Ty::OBJECT,
        },
        _ => return Err(npe("Cannot read the array length because \"a\" is null")),
    };
    for x in &items {
        if !x.is_null() && elem != Ty::OBJECT && elem != Ty::DEF && !vm.reg.instance_of(x, elem) {
            return Err(Throw::new(cls::ARRAY_STORE_EXCEPTION, Some("arraycopy: element type mismatch".to_string())));
        }
    }
    Ok(Value::obj(Obj::Array(JArray::from_data(elem, ArrayData::Ref(items)))))
}

fn coll_collect(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let mut out = Vec::new();
    for x in elements(vm, &a[0])? {
        out.push(call(vm, &f, &[x])?);
    }
    Ok(JList::array_list(out))
}

fn coll_collect2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (target, f) = (a[1].clone(), a[2].clone());
    for x in elements(vm, &a[0])? {
        let r = call(vm, &f, &[x])?;
        let mut args = [target.clone(), r];
        coll_add(vm, &mut args)?;
    }
    Ok(target)
}

fn coll_find(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    for x in elements(vm, &a[0])? {
        if call_bool(vm, &f, std::slice::from_ref(&x))? {
            return Ok(x);
        }
    }
    Ok(Value::Null)
}

fn coll_find_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let mut out = Vec::new();
    for x in elements(vm, &a[0])? {
        if call_bool(vm, &f, std::slice::from_ref(&x))? {
            out.push(x);
        }
    }
    Ok(JList::array_list(out))
}

fn coll_find_result(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    for x in elements(vm, &a[0])? {
        let r = call(vm, &f, &[x])?;
        if !r.is_null() {
            return Ok(r);
        }
    }
    Ok(Value::Null)
}

fn coll_find_result2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (def, f) = (a[1].clone(), a[2].clone());
    for x in elements(vm, &a[0])? {
        let r = call(vm, &f, &[x])?;
        if !r.is_null() {
            return Ok(r);
        }
    }
    Ok(def)
}

fn coll_split(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let (mut yes, mut no) = (Vec::new(), Vec::new());
    for x in elements(vm, &a[0])? {
        if call_bool(vm, &f, std::slice::from_ref(&x))? {
            yes.push(x);
        } else {
            no.push(x);
        }
    }
    Ok(JList::array_list(vec![JList::array_list(yes), JList::array_list(no)]))
}

// ------------------------------------------------------------------------------ List

fn list_add_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let x = std::mem::take(&mut a[2]);
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    check_mutable(l, true, "add@")?;
    let len = l.len();
    if i < 0 || i as usize > len {
        let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index: {i}, Size: {len}")));
        t.frames = if l.class == cls::LINKED_LIST {
            jframes(&["java.util.LinkedList.checkPositionIndex(LinkedList.java:572)", "java.util.LinkedList.add(LinkedList.java:519)"])
        } else {
            jframes(&["java.util.ArrayList.rangeCheckForAdd(ArrayList.java:847)", "java.util.ArrayList.add(ArrayList.java:511)"])
        };
        return Err(t);
    }
    crate::guard::check_elements(len + 1)?;
    l.v.borrow_mut().insert(i as usize, x);
    l.bump();
    Ok(Value::Null)
}

fn list_add_all_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let items = elements(vm, &a[2])?;
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    check_mutable(l, true, "addAll@")?;
    let len = l.len();
    if i < 0 || i as usize > len {
        let t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index: {i}, Size: {len}")));
        return Err(if l.class == cls::ARRAY_LIST {
            t.with_frames(&["java.base/java.util.ArrayList.rangeCheckForAdd(ArrayList.java:847)", "java.base/java.util.ArrayList.addAll(ArrayList.java:791)"])
        } else {
            t
        });
    }
    let changed = !items.is_empty();
    crate::guard::check_elements(len.saturating_add(items.len()))?;
    let mut v = l.v.borrow_mut();
    let tail: Vec<Value> = v.split_off(i as usize);
    v.extend(items);
    v.extend(tail);
    drop(v);
    l.bump();
    bool_v(changed)
}

fn list_get_n(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    list_get(vm, &r, int(&a[1]))
}

fn list_set_n(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    let x = std::mem::take(&mut a[2]);
    list_set(vm, &r, int(&a[1]), x)
}

fn list_index_of(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    Ok(Value::Int(items.iter().position(|x| x.java_equals(&a[1])).map(|i| i as i32).unwrap_or(-1)))
}

fn list_last_index_of(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    Ok(Value::Int(items.iter().rposition(|x| x.java_equals(&a[1])).map(|i| i as i32).unwrap_or(-1)))
}

fn list_remove_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    check_mutable(l, true, "remove@")?;
    let len = l.len();
    if i < 0 || i as usize >= len {
        if l.class == cls::LINKED_LIST {
            return Err(linked_index(i, len, "remove(LinkedList.java:537)"));
        }
        return Err(ioobe(i as i64, len, "java.util.ArrayList.remove(ArrayList.java:552)"));
    }
    let x = l.v.borrow_mut().remove(i as usize);
    l.bump();
    Ok(x)
}

fn list_replace_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    let l = l.clone();
    check_mutable(&l, false, "replaceAll")?;
    let n = l.len();
    for i in 0..n {
        let x = match l.get(i) {
            Some(x) => x,
            None => break,
        };
        let r = call(vm, &f, &[x])?;
        if let Some(slot) = l.v.borrow_mut().get_mut(i) {
            *slot = r;
        }
    }
    Ok(Value::Null)
}

fn list_get_length(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(size_of(vm, &a[0])?))
}

fn list_sort(vm: &mut Vm, a: &mut [Value]) -> VR {
    let cmp = a[1].clone();
    let Value::List(l) = &a[0] else {
        return Err(uoe());
    };
    let l = l.clone();
    check_mutable(&l, false, "sort")?;
    let mut items = l.v.borrow().clone();
    let natural = cmp.is_null();
    let r = func::sort_values(vm, &mut items, &cmp);
    // the JDK sorts the list's array in place: a failed sort leaves it partially sorted
    *l.v.borrow_mut() = items;
    if let Err(mut e) = r {
        if e.offset.is_none() {
            e.frames.extend(jframes(&[
                if natural { "java.util.Arrays.sort(Arrays.java:1104)" } else { "java.util.Arrays.sort(Arrays.java:1304)" },
            ]));
            if natural {
                e.frames.extend(jframes(&["java.util.Arrays.sort(Arrays.java:1298)"]));
            }
            if l.class == cls::ARRAY_LIST {
                e.frames.extend(jframes(&["java.util.ArrayList.sortRange(ArrayList.java:1825)", "java.util.ArrayList.sort(ArrayList.java:1818)"]));
            }
        }
        return Err(e);
    }
    l.bump();
    Ok(Value::Null)
}

fn list_sub_list(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (from, to) = (int(&a[1]), int(&a[2]));
    let items = elements(vm, &a[0])?;
    let n = items.len() as i32;
    if from < 0 {
        let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("fromIndex = {from}")));
        t.frames = jframes(&["java.util.AbstractList.subListRangeCheck(AbstractList.java:507)", "java.util.ArrayList.subList(ArrayList.java:1199)"]);
        return Err(t);
    }
    if to > n {
        let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("toIndex = {to}")));
        t.frames = jframes(&["java.util.AbstractList.subListRangeCheck(AbstractList.java:509)", "java.util.ArrayList.subList(ArrayList.java:1199)"]);
        return Err(t);
    }
    if from > to {
        let mut t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(format!("fromIndex({from}) > toIndex({to})")));
        t.frames = jframes(&["java.util.AbstractList.subListRangeCheck(AbstractList.java:511)", "java.util.ArrayList.subList(ArrayList.java:1199)"]);
        return Err(t);
    }
    // a sub list of an ingest-condition view is one too
    let class = if matches!(&a[0], Value::List(l) if l.class == cls::INGEST_LIST) { cls::INGEST_LIST } else { cls::SUB_LIST };
    let window = items[from as usize..to as usize].to_vec();
    if let Value::List(parent) = &a[0]
        && matches!(parent.class, cls::ARRAY_LIST | cls::LINKED_LIST | cls::SUB_LIST | cls::ABSTRACT_SUB_LIST)
    {
        // a live view: writes go through to the parent, the parent's structural changes
        // invalidate it
        let class = if matches!(parent.class, cls::ARRAY_LIST | cls::SUB_LIST) { cls::SUB_LIST } else { cls::ABSTRACT_SUB_LIST };
        let mut l = JList::new(class, window);
        l.view = Some(Box::new(crate::value::SubView {
            parent: parent.clone(),
            offset: from as usize,
            len: std::cell::Cell::new((to - from) as usize),
            expected: std::cell::Cell::new(parent.mods.get()),
        }));
        return Ok(Value::List(Rc::new(l)));
    }
    Ok(Value::List(Rc::new(JList::new(class, window))))
}

/// The frames between `checkForComodification` and the script when `method` meets a view
/// whose parent changed structurally.
fn view_cme(array_list: bool, call: &ViewCall) -> Thrown {
    let method = call.name;
    let mut t = Throw::new(cls::CONCURRENT_MODIFICATION_EXCEPTION, None);
    let (owner, check) = if array_list { ("ArrayList$SubList", "ArrayList.java:1506") } else { ("AbstractList$SubList", "AbstractList.java:906") };
    let mut frames = vec![format!("java.util.{owner}.checkForComodification({check})")];
    let at = |m: &str, line: u32| format!("java.util.{owner}.{m}({}:{line})", if array_list { "ArrayList.java" } else { "AbstractList.java" });
    let iterator = |f: &mut Vec<String>| {
        f.push(at("listIterator", 1375));
        f.push("java.util.AbstractList.listIterator(AbstractList.java:313)".into());
        f.push(at("iterator", 1371));
    };
    match method {
        "size" => frames.push(at("size", if array_list { 1246 } else { 792 })),
        "get" => frames.push(at("get", if array_list { 1241 } else { 787 })),
        "set" => frames.push(at("set", 1233)),
        "add" => {
            frames.push(at("size", if array_list { 1246 } else { 792 }));
            frames.push("java.util.AbstractList.add(AbstractList.java:113)".into());
        }
        "remove" => frames.push(at("remove", 1259)),
        "isEmpty" => {
            frames.push(at("size", if array_list { 1246 } else { 792 }));
            frames.push("java.util.AbstractCollection.isEmpty(AbstractCollection.java:91)".into());
        }
        "contains" => {
            frames.push(at("indexOf", 1356));
            frames.push(at("contains", 1367));
        }
        "containsAll" => {
            frames.push(at("indexOf", 1356));
            frames.push(at("contains", 1367));
            frames.push("java.util.AbstractCollection.containsAll(AbstractCollection.java:311)".into());
        }
        "indexOf" => frames.push(at("indexOf", 1356)),
        "lastIndexOf" => frames.push(at("lastIndexOf", 1362)),
        "iterator" => iterator(&mut frames),
        "listIterator" => {
            frames.push(at("listIterator", 1375));
            if call.arity == 0 {
                frames.push("java.util.AbstractList.listIterator(AbstractList.java:313)".into());
            }
        }
        "toString" => {
            iterator(&mut frames);
            frames.push("java.util.AbstractCollection.toString(AbstractCollection.java:451)".into());
        }
        "hashCode" => frames.push(at("hashCode", 1350)),
        "equals" => frames.push(at("equals", 1344)),
        "addAll" => {
            frames.push(at("addAll", 1280));
            if call.arity == 1 {
                frames.push(at("addAll", 1272));
            }
        }
        "removeAll" => {
            frames.push(at("batchRemove", 1299));
            frames.push(at("removeAll", 1291));
        }
        "retainAll" => {
            frames.push(at("batchRemove", 1299));
            frames.push(at("retainAll", 1295));
        }
        "removeIf" => frames.push(at("removeIf", 1309)),
        "toArray" => frames.push(at("toArray", 1318)),
        "spliterator" => frames.push(at("spliterator", 1519)),
        "stream" => {
            frames.push(at("spliterator", 1519));
            frames.push("java.util.Collection.stream(Collection.java:748)".into());
        }
        "sort" => frames.push(at("sort", 1596)),
        _ => {}
    }
    // library methods that received the view as an argument
    match (call.owner, method) {
        (cls::COLLECTIONS, "sort") => frames.push("java.util.Collections.sort(Collections.java:147)".into()),
        (cls::STRING, "valueOf") => {
            iterator(&mut frames);
            frames.push("java.util.AbstractCollection.toString(AbstractCollection.java:451)".into());
            frames.push("java.lang.String.valueOf(String.java:4905)".into());
        }
        (cls::ARRAY_LIST, "<init>") => {
            frames.push(at("toArray", 1318));
            frames.push("java.util.ArrayList.<init>(ArrayList.java:182)".into());
        }
        _ => {}
    }
    t.frames = frames.into_iter().map(|f| if f.starts_with("java.base/") { f } else { format!("java.base/{f}") }).collect();
    t
}

/// The library call a `subList` view takes part in (for the frames of its failures).
pub struct ViewCall<'a> {
    pub owner: crate::types::ClassId,
    pub name: &'a str,
    pub arity: usize,
}

/// Before a library call on a `subList` view: fail as `checkForComodification` does when the
/// parent changed structurally, else take the parent's current values for the window.
pub fn view_enter(l: &JList, call: &ViewCall) -> Result<(), Thrown> {
    let Some(view) = &l.view else { return Ok(()) };
    // `subList` of a view range-checks against the view's own size without the check
    if call.name == "subList" {
        return Ok(());
    }
    if view.parent.view.is_some() {
        view_enter(&view.parent, call)?;
    }
    if view.parent.mods.get() != view.expected.get() {
        return Err(view_cme(l.class == cls::SUB_LIST, call));
    }
    let p = view.parent.v.borrow();
    let end = (view.offset + view.len.get()).min(p.len());
    let fresh = p.get(view.offset..end).map(|s| s.to_vec()).unwrap_or_default();
    drop(p);
    *l.v.borrow_mut() = fresh;
    Ok(())
}

/// After a library call on a `subList` view: write the window back into the parent (a size
/// change is a structural change of the parent, as in Java).
pub fn view_exit(l: &JList) {
    let Some(view) = &l.view else { return };
    if view.parent.mods.get() != view.expected.get() {
        return;
    }
    let items = l.v.borrow().clone();
    let old = view.len.get();
    {
        let mut p = view.parent.v.borrow_mut();
        let end = (view.offset + old).min(p.len());
        if view.offset <= end {
            p.splice(view.offset..end, items.iter().cloned());
        }
    }
    if items.len() != old {
        view.parent.bump();
        view.len.set(items.len());
    }
    view.expected.set(view.parent.mods.get());
    if view.parent.view.is_some() {
        view_exit(&view.parent);
    }
}

/// `List.listIterator()`: a `ListIterator` (`set`, `add`, `previous` reach it dynamically too).
fn list_iterator(vm: &mut Vm, a: &mut [Value]) -> VR {
    let it = crate::iter::iterator_of(vm, &a[0])?;
    if let (Value::List(_), Value::Obj(o)) = (&a[0], &it)
        && let Obj::Iterator(i) = &**o
    {
        i.borrow_mut().class = cls::LIST_ITR;
    }
    Ok(it)
}

fn list_iterator_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let len = size_of(vm, &a[0])?;
    if i < 0 || i > len {
        let t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index: {i}, Size: {len}")));
        return Err(match &a[0] {
            Value::List(l) if l.class == cls::ARRAY_LIST => {
                t.with_frames(&["java.base/java.util.ArrayList.rangeCheckForAdd(ArrayList.java:847)", "java.base/java.util.ArrayList.listIterator(ArrayList.java:1014)"])
            }
            _ => t,
        });
    }
    let it = list_iterator(vm, a)?;
    if let Value::Obj(o) = &it
        && let Obj::Iterator(i) = &**o {
            i.borrow_mut().pos = int(&a[1]).max(0) as usize;
        }
    Ok(it)
}

fn list_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(a[0].java_equals(&a[1]))
}

fn list_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(a[0].java_hash()))
}

/// `getByPath` (Elasticsearch augmentation) over nested maps and lists.
fn get_by_path(vm: &mut Vm, recv: &Value, path: &str, default: Option<Value>) -> VR {
    if path.is_empty() {
        return Err(iae("Missing path"));
    }
    if path.ends_with('.') {
        return Err(iae(format!("Trailing '.' in path [{path}]")));
    }
    let elems: Vec<&str> = path.split('.').collect();
    let joined = elems.join(".");
    let mut cur = recv.clone();
    let mut i = 0;
    loop {
        if i > elems.len() - 1 {
            return Ok(cur);
        }
        if elems[i].is_empty() {
            return Err(iae(format!("Extra '.' in path [{joined}] at index [{i}]")));
        }
        let reg = vm.reg;
        let missing = |cur: &Value, i: usize| -> VR {
            if crate::dynamic::is_list(reg, cur) || crate::dynamic::is_map(reg, cur) {
                if elems.len() - 1 == i {
                    return match &default {
                        Some(d) => Ok(d.clone()),
                        None => Err(iae(format!("Could not find value at path [{path}]"))),
                    };
                }
                return Err(iae(format!("Container does not have [{}], for non-terminal index [{i}] in path [{joined}]", elems[i])));
            }
            Err(iae(format!("Non-container [{}] at [{}], index [{i}] in path [{joined}]", crate::lookup::java_binary_name(cur.class()), elems[i])))
        };
        if crate::dynamic::is_map(reg, &cur) {
            let key = Value::str(elems[i]);
            let has = match &cur {
                Value::Map(m) => map_contains(vm, m, &key)?,
                Value::Obj(o) => match &**o {
                    Obj::Es(e) => e.map_contains(vm, &key)?,
                    _ => false,
                },
                _ => false,
            };
            if has {
                cur = map_get(vm, &cur, &key)?;
                i += 1;
                continue;
            }
            return missing(&cur, i);
        }
        if crate::dynamic::is_list(reg, &cur) {
            let idx: i32 = match elems[i].parse() {
                Ok(v) => v,
                Err(_) => {
                    return Err(iae(format!("Could not parse [{}] as a int index into list at path [{joined}] and index [{i}]", elems[i])));
                }
            };
            let n = size_of(vm, &cur)?;
            if n >= idx {
                cur = list_get(vm, &cur, idx)?;
                i += 1;
                continue;
            }
            return missing(&cur, i);
        }
        if cur.is_null() {
            return Err(npe("Cannot invoke \"Object.getClass()\" because \"obj\" is null"));
        }
        return missing(&cur, i);
    }
}

fn get_by_path1(vm: &mut Vm, a: &mut [Value]) -> VR {
    let p = string(&a[1], "length()")?.clone();
    let r = a[0].clone();
    get_by_path(vm, &r, p.as_str(), None)
}

fn get_by_path2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let p = string(&a[1], "length()")?.clone();
    let r = a[0].clone();
    get_by_path(vm, &r, p.as_str(), Some(a[2].clone()))
}

// --- ArrayList / LinkedList / Vector / Stack / ArrayDeque / PriorityQueue constructors

fn list_new(class: ClassId) -> Value {
    Value::List(Rc::new(JList::new(class, Vec::new())))
}
fn arraylist_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(list_new(cls::ARRAY_LIST))
}
fn arraylist_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(npe("Cannot invoke \"java.util.Collection.toArray()\" because \"c\" is null"));
    }
    Ok(Value::List(Rc::new(JList::new(cls::ARRAY_LIST, elements(vm, &a[0])?))))
}
fn linkedlist_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(list_new(cls::LINKED_LIST))
}
fn linkedlist_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::List(Rc::new(JList::new(cls::LINKED_LIST, elements(vm, &a[0])?))))
}
fn vector_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(list_new(cls::VECTOR))
}
fn vector_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::List(Rc::new(JList::new(cls::VECTOR, elements(vm, &a[0])?))))
}
fn stack_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(list_new(cls::STACK))
}
fn deque_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(list_new(cls::ARRAY_DEQUE))
}
fn deque_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::List(Rc::new(JList::new(cls::ARRAY_DEQUE, elements(vm, &a[0])?))))
}
fn pq_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(list_new(cls::PRIORITY_QUEUE))
}
fn pq_new_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = Rc::new(JList::new(cls::PRIORITY_QUEUE, Vec::new()));
    PQ_CMP.with(|m| m.borrow_mut().push((Rc::downgrade(&l), a[0].clone())));
    Ok(Value::List(l))
}

thread_local! {
    /// Comparators of `PriorityQueue`s created with one (weak keys, pruned lazily).
    static PQ_CMP: RefCell<Vec<(std::rc::Weak<JList>, Value)>> = const { RefCell::new(Vec::new()) };
}

fn pq_comparator(l: &Rc<JList>) -> Value {
    PQ_CMP.with(|m| {
        let mut m = m.borrow_mut();
        m.retain(|(w, _)| w.strong_count() > 0);
        m.iter().find(|(w, _)| w.upgrade().is_some_and(|x| Rc::ptr_eq(&x, l))).map(|(_, c)| c.clone()).unwrap_or(Value::Null)
    })
}

/// `PriorityQueue.offer`: keeps the backing vector a binary heap like the JDK.
fn pq_offer(vm: &mut Vm, l: &Rc<JList>, x: Value) -> VR {
    if x.is_null() {
        return Err(npe("Cannot invoke \"Object.getClass()\""));
    }
    let cmp = pq_comparator(l);
    let mut items = l.v.borrow().clone();
    items.push(x);
    let mut k = items.len() - 1;
    while k > 0 {
        let parent = (k - 1) / 2;
        if compare_with(vm, &cmp, &items[k], &items[parent])? != std::cmp::Ordering::Less {
            break;
        }
        items.swap(k, parent);
        k = parent;
    }
    *l.v.borrow_mut() = items;
    l.bump();
    bool_v(true)
}

fn pq_poll(vm: &mut Vm, l: &Rc<JList>) -> VR {
    let cmp = pq_comparator(l);
    let mut items = l.v.borrow().clone();
    if items.is_empty() {
        return Ok(Value::Null);
    }
    let top = items.swap_remove(0);
    let n = items.len();
    let mut k = 0;
    loop {
        let (lc, rc) = (2 * k + 1, 2 * k + 2);
        let mut m = k;
        if lc < n && compare_with(vm, &cmp, &items[lc], &items[m])? == std::cmp::Ordering::Less {
            m = lc;
        }
        if rc < n && compare_with(vm, &cmp, &items[rc], &items[m])? == std::cmp::Ordering::Less {
            m = rc;
        }
        if m == k {
            break;
        }
        items.swap(k, m);
        k = m;
    }
    *l.v.borrow_mut() = items;
    l.bump();
    Ok(top)
}

fn list_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::List(l) => Ok(Value::List(Rc::new(JList::new(l.class, l.v.borrow().clone())))),
        Value::Map(m) | Value::Set(m) => {
            let n = JMap {
                class: m.class,
                data: RefCell::new(crate::value::MapData { m: m.data.borrow().m.clone(), cap: m.data.borrow().cap, init_cap: m.data.borrow().init_cap }),
                mods: std::cell::Cell::new(0),
                comparator: m.comparator.clone(),
                lazy_host: false,
            };
            Ok(if matches!(a[0], Value::Map(_)) { Value::Map(Rc::new(n)) } else { Value::Set(Rc::new(n)) })
        }
        v => Ok(v.clone()),
    }
}

fn noop(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Null)
}

// --- Deque / Queue (ArrayDeque, LinkedList)

fn dq(a: &[Value]) -> Result<Rc<JList>, Thrown> {
    match &a[0] {
        Value::List(l) => Ok(l.clone()),
        _ => Err(uoe()),
    }
}

/// `ArrayDeque` refuses null elements with a message-less NPE from `addFirst`/`addLast`;
/// `via` are the JDK frames of the public method the script called.
fn deque_null(first: bool, via: &[&str]) -> Thrown {
    let mut t = npe("");
    t.message = None;
    let mut f = vec![if first { "java.util.ArrayDeque.addFirst(ArrayDeque.java:284)" } else { "java.util.ArrayDeque.addLast(ArrayDeque.java:301)" }];
    f.extend_from_slice(via);
    t.frames = jframes(&f);
    t
}

/// The frames of an empty `Deque`'s `NoSuchElementException`: the innermost `ArrayDeque` /
/// `LinkedList` method, then `via` (the calling public method, per class).
fn deque_empty(l: &JList, op: &str) -> Thrown {
    let ad = l.class == cls::ARRAY_DEQUE;
    let f: &[&str] = match (op, ad) {
        ("getFirst", true) => &["java.util.ArrayDeque.getFirst(ArrayDeque.java:400)"],
        ("getLast", true) => &["java.util.ArrayDeque.getLast(ArrayDeque.java:411)"],
        ("removeFirst", true) => &["java.util.ArrayDeque.removeFirst(ArrayDeque.java:360)"],
        ("removeLast", true) => &["java.util.ArrayDeque.removeLast(ArrayDeque.java:370)"],
        ("pop", true) => &["java.util.ArrayDeque.removeFirst(ArrayDeque.java:360)", "java.util.ArrayDeque.pop(ArrayDeque.java:591)"],
        ("element", true) => &["java.util.ArrayDeque.getFirst(ArrayDeque.java:400)", "java.util.ArrayDeque.element(ArrayDeque.java:549)"],
        ("remove", true) => &["java.util.ArrayDeque.removeFirst(ArrayDeque.java:360)", "java.util.ArrayDeque.remove(ArrayDeque.java:521)"],
        ("getFirst", false) => &["java.util.LinkedList.getFirst(LinkedList.java:256)"],
        ("getLast", false) => &["java.util.LinkedList.getLast(LinkedList.java:269)"],
        ("removeFirst", false) => &["java.util.LinkedList.removeFirst(LinkedList.java:282)"],
        ("removeLast", false) => &["java.util.LinkedList.removeLast(LinkedList.java:295)"],
        ("pop", false) => &["java.util.LinkedList.removeFirst(LinkedList.java:282)", "java.util.LinkedList.pop(LinkedList.java:813)"],
        ("element", false) => &["java.util.LinkedList.getFirst(LinkedList.java:256)", "java.util.LinkedList.element(LinkedList.java:675)"],
        ("remove", false) => &["java.util.LinkedList.removeFirst(LinkedList.java:282)", "java.util.LinkedList.remove(LinkedList.java:697)"],
        _ => &[],
    };
    if l.class == cls::PRIORITY_QUEUE {
        return match op {
            "element" => nsee(&["java.util.AbstractQueue.element(AbstractQueue.java:136)"]),
            "remove" => nsee(&["java.util.AbstractQueue.remove(AbstractQueue.java:117)"]),
            _ => nsee(&[]),
        };
    }
    if l.class == cls::ARRAY_DEQUE || l.class == cls::LINKED_LIST {
        nsee(f)
    } else {
        nsee(&[])
    }
}

fn deque_add_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if a[1].is_null() && l.class == cls::ARRAY_DEQUE {
        return Err(deque_null(true, &[]));
    }
    l.v.borrow_mut().insert(0, a[1].clone());
    l.bump();
    Ok(Value::Null)
}
fn deque_add_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if a[1].is_null() && l.class == cls::ARRAY_DEQUE {
        return Err(deque_null(false, &[]));
    }
    l.v.borrow_mut().push(a[1].clone());
    l.bump();
    Ok(Value::Null)
}
fn deque_offer_first(vm: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() && dq(a)?.class == cls::ARRAY_DEQUE {
        return Err(deque_null(true, &["java.util.ArrayDeque.offerFirst(ArrayDeque.java:338)"]));
    }
    deque_add_first(vm, a)?;
    bool_v(true)
}
fn deque_offer_last(vm: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() && dq(a)?.class == cls::ARRAY_DEQUE {
        return Err(deque_null(false, &["java.util.ArrayDeque.offerLast(ArrayDeque.java:350)"]));
    }
    deque_add_last(vm, a)?;
    bool_v(true)
}
fn deque_get_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let r = l.get(0);
    r.ok_or_else(|| deque_empty(&l, "getFirst"))
}
fn deque_get_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let r = l.v.borrow().last().cloned();
    r.ok_or_else(|| deque_empty(&l, "getLast"))
}
fn deque_peek_first(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.class == cls::PRIORITY_QUEUE {
        let _ = vm;
    }
    Ok(l.get(0).unwrap_or(Value::Null))
}
fn deque_peek_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let r = l.v.borrow().last().cloned().unwrap_or(Value::Null);
    Ok(r)
}
fn deque_poll_first(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.class == cls::PRIORITY_QUEUE {
        return pq_poll(vm, &l);
    }
    if l.is_empty() {
        return Ok(Value::Null);
    }
    let r = l.v.borrow_mut().remove(0);
    l.bump();
    Ok(r)
}
fn deque_poll_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let r = l.v.borrow_mut().pop();
    l.bump();
    Ok(r.unwrap_or(Value::Null))
}
fn deque_remove_first(vm: &mut Vm, a: &mut [Value]) -> VR {
    deque_take_first(vm, a, "removeFirst")
}
fn deque_take_first(vm: &mut Vm, a: &mut [Value], op: &str) -> VR {
    let l = dq(a)?;
    if l.is_empty() {
        return Err(deque_empty(&l, op));
    }
    deque_poll_first(vm, a)
}
fn deque_remove_last(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.is_empty() {
        return Err(deque_empty(&l, "removeLast"));
    }
    deque_poll_last(vm, a)
}
fn deque_push(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.class == cls::STACK || l.class == cls::VECTOR {
        l.v.borrow_mut().push(a[1].clone());
        l.bump();
        return Ok(a[1].clone());
    }
    if a[1].is_null() && l.class == cls::ARRAY_DEQUE {
        return Err(deque_null(true, &["java.util.ArrayDeque.push(ArrayDeque.java:577)"]));
    }
    deque_add_first(vm, a)
}
fn empty_stack(frames: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::EMPTY_STACK_EXCEPTION, None);
    t.frames = jframes(frames);
    t
}
fn deque_pop(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.class == cls::STACK {
        if l.is_empty() {
            return Err(empty_stack(&["java.util.Stack.peek(Stack.java:103)", "java.util.Stack.pop(Stack.java:85)"]));
        }
        let r = l.v.borrow_mut().pop();
        l.bump();
        return Ok(r.unwrap_or(Value::Null));
    }
    deque_take_first(vm, a, "pop")
}
fn deque_remove_obj(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let pos = l.v.borrow().iter().position(|x| x.java_equals(&a[1]));
    match pos {
        Some(i) => {
            l.v.borrow_mut().remove(i);
            l.bump();
            bool_v(true)
        }
        None => bool_v(false),
    }
}
fn deque_remove_last_occ(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let pos = l.v.borrow().iter().rposition(|x| x.java_equals(&a[1]));
    match pos {
        Some(i) => {
            l.v.borrow_mut().remove(i);
            l.bump();
            bool_v(true)
        }
        None => bool_v(false),
    }
}
fn deque_descending_iterator(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let mut items = l.v.borrow().clone();
    items.reverse();
    Ok(Value::obj(Obj::Iterator(RefCell::new(JIter::of_values(cls::GENERIC_ITERATOR, items)))))
}
fn queue_element(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.is_empty() {
        return Err(deque_empty(&l, "element"));
    }
    deque_peek_first(vm, a)
}
fn queue_offer(vm: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    if l.class == cls::PRIORITY_QUEUE {
        return pq_offer(vm, &l, a[1].clone()).map_err(|e| pq_frames(e, false));
    }
    if a[1].is_null() && l.class == cls::ARRAY_DEQUE {
        return Err(deque_null(false, &["java.util.ArrayDeque.offerLast(ArrayDeque.java:350)", "java.util.ArrayDeque.offer(ArrayDeque.java:506)"]));
    }
    deque_offer_last(vm, a)
}
fn queue_remove(vm: &mut Vm, a: &mut [Value]) -> VR {
    deque_take_first(vm, a, "remove")
}
fn stack_peek(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let r = l.v.borrow().last().cloned();
    r.ok_or_else(|| empty_stack(&["java.util.Stack.peek(Stack.java:103)"]))
}

/// Frames of a failing `PriorityQueue.offer` (a null element, or a comparison failing in
/// `siftUp`), called directly or through `add`.
fn pq_frames(mut e: Thrown, via_add: bool) -> Thrown {
    if e.class == cls::NULL_POINTER_EXCEPTION && e.frames.is_empty() {
        e.message = None;
        e.frames = jframes(&["java.util.PriorityQueue.offer(PriorityQueue.java:325)"]);
    } else if e.class == cls::CLASS_CAST_EXCEPTION {
        e.frames.extend(jframes(&[
            "java.util.PriorityQueue.siftUpComparable(PriorityQueue.java:647)",
            "java.util.PriorityQueue.siftUp(PriorityQueue.java:639)",
            "java.util.PriorityQueue.offer(PriorityQueue.java:330)",
        ]));
    }
    if via_add {
        e.frames.extend(jframes(&["java.util.PriorityQueue.add(PriorityQueue.java:311)"]));
    }
    e
}
fn stack_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(dq(a)?.is_empty())
}
fn stack_search(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let v = l.v.borrow();
    Ok(Value::Int(match v.iter().rposition(|x| x.java_equals(&a[1])) {
        Some(i) => (v.len() - i) as i32,
        None => -1,
    }))
}
fn vector_element_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    list_get_n(vm, a)
}
fn vector_first(_: &mut Vm, a: &mut [Value]) -> VR {
    dq(a)?.get(0).ok_or_else(|| nsee(&[]))
}
fn vector_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let r = l.v.borrow().last().cloned();
    r.ok_or_else(|| nsee(&[]))
}
fn vector_insert_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [a[0].clone(), a[2].clone(), a[1].clone()];
    list_add_at(vm, &mut args)
}
fn vector_set_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [a[0].clone(), a[2].clone(), a[1].clone()];
    list_set_n(vm, &mut args)?;
    Ok(Value::Null)
}
fn vector_remove_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    list_remove_at(vm, a)?;
    Ok(Value::Null)
}
fn vector_remove_element(vm: &mut Vm, a: &mut [Value]) -> VR {
    deque_remove_obj(vm, a)
}
fn vector_add_element(vm: &mut Vm, a: &mut [Value]) -> VR {
    coll_add(vm, a)?;
    Ok(Value::Null)
}
fn vector_last_index_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = dq(a)?;
    let from = int(&a[2]);
    let v = l.v.borrow();
    let upto = (from as i64).min(v.len() as i64 - 1);
    let mut i = upto;
    while i >= 0 {
        if v[i as usize].java_equals(&a[1]) {
            return Ok(Value::Int(i as i32));
        }
        i -= 1;
    }
    Ok(Value::Int(-1))
}

// ------------------------------------------------------------------------------ Set

fn set_remove(vm: &mut Vm, a: &mut [Value]) -> VR {
    let x = a[1].clone();
    match &a[0] {
        Value::Set(s) => {
            if s.mutability() == Mutability::Immutable {
                return Err(uoe());
            }
            if s.order() == MapOrder::Sorted {
                let had = map_remove_key(vm, s, &x)?;
                let _ = had;
                return bool_v(true);
            }
            bool_v(s.remove(&x).is_some())
        }
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind: ViewKind::Keys } => {
                check_view_mutable(map, ViewKind::Keys, "remove")?;
                let had = map.contains(&x);
                map.remove(&x);
                bool_v(had)
            }
            Obj::MapView { map, kind: ViewKind::Entries } => {
                check_view_mutable(map, ViewKind::Entries, "remove")?;
                if let Value::Obj(eo) = &x
                    && let Obj::Entry(e) = &**eo
                        && map.get(&e.key).is_some_and(|v| v.java_equals(&e.value.borrow())) {
                            map.remove(&e.key);
                            return bool_v(true);
                        }
                bool_v(false)
            }
            _ => Err(uoe()),
        },
        Value::List(_) => deque_remove_obj(vm, a),
        _ => Err(uoe()),
    }
}

fn set_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(a[0].java_equals(&a[1]))
}

fn hashset_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Set(new_set(cls::HASH_SET)))
}
fn hashset_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    let s = new_set(cls::HASH_SET);
    s.presize_for_copy(items.len(), true);
    for x in items {
        if !s.contains(&x) {
            s.insert_raw(x, Value::Null);
        }
    }
    Ok(Value::Set(s))
}
fn linkedhashset_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Set(new_set(cls::LINKED_HASH_SET)))
}
fn linkedhashset_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    let s = new_set(cls::LINKED_HASH_SET);
    for x in items {
        if !s.contains(&x) {
            s.insert_raw(x, Value::Null);
        }
    }
    Ok(Value::Set(s))
}
fn treeset_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Set(new_set(cls::TREE_SET)))
}
fn treeset_new_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut m = JMap::new(cls::TREE_SET);
    m.comparator = if a[0].is_null() { None } else { Some(a[0].clone()) };
    Ok(Value::Set(Rc::new(m)))
}

fn sorted_keys(v: &Value) -> Result<(Rc<JMap>, Vec<Value>), Thrown> {
    match v {
        Value::Set(s) | Value::Map(s) => Ok((s.clone(), s.keys())),
        _ => Err(uoe()),
    }
}

/// `TreeSet.first()`/`last()` and `TreeMap.firstKey()`/`lastKey()` of an empty tree.
fn tree_empty(m: &JMap, first: bool) -> Thrown {
    let mut f = vec!["java.util.TreeMap.key(TreeMap.java:1638)", if first { "java.util.TreeMap.firstKey(TreeMap.java:303)" } else { "java.util.TreeMap.lastKey(TreeMap.java:310)" }];
    if m.class == cls::TREE_SET {
        f.push(if first { "java.util.TreeSet.first(TreeSet.java:397)" } else { "java.util.TreeSet.last(TreeSet.java:404)" });
    }
    if m.class == cls::TREE_SET || m.class == cls::TREE_MAP { nsee(&f) } else { nsee(&[]) }
}
fn sset_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    k.first().cloned().ok_or_else(|| tree_empty(&m, true))
}
fn sset_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    k.last().cloned().ok_or_else(|| tree_empty(&m, false))
}

/// Index of the first key `>= x` (or `> x` when `strict`) in a sorted map.
fn lower_bound(vm: &mut Vm, m: &Rc<JMap>, keys: &[Value], x: &Value, strict: bool) -> Result<usize, Thrown> {
    let cmp = m.comparator.clone().unwrap_or(Value::Null);
    let (mut lo, mut hi) = (0, keys.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let o = compare_with(vm, &cmp, &keys[mid], x)?;
        let go_right = if strict { o != std::cmp::Ordering::Greater } else { o == std::cmp::Ordering::Less };
        if go_right {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn sset_ceiling(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    let i = lower_bound(vm, &m, &k, &a[1], false)?;
    Ok(k.get(i).cloned().unwrap_or(Value::Null))
}
fn sset_higher(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    let i = lower_bound(vm, &m, &k, &a[1], true)?;
    Ok(k.get(i).cloned().unwrap_or(Value::Null))
}
fn sset_floor(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    let i = lower_bound(vm, &m, &k, &a[1], true)?;
    Ok(if i == 0 { Value::Null } else { k[i - 1].clone() })
}
fn sset_lower(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    let i = lower_bound(vm, &m, &k, &a[1], false)?;
    Ok(if i == 0 { Value::Null } else { k[i - 1].clone() })
}
fn sset_poll_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    match k.first() {
        Some(x) => {
            m.remove(x);
            Ok(x.clone())
        }
        None => Ok(Value::Null),
    }
}
fn sset_poll_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, k) = sorted_keys(&a[0])?;
    match k.last() {
        Some(x) => {
            m.remove(x);
            Ok(x.clone())
        }
        None => Ok(Value::Null),
    }
}
fn sorted_subset(vm: &mut Vm, recv: &Value, lo: Option<(&Value, bool)>, hi: Option<(&Value, bool)>) -> VR {
    let (m, keys) = sorted_keys(recv)?;
    let start = match lo {
        Some((x, incl)) => lower_bound(vm, &m, &keys, x, !incl)?,
        None => 0,
    };
    let end = match hi {
        Some((x, incl)) => lower_bound(vm, &m, &keys, x, incl)?,
        None => keys.len(),
    };
    let mut out = JMap::new(m.class);
    out.comparator = m.comparator.clone();
    let is_set = matches!(recv, Value::Set(_));
    {
        let d = m.data.borrow();
        for i in start..end.max(start) {
            let (k, v) = d.m.get_index(i).unwrap();
            out.data.borrow_mut().m.insert(k.clone(), v.clone());
        }
    }
    Ok(if is_set { Value::Set(Rc::new(out)) } else { Value::Map(Rc::new(out)) })
}
fn sset_head(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    sorted_subset(vm, &r, None, Some((&a[1], false)))
}
fn sset_head2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    sorted_subset(vm, &r, None, Some((&a[1], crate::lib_lang::boolean(&a[2]))))
}
fn sset_tail(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    sorted_subset(vm, &r, Some((&a[1], true)), None)
}
fn sset_tail2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    sorted_subset(vm, &r, Some((&a[1], crate::lib_lang::boolean(&a[2]))), None)
}
fn sset_sub(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    sorted_subset(vm, &r, Some((&a[1], true)), Some((&a[2], false)))
}
fn sset_sub4(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    sorted_subset(vm, &r, Some((&a[1], crate::lib_lang::boolean(&a[2]))), Some((&a[3], crate::lib_lang::boolean(&a[4]))))
}
fn sset_descending(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, mut keys) = sorted_keys(&a[0])?;
    keys.reverse();
    let out = Rc::new(JMap::new(cls::LINKED_HASH_SET));
    for k in keys {
        out.insert_raw(k, Value::Null);
    }
    let _ = m;
    Ok(Value::Set(out))
}
fn sset_descending_iterator(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, mut keys) = sorted_keys(&a[0])?;
    keys.reverse();
    Ok(Value::obj(Obj::Iterator(RefCell::new(JIter::of_values(cls::GENERIC_ITERATOR, keys)))))
}
fn sorted_comparator(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(match &a[0] {
        Value::Set(s) | Value::Map(s) => s.comparator.clone().unwrap_or(Value::Null),
        _ => Value::Null,
    })
}

// ------------------------------------------------------------------------------ Map

fn mm(a: &[Value]) -> Result<Rc<JMap>, Thrown> {
    match &a[0] {
        Value::Map(m) => Ok(m.clone()),
        _ => Err(uoe()),
    }
}

/// Entries of any map-like receiver.
pub fn map_entries(vm: &mut Vm, v: &Value) -> Result<Vec<(Value, Value)>, Thrown> {
    match v {
        Value::Map(m) => Ok(m.entries()),
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.map_entries(vm),
            _ => Err(crate::vm::cce("not a map")),
        },
        Value::Null => Err(npe("Cannot invoke \"java.util.Map.entrySet()\"")),
        _ => Err(crate::vm::cce("not a map")),
    }
}

fn map_clear(vm: &mut Vm, a: &mut [Value]) -> VR {
    if let Value::Map(m) = &a[0] {
        check_map_op(m, "clear")?;
    }
    coll_clear(vm, a)
}
fn map_compute(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "compute")?;
    let (k, f) = (a[1].clone(), a[2].clone());
    let old = m.get(&k).unwrap_or(Value::Null);
    let nv = call(vm, &f, &[k.clone(), old])?;
    if nv.is_null() {
        map_remove_key(vm, &m, &k)?;
    } else {
        map_put(vm, &a[0].clone(), k, nv.clone())?;
    }
    Ok(nv)
}
fn map_compute_if_absent(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "computeIfAbsent")?;
    let (k, f) = (a[1].clone(), a[2].clone());
    if let Some(v) = m.get(&k)
        && !v.is_null() {
            return Ok(v);
        }
    let nv = call(vm, &f, std::slice::from_ref(&k))?;
    if !nv.is_null() {
        map_put(vm, &a[0].clone(), k, nv.clone())?;
    }
    Ok(nv)
}
fn map_compute_if_present(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "computeIfPresent")?;
    let (k, f) = (a[1].clone(), a[2].clone());
    match m.get(&k) {
        Some(old) if !old.is_null() => {
            let nv = call(vm, &f, &[k.clone(), old])?;
            if nv.is_null() {
                map_remove_key(vm, &m, &k)?;
            } else {
                map_put(vm, &a[0].clone(), k, nv.clone())?;
            }
            Ok(nv)
        }
        _ => Ok(Value::Null),
    }
}
fn map_contains_key(vm: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Map(m) => {
            let m = m.clone();
            bool_v(map_contains(vm, &m, &a[1])?)
        }
        Value::Obj(o) => match &**o {
            Obj::Es(e) => bool_v(e.map_contains(vm, &a[1])?),
            _ => bool_v(false),
        },
        _ => bool_v(false),
    }
}
fn map_contains_value(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    Ok(Value::Bool(map_entries(vm, &r)?.iter().any(|(_, v)| v.java_equals(&a[1]))))
}
fn map_entry_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    if m.class == cls::INGEST_DATA {
        // `UnmodifiableIngestData.entrySet()` collects read-only entries into a new `HashSet`
        let s = new_set(cls::HASH_SET);
        for e in crate::iter::entry_values(&m) {
            s.insert_raw(e, Value::Null);
        }
        return Ok(Value::Set(s));
    }
    Ok(Value::obj(Obj::MapView { map: m, kind: ViewKind::Entries }))
}
fn map_key_set(vm: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Map(m) => Ok(Value::obj(Obj::MapView { map: m.clone(), kind: ViewKind::Keys })),
        v => {
            let v = v.clone();
            let keys: Vec<Value> = map_entries(vm, &v)?.into_iter().map(|(k, _)| k).collect();
            let s = new_set(cls::LINKED_HASH_SET);
            for k in keys {
                s.insert_raw(k, Value::Null);
            }
            Ok(Value::Set(s))
        }
    }
}
fn map_values(vm: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        // `UnmodifiableIngestData.values()` copies them into an `UnmodifiableIngestList`
        Value::Map(m) if m.class == cls::INGEST_DATA => Ok(Value::List(Rc::new(JList::new(cls::INGEST_LIST, m.values())))),
        Value::Map(m) => Ok(Value::obj(Obj::MapView { map: m.clone(), kind: ViewKind::Values })),
        v => {
            let v = v.clone();
            Ok(JList::array_list(map_entries(vm, &v)?.into_iter().map(|(_, x)| x).collect()))
        }
    }
}
fn map_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(a[0].java_equals(&a[1]))
}
fn map_for_each(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    let r = a[0].clone();
    let mods = match &r {
        Value::Map(m) => Some((m.clone(), m.mods.get())),
        _ => None,
    };
    for (k, v) in map_entries(vm, &r)? {
        call(vm, &f, &[k, v])?;
    }
    if let Some((m, before)) = mods
        && m.mods.get() != before {
            return Err(Throw::new(cls::CONCURRENT_MODIFICATION_EXCEPTION, None));
        }
    Ok(Value::Null)
}
fn map_get_n(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    map_get(vm, &r, &a[1])
}
fn map_get_or_default(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    let has = match &r {
        Value::Map(m) => map_contains(vm, m, &a[1])?,
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.map_contains(vm, &a[1])?,
            _ => false,
        },
        _ => false,
    };
    if has {
        map_get(vm, &r, &a[1])
    } else {
        Ok(a[2].clone())
    }
}
fn map_is_empty(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    bool_v(map_entries(vm, &r)?.is_empty())
}
fn map_merge(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "merge")?;
    let (k, v, f) = (a[1].clone(), a[2].clone(), a[3].clone());
    if v.is_null() {
        return Err(npe("Cannot invoke \"Object.hashCode()\" because \"value\" is null"));
    }
    let nv = match m.get(&k) {
        Some(old) if !old.is_null() => call(vm, &f, &[old, v])?,
        _ => v,
    };
    if nv.is_null() {
        map_remove_key(vm, &m, &k)?;
    } else {
        map_put(vm, &a[0].clone(), k, nv.clone())?;
    }
    Ok(nv)
}
fn map_put_n(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    let (k, v) = (std::mem::take(&mut a[1]), std::mem::take(&mut a[2]));
    map_put(vm, &r, k, v)
}
fn map_put_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    if let Value::Map(m) = &a[0] {
        check_map_op(m, "putAll")?;
    }
    let r = a[0].clone();
    let src = a[1].clone();
    let entries = map_entries(vm, &src)?;
    if let Value::Map(m) = &r {
        // HashMap.putAll grows the table up front
        let mut d = m.data.borrow_mut();
        let s = entries.len() as u64;
        if d.cap > 0 {
            while s * 4 > d.cap as u64 * 3 && d.cap < (1 << 30) {
                d.cap *= 2;
            }
        } else if s > 0 {
            let ft = (s as f32 / 0.75) + 1.0;
            d.init_cap = d.init_cap.max(crate::value::table_size_for(ft as u32));
        }
    }
    for (k, v) in entries {
        map_put(vm, &r, k, v)?;
    }
    Ok(Value::Null)
}
fn map_put_if_absent(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    // the ingest-condition view inherits `Map.putIfAbsent`, which only fails when it would put
    if m.class != cls::INGEST_DATA {
        check_map_op(&m, "putIfAbsent")?;
    }
    match m.get(&a[1]) {
        Some(v) if !v.is_null() => Ok(v),
        _ if m.class == cls::INGEST_DATA => Err(ingest_view_error("putIfAbsent")),
        _ => {
            let r = a[0].clone();
            map_put(vm, &r, a[1].clone(), a[2].clone())?;
            Ok(Value::Null)
        }
    }
}
fn map_remove(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "remove")?;
    map_remove_key(vm, &m, &a[1])
}
fn map_remove2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "remove2")?;
    match m.get(&a[1]) {
        Some(v) if v.java_equals(&a[2]) && (m.contains(&a[1])) => {
            map_remove_key(vm, &m, &a[1])?;
            bool_v(true)
        }
        _ => bool_v(false),
    }
}
fn map_replace(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "replace")?;
    if m.contains(&a[1]) {
        let r = a[0].clone();
        return map_put(vm, &r, a[1].clone(), a[2].clone());
    }
    Ok(Value::Null)
}
fn map_replace3(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    if m.class != cls::INGEST_DATA {
        check_map_op(&m, "replace3")?;
    }
    match m.get(&a[1]) {
        Some(v) if v.java_equals(&a[2]) && m.contains(&a[1]) && m.class == cls::INGEST_DATA => Err(ingest_view_error("replace3")),
        Some(v) if v.java_equals(&a[2]) && m.contains(&a[1]) => {
            let r = a[0].clone();
            map_put(vm, &r, a[1].clone(), a[3].clone())?;
            bool_v(true)
        }
        _ => bool_v(false),
    }
}
fn map_replace_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    check_map_op(&m, "replaceAll")?;
    let f = a[1].clone();
    for (k, v) in m.entries() {
        let nv = call(vm, &f, &[k.clone(), v])?;
        if let Some(slot) = m.data.borrow_mut().m.get_mut(&crate::value::Key(k)) {
            *slot = nv;
        }
    }
    Ok(Value::Null)
}
fn map_size(vm: &mut Vm, a: &mut [Value]) -> VR {
    let r = a[0].clone();
    Ok(Value::Int(match &r {
        Value::Map(m) => m.len() as i32,
        _ => map_entries(vm, &r)?.len() as i32,
    }))
}
fn map_collect(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    let mut out = Vec::new();
    for (k, v) in map_entries(vm, &r)? {
        out.push(call(vm, &f, &[k, v])?);
    }
    Ok(JList::array_list(out))
}
fn map_collect2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, target, f) = (a[0].clone(), a[1].clone(), a[2].clone());
    for (k, v) in map_entries(vm, &r)? {
        let x = call(vm, &f, &[k, v])?;
        coll_add(vm, &mut [target.clone(), x])?;
    }
    Ok(target)
}
fn map_count(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    let mut n = 0;
    for (k, v) in map_entries(vm, &r)? {
        if call_bool(vm, &f, &[k, v])? {
            n += 1;
        }
    }
    Ok(Value::Int(n))
}
fn map_each(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    for (k, v) in map_entries(vm, &r)? {
        call(vm, &f, &[k, v])?;
    }
    Ok(r)
}
fn map_every(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    for (k, v) in map_entries(vm, &r)? {
        if !call_bool(vm, &f, &[k, v])? {
            return bool_v(false);
        }
    }
    bool_v(true)
}
fn entry_value(m: Option<Rc<JMap>>, k: Value, v: Value) -> Value {
    Value::obj(Obj::Entry(JEntry { class: cls::HASH_MAP_NODE, key: k, value: RefCell::new(v), map: m, ctx: None }))
}
fn map_find(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    let m = as_map(&r).cloned();
    for (k, v) in map_entries(vm, &r)? {
        if call_bool(vm, &f, &[k.clone(), v.clone()])? {
            return Ok(entry_value(m, k, v));
        }
    }
    Ok(Value::Null)
}
fn map_find_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    let tree = matches!(&r, Value::Map(m) if m.class == cls::TREE_MAP);
    let out = Rc::new(JMap::new(if tree { cls::TREE_MAP } else { cls::LINKED_HASH_MAP }));
    for (k, v) in map_entries(vm, &r)? {
        if call_bool(vm, &f, &[k.clone(), v.clone()])? {
            out.insert_raw(k, v);
        }
    }
    Ok(Value::Map(out))
}
fn map_find_result(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    for (k, v) in map_entries(vm, &r)? {
        let x = call(vm, &f, &[k, v])?;
        if !x.is_null() {
            return Ok(x);
        }
    }
    Ok(Value::Null)
}
fn map_find_result2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, d, f) = (a[0].clone(), a[1].clone(), a[2].clone());
    for (k, v) in map_entries(vm, &r)? {
        let x = call(vm, &f, &[k, v])?;
        if !x.is_null() {
            return Ok(x);
        }
    }
    Ok(d)
}
fn map_find_results(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    let mut out = Vec::new();
    for (k, v) in map_entries(vm, &r)? {
        let x = call(vm, &f, &[k, v])?;
        if !x.is_null() {
            out.push(x);
        }
    }
    Ok(JList::array_list(out))
}
fn map_group_by(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (r, f) = (a[0].clone(), a[1].clone());
    let tree = matches!(&r, Value::Map(m) if m.class == cls::TREE_MAP);
    let out = Rc::new(JMap::linked());
    for (k, v) in map_entries(vm, &r)? {
        let g = call(vm, &f, &[k.clone(), v.clone()])?;
        let inner = match out.get(&g) {
            Some(x) => x,
            None => {
                let x = Value::Map(Rc::new(JMap::new(if tree { cls::TREE_MAP } else { cls::LINKED_HASH_MAP })));
                out.insert_raw(g, x.clone());
                x
            }
        };
        map_put(vm, &inner, k, v)?;
    }
    Ok(Value::Map(out))
}

fn hashmap_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::hash_map())))
}
fn hashmap_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    let src = a[0].clone();
    let entries = map_entries(vm, &src)?;
    let m = JMap::hash_map();
    m.presize_for_copy(entries.len(), false);
    for (k, v) in entries {
        m.insert_raw(k, v);
    }
    Ok(Value::Map(Rc::new(m)))
}
fn linkedhashmap_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::linked())))
}
fn linkedhashmap_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    let src = a[0].clone();
    let m = JMap::linked();
    for (k, v) in map_entries(vm, &src)? {
        m.insert_raw(k, v);
    }
    Ok(Value::Map(Rc::new(m)))
}
fn treemap_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::new(cls::TREE_MAP))))
}
fn treemap_new_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut m = JMap::new(cls::TREE_MAP);
    m.comparator = if a[0].is_null() { None } else { Some(a[0].clone()) };
    Ok(Value::Map(Rc::new(m)))
}
fn hashtable_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::new(cls::HASHTABLE))))
}
fn hashtable_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    let src = a[0].clone();
    let m = JMap::new(cls::HASHTABLE);
    for (k, v) in map_entries(vm, &src)? {
        m.insert_raw(k, v);
    }
    Ok(Value::Map(Rc::new(m)))
}
fn identity_map_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::new(cls::IDENTITY_HASH_MAP))))
}

// --- SortedMap / NavigableMap

fn smap_first_key(vm: &mut Vm, a: &mut [Value]) -> VR {
    sset_first(vm, a)
}
fn smap_last_key(vm: &mut Vm, a: &mut [Value]) -> VR {
    sset_last(vm, a)
}
fn smap_entry_at(m: &Rc<JMap>, i: Option<usize>) -> Value {
    match i {
        Some(i) => match m.data.borrow().m.get_index(i) {
            Some((k, v)) => entry_value(None, k.0.clone(), v.clone()),
            None => Value::Null,
        },
        None => Value::Null,
    }
}
fn smap_first_entry(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    Ok(smap_entry_at(&m, if m.is_empty() { None } else { Some(0) }))
}
fn smap_last_entry(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    let n = m.len();
    Ok(smap_entry_at(&m, if n == 0 { None } else { Some(n - 1) }))
}
fn smap_poll_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    let r = smap_entry_at(&m, if m.is_empty() { None } else { Some(0) });
    if !m.is_empty() {
        m.data.borrow_mut().m.shift_remove_index(0);
        m.bump();
    }
    Ok(r)
}
fn smap_poll_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    let n = m.len();
    let r = smap_entry_at(&m, if n == 0 { None } else { Some(n - 1) });
    if n > 0 {
        m.data.borrow_mut().m.shift_remove_index(n - 1);
        m.bump();
    }
    Ok(r)
}
fn smap_nav(vm: &mut Vm, a: &mut [Value], op: &str, entry: bool) -> VR {
    let m = mm(a)?;
    let keys = m.keys();
    let x = a[1].clone();
    let idx = match op {
        "ceiling" => Some(lower_bound(vm, &m, &keys, &x, false)?).filter(|&i| i < keys.len()),
        "higher" => Some(lower_bound(vm, &m, &keys, &x, true)?).filter(|&i| i < keys.len()),
        "floor" => lower_bound(vm, &m, &keys, &x, true)?.checked_sub(1),
        _ => lower_bound(vm, &m, &keys, &x, false)?.checked_sub(1),
    };
    if entry {
        Ok(smap_entry_at(&m, idx))
    } else {
        Ok(idx.map(|i| keys[i].clone()).unwrap_or(Value::Null))
    }
}
fn smap_ceiling_key(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "ceiling", false)
}
fn smap_ceiling_entry(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "ceiling", true)
}
fn smap_floor_key(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "floor", false)
}
fn smap_floor_entry(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "floor", true)
}
fn smap_higher_key(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "higher", false)
}
fn smap_higher_entry(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "higher", true)
}
fn smap_lower_entry(vm: &mut Vm, a: &mut [Value]) -> VR {
    smap_nav(vm, a, "lower", true)
}
fn smap_descending_map(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    let mut entries = m.entries();
    entries.reverse();
    let out = Rc::new(JMap::linked());
    for (k, v) in entries {
        out.insert_raw(k, v);
    }
    Ok(Value::Map(out))
}
fn smap_key_set_view(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = mm(a)?;
    Ok(Value::obj(Obj::MapView { map: m, kind: ViewKind::Keys }))
}
fn smap_descending_key_set(vm: &mut Vm, a: &mut [Value]) -> VR {
    sset_descending(vm, a)
}

// --- Map.Entry

fn entry_get_key(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Entry(e) => Ok(e.key.clone()),
            _ => Ok(Value::Null),
        },
        _ => Ok(Value::Null),
    }
}
fn entry_get_value(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Entry(e) => {
                if let Some(m) = &e.map
                    && let Some(v) = m.get(&e.key) {
                        return Ok(v);
                    }
                Ok(e.value.borrow().clone())
            }
            _ => Ok(Value::Null),
        },
        _ => Ok(Value::Null),
    }
}
fn entry_set_value(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Entry(e) => {
                if e.class == cls::SIMPLE_IMMUTABLE_ENTRY {
                    return Err(uoe());
                }
                if let Some(m) = &e.map
                    && m.class == cls::INGEST_DATA
                {
                    return Err(ingest_view_error("setValue"));
                }
                if let Some(c) = &e.ctx {
                    // `CtxMap.EntrySet` entries validate through the ctx
                    let old = c.entry_set_value(&e.key, a[1].clone())?;
                    *e.value.borrow_mut() = a[1].clone();
                    return Ok(old);
                }
                let old = std::mem::replace(&mut *e.value.borrow_mut(), a[1].clone());
                if let Some(m) = &e.map
                    && let Some(slot) = m.data.borrow_mut().m.get_mut(&crate::value::Key(e.key.clone())) {
                        *slot = a[1].clone();
                    }
                Ok(old)
            }
            _ => Ok(Value::Null),
        },
        _ => Ok(Value::Null),
    }
}
fn entry_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(a[0].java_equals(&a[1]))
}
fn entry_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(a[0].java_hash()))
}
fn entry_key_of(v: &Value) -> Value {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Entry(e) => e.key.clone(),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}
fn entry_value_of(v: &Value) -> Value {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Entry(e) => e.value.borrow().clone(),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}
fn entry_comparing_by_key(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(func::native(Some(cls::COMPARATOR), |_, a| {
        Ok(Value::Int(func::natural_compare(&entry_key_of(&a[0]), &entry_key_of(&a[1]))? as i32))
    }))
}
fn entry_comparing_by_key_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = a[0].clone();
    Ok(func::native(Some(cls::COMPARATOR), move |vm, a| {
        Ok(Value::Int(compare_with(vm, &c, &entry_key_of(&a[0]), &entry_key_of(&a[1]))? as i32))
    }))
}
fn entry_comparing_by_value(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(func::native(Some(cls::COMPARATOR), |_, a| {
        Ok(Value::Int(func::natural_compare(&entry_value_of(&a[0]), &entry_value_of(&a[1]))? as i32))
    }))
}
fn entry_comparing_by_value_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = a[0].clone();
    Ok(func::native(Some(cls::COMPARATOR), move |vm, a| {
        Ok(Value::Int(compare_with(vm, &c, &entry_value_of(&a[0]), &entry_value_of(&a[1]))? as i32))
    }))
}
fn simple_entry_new(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Entry(JEntry { class: cls::SIMPLE_ENTRY, key: a[0].clone(), value: RefCell::new(a[1].clone()), map: None, ctx: None })))
}
fn simple_entry_from(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Entry(JEntry { class: cls::SIMPLE_ENTRY, key: entry_key_of(&a[0]), value: RefCell::new(entry_value_of(&a[0])), map: None, ctx: None })))
}
fn simple_immutable_entry_new(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Entry(JEntry { class: cls::SIMPLE_IMMUTABLE_ENTRY, key: a[0].clone(), value: RefCell::new(a[1].clone()), map: None, ctx: None })))
}
fn simple_immutable_entry_from(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Entry(JEntry {
        class: cls::SIMPLE_IMMUTABLE_ENTRY,
        key: entry_key_of(&a[0]),
        value: RefCell::new(entry_value_of(&a[0])),
        map: None,
        ctx: None,
    })))
}

// ------------------------------------------------------------------------------ Iterator

fn with_iter<R>(v: &Value, f: impl FnOnce(&mut JIter) -> R) -> Option<R> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Iterator(i) => Some(f(&mut i.borrow_mut())),
            _ => None,
        },
        _ => None,
    }
}

fn iter_has_next(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(with_iter(&a[0], |i| i.has_next()).unwrap_or(false))
}
fn iter_next(_: &mut Vm, a: &mut [Value]) -> VR {
    match with_iter(&a[0], |i| i.next().map(|v| v.ok_or_else(|| i.exhausted()))) {
        Some(r) => r?,
        None => Err(nsee(&[])),
    }
}
fn iter_remove(_: &mut Vm, a: &mut [Value]) -> VR {
    match with_iter(&a[0], |i| i.remove()) {
        Some(r) => {
            r?;
            Ok(Value::Null)
        }
        None => Err(uoe()),
    }
}
fn iter_for_each_remaining(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    loop {
        let next = with_iter(&a[0], |i| i.next()).unwrap_or(Ok(None))?;
        match next {
            Some(x) => {
                call(vm, &f, &[x])?;
            }
            None => return Ok(Value::Null),
        }
    }
}
fn iter_has_previous(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(with_iter(&a[0], |i| i.pos > 0).unwrap_or(false))
}
fn iter_next_index(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(with_iter(&a[0], |i| i.pos as i32).unwrap_or(0)))
}
fn iter_previous_index(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(with_iter(&a[0], |i| i.pos as i32 - 1).unwrap_or(-1)))
}
fn iter_add(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = a[1].clone();
    let r = with_iter(&a[0], |i| {
        if let Source::List(l) = &i.source {
            if l.mutability() != Mutability::Mutable {
                return Err(match l.class {
                    cls::INGEST_LIST => ingest_view_error("iteratorAdd"),
                    cls::UNMODIFIABLE_LIST => uoe_frames(&["java.util.Collections$UnmodifiableList$1.add(Collections.java:1553)"]),
                    _ => uoe(),
                });
            }
            l.v.borrow_mut().insert(i.pos, x);
            l.bump();
            i.mods = l.mods.get();
            i.pos += 1;
            i.can_remove = false;
        }
        Ok(())
    });
    if let Some(r) = r {
        r?;
    }
    Ok(Value::Null)
}
fn iter_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = a[1].clone();
    let r = with_iter(&a[0], |i| {
        if !i.can_remove {
            let t = Throw::new(cls::ILLEGAL_STATE_EXCEPTION, None);
            return Err(match &i.source {
                Source::List(l) if l.class == cls::ARRAY_LIST => t.with_frames(&["java.base/java.util.ArrayList$ListItr.set(ArrayList.java:1144)"]),
                _ => t,
            });
        }
        if let Source::List(l) = &i.source {
            if l.mutability() == Mutability::Immutable {
                return Err(match l.class {
                    cls::INGEST_LIST => ingest_view_error("iteratorSet"),
                    cls::UNMODIFIABLE_LIST => uoe_frames(&["java.util.Collections$UnmodifiableList$1.set(Collections.java:1550)"]),
                    _ => uoe(),
                });
            }
            if let Some(slot) = l.v.borrow_mut().get_mut(i.pos - 1) {
                *slot = x;
            }
        }
        Ok(())
    });
    if let Some(r) = r {
        r?;
    }
    Ok(Value::Null)
}

// ------------------------------------------------------------------------------ Collections / Arrays / Objects

fn immutable_list(items: Vec<Value>, class: ClassId) -> Value {
    Value::List(Rc::new(JList::new(class, items)))
}

fn col_empty_list(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(immutable_list(Vec::new(), cls::EMPTY_LIST))
}
fn col_empty_map(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::new(cls::EMPTY_MAP))))
}
fn col_empty_set(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Set(Rc::new(JMap::new(cls::EMPTY_SET))))
}
fn col_empty_iterator(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Iterator(RefCell::new(JIter::of_values(cls::GENERIC_ITERATOR, Vec::new())))))
}
fn col_add_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    let target = a[0].clone();
    let mut changed = false;
    for x in array_values(&a[1]) {
        let r = coll_add(vm, &mut [target.clone(), x])?;
        changed |= matches!(r, Value::Bool(true));
    }
    bool_v(changed)
}
fn col_binary_search(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    let key = a[1].clone();
    let cmp = a.get(2).cloned().unwrap_or(Value::Null);
    let (mut lo, mut hi) = (0i64, items.len() as i64 - 1);
    while lo <= hi {
        let mid = (lo + hi) >> 1;
        match compare_with(vm, &cmp, &items[mid as usize], &key)? {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid - 1,
            std::cmp::Ordering::Equal => return Ok(Value::Int(mid as i32)),
        }
    }
    Ok(Value::Int(-(lo as i32 + 1)))
}
fn col_copy(vm: &mut Vm, a: &mut [Value]) -> VR {
    let src = elements(vm, &a[1])?;
    let Value::List(d) = &a[0] else { return Err(uoe()) };
    if src.len() > d.len() {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("Source does not fit in dest".into())));
    }
    let mut dv = d.v.borrow_mut();
    for (i, x) in src.into_iter().enumerate() {
        dv[i] = x;
    }
    Ok(Value::Null)
}
fn col_disjoint(vm: &mut Vm, a: &mut [Value]) -> VR {
    let x = elements(vm, &a[0])?;
    let y = a[1].clone();
    for e in x {
        if contains_value(vm, &y, &e)? {
            return bool_v(false);
        }
    }
    bool_v(true)
}
fn col_fill(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    for slot in l.v.borrow_mut().iter_mut() {
        *slot = a[1].clone();
    }
    Ok(Value::Null)
}
fn col_frequency(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    Ok(Value::Int(items.iter().filter(|x| x.java_equals(&a[1])).count() as i32))
}
fn col_index_of_sub_list(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (s, t) = (elements(vm, &a[0])?, elements(vm, &a[1])?);
    if t.is_empty() {
        return Ok(Value::Int(0));
    }
    for i in 0..=(s.len() as i64 - t.len() as i64).max(-1) {
        let i = i as usize;
        if s[i..i + t.len()].iter().zip(t.iter()).all(|(x, y)| x.java_equals(y)) {
            return Ok(Value::Int(i as i32));
        }
    }
    Ok(Value::Int(-1))
}
fn col_last_index_of_sub_list(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (s, t) = (elements(vm, &a[0])?, elements(vm, &a[1])?);
    if t.len() > s.len() {
        return Ok(Value::Int(-1));
    }
    for i in (0..=(s.len() - t.len())).rev() {
        if s[i..i + t.len()].iter().zip(t.iter()).all(|(x, y)| x.java_equals(y)) {
            return Ok(Value::Int(i as i32));
        }
    }
    Ok(Value::Int(-1))
}
/// `Collections.max` / `min`: the iterator's first element, then one comparison per element
/// (`next` against the candidate), with the JDK's frames on failure.
fn col_extreme(vm: &mut Vm, a: &mut [Value], max: bool) -> VR {
    let items = elements(vm, &a[0])?;
    let cmp = a.get(1).cloned().unwrap_or(Value::Null);
    let with_cmp = a.len() > 1 && !cmp.is_null();
    let (name, first_line, cmp_line) = match (max, with_cmp) {
        (true, false) => ("max", 700, 704),
        (true, true) => ("max", 739, 743),
        (false, false) => ("min", 627, 631),
        (false, true) => ("min", 666, 670),
    };
    let mut it = items.into_iter();
    let Some(mut best) = it.next() else {
        let mut f = vec![];
        if matches!(&a[0], Value::List(l) if l.class == cls::ARRAY_LIST) {
            f.push("java.util.ArrayList$Itr.next(ArrayList.java:1061)".to_string());
        }
        f.push(format!("java.util.Collections.{name}(Collections.java:{first_line})"));
        let refs: Vec<&str> = f.iter().map(|s| s.as_str()).collect();
        return Err(nsee(&refs));
    };
    let want = if max { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
    for x in it {
        let o = compare_with(vm, &cmp, &x, &best).map_err(|mut e| {
            if e.offset.is_none() {
                e.frames.extend(jframes(&[&format!("java.util.Collections.{name}(Collections.java:{cmp_line})")]));
            }
            e
        })?;
        if o == want {
            best = x;
        }
    }
    Ok(best)
}
fn col_max(vm: &mut Vm, a: &mut [Value]) -> VR {
    col_extreme(vm, a, true)
}
fn col_min(vm: &mut Vm, a: &mut [Value]) -> VR {
    col_extreme(vm, a, false)
}
fn col_n_copies(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = int(&a[0]);
    if n < 0 {
        return Err(iae(format!("List length = {n}")).with_frames(&["java.base/java.util.Collections.nCopies(Collections.java:5506)"]));
    }
    // Java's copies list is a view of one element; this one holds every copy
    crate::guard::check_elements(n as usize)?;
    Ok(immutable_list(vec![a[1].clone(); n as usize], cls::IMMUTABLE_LIST))
}
fn col_replace_all(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    let mut changed = false;
    for slot in l.v.borrow_mut().iter_mut() {
        if slot.java_equals(&a[1]) {
            *slot = a[2].clone();
            changed = true;
        }
    }
    bool_v(changed)
}
fn col_reverse(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    l.v.borrow_mut().reverse();
    Ok(Value::Null)
}
fn col_rotate(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    let mut v = l.v.borrow_mut();
    let n = v.len() as i64;
    if n > 0 {
        let d = (int(&a[1]) as i64).rem_euclid(n) as usize;
        v.rotate_right(d);
    }
    Ok(Value::Null)
}
fn col_shuffle(vm: &mut Vm, a: &mut [Value]) -> VR {
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    let l = l.clone();
    let rnd = a.get(1).cloned();
    let mut items = l.v.borrow().clone();
    for i in (1..items.len()).rev() {
        let j = match &rnd {
            Some(r) => crate::lib_util::random_next_int_bound(r, (i + 1) as i32)? as usize,
            None => (crate::lib_util::thread_random(vm) * (i + 1) as f64) as usize,
        };
        items.swap(i, j);
    }
    *l.v.borrow_mut() = items;
    Ok(Value::Null)
}
fn col_singleton(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = JMap::new(cls::SINGLETON_SET);
    s.insert_raw(a[0].clone(), Value::Null);
    Ok(Value::Set(Rc::new(s)))
}
fn col_singleton_list(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(immutable_list(vec![a[0].clone()], cls::SINGLETON_LIST))
}
fn col_singleton_map(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = JMap::new(cls::SINGLETON_MAP);
    m.insert_raw(a[0].clone(), a[1].clone());
    Ok(Value::Map(Rc::new(m)))
}
fn col_sort(vm: &mut Vm, a: &mut [Value]) -> VR {
    let with_cmp = a.len() > 1;
    let mut args = [a[0].clone(), a.get(1).cloned().unwrap_or(Value::Null)];
    list_sort(vm, &mut args).map_err(|mut e| {
        if e.offset.is_none() {
            e.frames.extend(jframes(&[if with_cmp { "java.util.Collections.sort(Collections.java:180)" } else { "java.util.Collections.sort(Collections.java:147)" }]));
        }
        e
    })
}
fn col_swap(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::List(l) = &a[0] else { return Err(uoe()) };
    let (i, j) = (int(&a[1]), int(&a[2]));
    let n = l.len();
    if i < 0 || i as usize >= n || j < 0 || j as usize >= n {
        return Err(ioobe(if i < 0 || i as usize >= n { i as i64 } else { j as i64 }, n, "java.util.Collections.swap(Collections.java:0)"));
    }
    l.v.borrow_mut().swap(i as usize, j as usize);
    Ok(Value::Null)
}
fn col_unmodifiable_list(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(immutable_list(elements(vm, &a[0])?, cls::UNMODIFIABLE_LIST))
}
fn col_unmodifiable_collection(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(immutable_list(elements(vm, &a[0])?, cls::UNMODIFIABLE_LIST))
}
fn col_unmodifiable_set(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items = elements(vm, &a[0])?;
    let s = JMap::new(cls::UNMODIFIABLE_SET);
    for x in items {
        s.data.borrow_mut().m.insert(crate::value::Key(x), Value::Null);
    }
    Ok(Value::Set(Rc::new(s)))
}
fn col_unmodifiable_map(vm: &mut Vm, a: &mut [Value]) -> VR {
    let src = a[0].clone();
    let m = JMap::new(cls::UNMODIFIABLE_MAP);
    for (k, v) in map_entries(vm, &src)? {
        m.data.borrow_mut().m.insert(crate::value::Key(k), v);
    }
    Ok(Value::Map(Rc::new(m)))
}
fn col_reverse_order(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(func::native(Some(cls::COMPARATOR), |_, a| Ok(Value::Int(func::natural_compare(&a[1], &a[0])? as i32))))
}
fn col_reverse_order_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = a[0].clone();
    Ok(func::native(Some(cls::COMPARATOR), move |vm, a| Ok(Value::Int(compare_with(vm, &c, &a[1], &a[0])? as i32))))
}
fn col_empty_list_field(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(immutable_list(Vec::new(), cls::EMPTY_LIST))
}
fn col_empty_map_field(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Map(Rc::new(JMap::new(cls::EMPTY_MAP))))
}
fn col_empty_set_field(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Set(Rc::new(JMap::new(cls::EMPTY_SET))))
}

fn arrays_as_list(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(npe("Cannot read the array length because \"array\" is null"));
    }
    Ok(Value::List(Rc::new(JList::new(cls::ARRAYS_LIST, array_values(&a[0])))))
}
fn arrays_deep_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(deep_equals(&a[0], &a[1]))
}
fn deep_equals(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Obj(a), Value::Obj(b)) => match (&**a, &**b) {
            (Obj::Array(p), Obj::Array(q)) => {
                let (pv, qv) = (p.data.borrow().to_values(), q.data.borrow().to_values());
                pv.len() == qv.len() && pv.iter().zip(qv.iter()).all(|(u, v)| deep_equals(u, v))
            }
            _ => x.java_equals(y),
        },
        _ => x.java_equals(y),
    }
}
fn deep_hash(v: &Value) -> i32 {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => a.data.borrow().to_values().iter().fold(1i32, |h, x| h.wrapping_mul(31).wrapping_add(deep_hash(x))),
            _ => v.java_hash(),
        },
        _ => v.java_hash(),
    }
}
fn deep_to_string(v: &Value, out: &mut String) {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => {
                out.push('[');
                for (i, x) in a.data.borrow().to_values().iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    deep_to_string(x, out);
                }
                out.push(']');
            }
            _ => v.write_java_string(out, 0),
        },
        _ => v.write_java_string(out, 0),
    }
}
fn arrays_deep_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(if a[0].is_null() { 0 } else { deep_hash(&a[0]) }))
}
fn arrays_deep_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Ok(Value::str("null"));
    }
    let mut s = String::new();
    deep_to_string(&a[0], &mut s);
    Ok(Value::string(s))
}

fn objects_compare(vm: &mut Vm, a: &mut [Value]) -> VR {
    if crate::vm::same_ref_pub(&a[0], &a[1]) {
        return Ok(Value::Int(0));
    }
    let c = a[2].clone();
    Ok(Value::Int(compare_with(vm, &c, &a[0], &a[1])? as i32))
}
fn objects_deep_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(deep_equals(&a[0], &a[1]))
}
fn objects_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(a[0].java_equals(&a[1]))
}
fn objects_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    let items = array_values(&a[0]);
    Ok(Value::Int(items.iter().fold(1i32, |h, x| h.wrapping_mul(31).wrapping_add(x.java_hash()))))
}
fn objects_hash_code(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(a[0].java_hash()))
}
fn objects_is_null(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(a[0].is_null())
}
fn objects_non_null(_: &mut Vm, a: &mut [Value]) -> VR {
    bool_v(!a[0].is_null())
}
fn objects_require_non_null(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        let mut t = Throw::new(cls::NULL_POINTER_EXCEPTION, None);
        t.frames = jframes(&["java.util.Objects.requireNonNull(Objects.java:220)"]);
        return Err(t);
    }
    Ok(a[0].clone())
}
fn objects_require_non_null2(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        let msg = if a[1].is_null() { None } else { Some(a[1].to_java_string()) };
        let mut t = Throw::new(cls::NULL_POINTER_EXCEPTION, msg);
        t.frames = jframes(&["java.util.Objects.requireNonNull(Objects.java:246)"]);
        return Err(t);
    }
    Ok(a[0].clone())
}
fn objects_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(a[0].to_java_string()))
}
fn objects_to_string2(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Ok(a[1].clone());
    }
    Ok(Value::string(a[0].to_java_string()))
}

#[allow(dead_code)]
fn sum_values(xs: &[Value]) -> f64 {
    xs.iter().map(double).sum()
}

pub fn register(b: &mut Builder) {
    use cls::*;
    // hierarchy
    b.interface(COLLECTION, &[ITERABLE]);
    b.interface(LIST, &[COLLECTION]);
    b.interface(SET, &[COLLECTION]);
    b.interface(SORTED_SET, &[SET]);
    b.interface(NAVIGABLE_SET, &[SORTED_SET]);
    b.interface(QUEUE, &[COLLECTION]);
    b.interface(DEQUE, &[QUEUE]);
    b.interface(MAP, &[]);
    b.interface(SORTED_MAP, &[MAP]);
    b.interface(NAVIGABLE_MAP, &[SORTED_MAP]);
    b.interface(MAP_ENTRY, &[]);
    b.interface(ITERATOR, &[]);
    b.interface(LIST_ITERATOR, &[ITERATOR]);
    b.interface(PRIMITIVE_ITERATOR, &[ITERATOR]);
    b.interface(PRIMITIVE_ITERATOR_OF_INT, &[PRIMITIVE_ITERATOR]);
    b.interface(PRIMITIVE_ITERATOR_OF_LONG, &[PRIMITIVE_ITERATOR]);
    b.interface(PRIMITIVE_ITERATOR_OF_DOUBLE, &[PRIMITIVE_ITERATOR]);
    b.interface(RANDOM_ACCESS, &[]);
    b.supers(ABSTRACT_COLLECTION, &[OBJECT, COLLECTION]);
    b.supers(ABSTRACT_LIST, &[ABSTRACT_COLLECTION, LIST]);
    b.supers(ABSTRACT_SEQUENTIAL_LIST, &[ABSTRACT_LIST]);
    b.supers(ABSTRACT_SET, &[ABSTRACT_COLLECTION, SET]);
    b.supers(ABSTRACT_QUEUE, &[ABSTRACT_COLLECTION, QUEUE]);
    b.supers(ABSTRACT_MAP, &[OBJECT, MAP]);
    b.supers(SIMPLE_ENTRY, &[OBJECT, MAP_ENTRY]);
    b.supers(SIMPLE_IMMUTABLE_ENTRY, &[OBJECT, MAP_ENTRY]);
    b.supers(ARRAY_LIST, &[ABSTRACT_LIST, LIST, RANDOM_ACCESS]);
    b.supers(LINKED_LIST, &[ABSTRACT_SEQUENTIAL_LIST, LIST, DEQUE]);
    b.supers(VECTOR, &[ABSTRACT_LIST, LIST, RANDOM_ACCESS]);
    b.supers(STACK, &[VECTOR]);
    b.supers(ARRAY_DEQUE, &[ABSTRACT_COLLECTION, DEQUE]);
    b.supers(PRIORITY_QUEUE, &[ABSTRACT_QUEUE]);
    b.supers(HASH_MAP, &[ABSTRACT_MAP, MAP]);
    b.supers(LINKED_HASH_MAP, &[HASH_MAP, MAP]);
    b.supers(TREE_MAP, &[ABSTRACT_MAP, NAVIGABLE_MAP]);
    b.supers(DICTIONARY, &[OBJECT]);
    b.supers(HASHTABLE, &[DICTIONARY, MAP]);
    b.supers(IDENTITY_HASH_MAP, &[ABSTRACT_MAP, MAP]);
    b.supers(HASH_SET, &[ABSTRACT_SET, SET]);
    b.supers(LINKED_HASH_SET, &[HASH_SET, SET]);
    b.supers(TREE_SET, &[ABSTRACT_SET, NAVIGABLE_SET]);
    b.supers(ARRAYS_LIST, &[ABSTRACT_LIST, RANDOM_ACCESS]);
    b.supers(IMMUTABLE_LIST, &[ABSTRACT_LIST, RANDOM_ACCESS]);
    b.supers(IMMUTABLE_SET, &[ABSTRACT_SET]);
    b.supers(IMMUTABLE_MAP, &[ABSTRACT_MAP]);
    b.supers(UNMODIFIABLE_LIST, &[OBJECT, LIST, RANDOM_ACCESS]);
    b.supers(UNMODIFIABLE_SET, &[OBJECT, SET]);
    b.supers(UNMODIFIABLE_MAP, &[OBJECT, MAP]);
    b.supers(UNMODIFIABLE_COLLECTION, &[OBJECT, COLLECTION]);
    b.supers(EMPTY_LIST, &[ABSTRACT_LIST, RANDOM_ACCESS]);
    b.supers(EMPTY_MAP, &[ABSTRACT_MAP]);
    b.supers(EMPTY_SET, &[ABSTRACT_SET]);
    b.supers(SINGLETON_LIST, &[ABSTRACT_LIST, RANDOM_ACCESS]);
    b.supers(SINGLETON_SET, &[ABSTRACT_SET]);
    b.supers(SINGLETON_MAP, &[ABSTRACT_MAP]);
    b.supers(SUB_LIST, &[ABSTRACT_LIST, RANDOM_ACCESS]);
    b.supers(HASH_MAP_KEY_SET, &[ABSTRACT_SET]);
    b.supers(HASH_MAP_VALUES, &[ABSTRACT_COLLECTION]);
    b.supers(HASH_MAP_ENTRY_SET, &[ABSTRACT_SET]);
    b.supers(HASH_MAP_NODE, &[OBJECT, MAP_ENTRY]);
    b.supers(LINKED_KEY_SET, &[ABSTRACT_SET]);
    b.supers(LINKED_VALUES, &[ABSTRACT_COLLECTION]);
    b.supers(LINKED_ENTRY_SET, &[ABSTRACT_SET]);
    b.supers(LINKED_ENTRY, &[HASH_MAP_NODE]);
    b.supers(TREE_MAP_KEY_SET, &[ABSTRACT_SET, NAVIGABLE_SET]);
    b.supers(TREE_MAP_VALUES, &[ABSTRACT_COLLECTION]);
    b.supers(TREE_MAP_ENTRY_SET, &[ABSTRACT_SET]);
    b.supers(TREE_MAP_ENTRY, &[OBJECT, MAP_ENTRY]);
    b.supers(ARRAY_LIST_ITR, &[OBJECT, ITERATOR]);
    b.supers(LIST_ITR, &[ARRAY_LIST_ITR, LIST_ITERATOR]);
    b.supers(HASH_MAP_ITERATOR, &[OBJECT, ITERATOR]);
    b.supers(GENERIC_ITERATOR, &[OBJECT, LIST_ITERATOR]);
    b.supers(PRIMITIVE_INT_ITERATOR, &[OBJECT, PRIMITIVE_ITERATOR_OF_INT]);
    b.supers(PRIMITIVE_LONG_ITERATOR, &[OBJECT, PRIMITIVE_ITERATOR_OF_LONG]);
    b.supers(PRIMITIVE_DOUBLE_ITERATOR, &[OBJECT, PRIMITIVE_ITERATOR_OF_DOUBLE]);

    // Iterable (augmentations)
    let m = b.m(ITERABLE, "void forEach(Consumer)", it_for_each);
    b.aug(m, "java.lang.Iterable.forEach(java.util.function.Consumer)");
    b.m(ITERABLE, "Iterator iterator()", it_iterator);
    let aug = |b: &mut Builder, c, sig: &str, imp, inner: &str| {
        let m = b.m(c, sig, imp);
        b.aug(m, inner);
    };
    let it_inner = "java.lang.Iterable.iterator()";
    aug(b, ITERABLE, "boolean any(Predicate)", it_any, it_inner);
    aug(b, ITERABLE, "Collection asCollection()", it_as_collection, it_inner);
    aug(b, ITERABLE, "List asList()", it_as_list, it_inner);
    aug(b, ITERABLE, "int count(Predicate)", it_count, it_inner);
    aug(b, ITERABLE, "def each(Consumer)", it_each, it_inner);
    aug(b, ITERABLE, "def eachWithIndex(ObjIntConsumer)", it_each_with_index, it_inner);
    aug(b, ITERABLE, "boolean every(Predicate)", it_every, it_inner);
    aug(b, ITERABLE, "List findResults(Function)", it_find_results, it_inner);
    aug(b, ITERABLE, "Map groupBy(Function)", it_group_by, it_inner);
    aug(b, ITERABLE, "String join(String)", it_join, it_inner);
    aug(b, ITERABLE, "double sum()", it_sum, it_inner);
    aug(b, ITERABLE, "double sum(ToDoubleFunction)", it_sum_fn, it_inner);

    // Collection
    b.m(COLLECTION, "boolean add(def)", coll_add);
    b.m(COLLECTION, "boolean addAll(Collection)", coll_add_all);
    b.m(COLLECTION, "void clear()", coll_clear);
    b.m(COLLECTION, "boolean contains(def)", coll_contains);
    b.m(COLLECTION, "boolean containsAll(Collection)", coll_contains_all);
    b.m(COLLECTION, "boolean isEmpty()", coll_is_empty);
    b.m(COLLECTION, "boolean removeAll(Collection)", coll_remove_all);
    aug(b, COLLECTION, "boolean removeIf(Predicate)", coll_remove_if, "java.util.Collection.removeIf(java.util.function.Predicate)");
    b.m(COLLECTION, "boolean retainAll(Collection)", coll_retain_all);
    b.m(COLLECTION, "int size()", coll_size);
    b.m(COLLECTION, "Stream stream()", coll_stream);
    b.m(COLLECTION, "def[] toArray()", coll_to_array);
    b.m(COLLECTION, "def[] toArray(def[])", coll_to_array2);
    let c_inner = "java.util.Collection.iterator()";
    aug(b, COLLECTION, "List collect(Function)", coll_collect, c_inner);
    aug(b, COLLECTION, "def collect(Collection,Function)", coll_collect2, c_inner);
    aug(b, COLLECTION, "def find(Predicate)", coll_find, c_inner);
    aug(b, COLLECTION, "List findAll(Predicate)", coll_find_all, c_inner);
    aug(b, COLLECTION, "def findResult(Function)", coll_find_result, c_inner);
    aug(b, COLLECTION, "def findResult(def,Function)", coll_find_result2, c_inner);
    aug(b, COLLECTION, "List split(Predicate)", coll_split, c_inner);

    // List
    b.m(LIST, "void add(int,def)", list_add_at);
    b.m(LIST, "boolean addAll(int,Collection)", list_add_all_at);
    b.m(LIST, "boolean equals(Object)", list_equals);
    b.m(LIST, "def get(int)", list_get_n);
    b.m(LIST, "int hashCode()", list_hash);
    b.m(LIST, "int indexOf(def)", list_index_of);
    b.m(LIST, "int lastIndexOf(def)", list_last_index_of);
    b.m(LIST, "ListIterator listIterator()", list_iterator);
    b.m(LIST, "ListIterator listIterator(int)", list_iterator_at);
    b.m(LIST, "def remove(int)", list_remove_at);
    aug(b, LIST, "void replaceAll(UnaryOperator)", list_replace_all, "java.util.List.replaceAll(java.util.function.UnaryOperator)");
    b.m(LIST, "def set(int,def)", list_set_n);
    aug(b, LIST, "int getLength()", list_get_length, "java.util.List.size()");
    b.m(LIST, "void sort(Comparator)", list_sort);
    b.m(LIST, "List subList(int,int)", list_sub_list);
    aug(b, LIST, "Object getByPath(String)", get_by_path1, "java.lang.Object.getClass()");
    aug(b, LIST, "Object getByPath(String,Object)", get_by_path2, "java.lang.Object.getClass()");
    b.m(LIST_ITERATOR, "void add(def)", iter_add);
    b.m(LIST_ITERATOR, "boolean hasPrevious()", iter_has_previous);
    b.m(LIST_ITERATOR, "int nextIndex()", iter_next_index);
    b.m(LIST_ITERATOR, "int previousIndex()", iter_previous_index);
    b.m(LIST_ITERATOR, "void set(def)", iter_set);

    // Iterator
    aug(b, ITERATOR, "void forEachRemaining(Consumer)", iter_for_each_remaining, "java.util.Iterator.forEachRemaining(java.util.function.Consumer)");
    b.m(ITERATOR, "boolean hasNext()", iter_has_next);
    b.m(ITERATOR, "def next()", iter_next);
    b.m(ITERATOR, "void remove()", iter_remove);
    b.m(PRIMITIVE_ITERATOR, "void forEachRemaining(def)", iter_for_each_remaining);
    b.m(PRIMITIVE_ITERATOR_OF_INT, "void forEachRemaining(IntConsumer)", iter_for_each_remaining);
    b.m(PRIMITIVE_ITERATOR_OF_INT, "Integer next()", iter_next);
    b.m(PRIMITIVE_ITERATOR_OF_INT, "int nextInt()", iter_next);
    b.m(PRIMITIVE_ITERATOR_OF_LONG, "void forEachRemaining(LongConsumer)", iter_for_each_remaining);
    b.m(PRIMITIVE_ITERATOR_OF_LONG, "Long next()", iter_next);
    b.m(PRIMITIVE_ITERATOR_OF_LONG, "long nextLong()", iter_next);
    b.m(PRIMITIVE_ITERATOR_OF_DOUBLE, "void forEachRemaining(DoubleConsumer)", iter_for_each_remaining);
    b.m(PRIMITIVE_ITERATOR_OF_DOUBLE, "Double next()", iter_next);
    b.m(PRIMITIVE_ITERATOR_OF_DOUBLE, "double nextDouble()", iter_next);

    // Deque / Queue
    b.m(DEQUE, "void addFirst(def)", deque_add_first);
    b.m(DEQUE, "void addLast(def)", deque_add_last);
    b.m(DEQUE, "Iterator descendingIterator()", deque_descending_iterator);
    b.m(DEQUE, "def getFirst()", deque_get_first);
    b.m(DEQUE, "def getLast()", deque_get_last);
    b.m(DEQUE, "boolean offerFirst(def)", deque_offer_first);
    b.m(DEQUE, "boolean offerLast(def)", deque_offer_last);
    b.m(DEQUE, "def peekFirst()", deque_peek_first);
    b.m(DEQUE, "def peekLast()", deque_peek_last);
    b.m(DEQUE, "def pollFirst()", deque_poll_first);
    b.m(DEQUE, "def pollLast()", deque_poll_last);
    b.m(DEQUE, "def pop()", deque_pop);
    b.m(DEQUE, "void push(def)", deque_push);
    b.m(DEQUE, "boolean remove(def)", deque_remove_obj);
    b.m(DEQUE, "def removeFirst()", deque_remove_first);
    b.m(DEQUE, "boolean removeFirstOccurrence(def)", deque_remove_obj);
    b.m(DEQUE, "def removeLast()", deque_remove_last);
    b.m(DEQUE, "boolean removeLastOccurrence(def)", deque_remove_last_occ);
    b.m(QUEUE, "def element()", queue_element);
    b.m(QUEUE, "boolean offer(def)", queue_offer);
    b.m(QUEUE, "def peek()", deque_peek_first);
    b.m(QUEUE, "def poll()", deque_poll_first);
    b.m(QUEUE, "def remove()", queue_remove);

    // Set
    b.m(SET, "boolean equals(Object)", set_equals);
    b.m(SET, "int hashCode()", list_hash);
    b.m(SET, "boolean remove(def)", set_remove);
    b.m(SORTED_SET, "Comparator comparator()", sorted_comparator);
    b.m(SORTED_SET, "def first()", sset_first);
    b.m(SORTED_SET, "SortedSet headSet(def)", sset_head);
    b.m(SORTED_SET, "def last()", sset_last);
    b.m(SORTED_SET, "SortedSet subSet(def,def)", sset_sub);
    b.m(SORTED_SET, "SortedSet tailSet(def)", sset_tail);
    b.m(NAVIGABLE_SET, "def ceiling(def)", sset_ceiling);
    b.m(NAVIGABLE_SET, "Iterator descendingIterator()", sset_descending_iterator);
    b.m(NAVIGABLE_SET, "NavigableSet descendingSet()", sset_descending);
    b.m(NAVIGABLE_SET, "def floor(def)", sset_floor);
    b.m(NAVIGABLE_SET, "NavigableSet headSet(def,boolean)", sset_head2);
    b.m(NAVIGABLE_SET, "def higher(def)", sset_higher);
    b.m(NAVIGABLE_SET, "def lower(def)", sset_lower);
    b.m(NAVIGABLE_SET, "def pollFirst()", sset_poll_first);
    b.m(NAVIGABLE_SET, "def pollLast()", sset_poll_last);
    b.m(NAVIGABLE_SET, "NavigableSet subSet(def,boolean,def,boolean)", sset_sub4);
    b.m(NAVIGABLE_SET, "NavigableSet tailSet(def,boolean)", sset_tail2);

    // Map
    b.m(MAP, "void clear()", map_clear);
    b.m(MAP, "def compute(def,BiFunction)", map_compute);
    b.m(MAP, "def computeIfAbsent(def,Function)", map_compute_if_absent);
    b.m(MAP, "def computeIfPresent(def,BiFunction)", map_compute_if_present);
    b.m(MAP, "boolean containsKey(def)", map_contains_key);
    b.m(MAP, "boolean containsValue(def)", map_contains_value);
    b.m(MAP, "Set entrySet()", map_entry_set);
    b.m(MAP, "boolean equals(Object)", map_equals);
    aug(b, MAP, "void forEach(BiConsumer)", map_for_each, "java.util.Map.forEach(java.util.function.BiConsumer)");
    b.m(MAP, "def get(def)", map_get_n);
    b.m(MAP, "def getOrDefault(def,def)", map_get_or_default);
    b.m(MAP, "boolean isEmpty()", map_is_empty);
    b.m(MAP, "Set keySet()", map_key_set);
    b.m(MAP, "def merge(def,def,BiFunction)", map_merge);
    b.m(MAP, "def put(def,def)", map_put_n);
    b.m(MAP, "void putAll(Map)", map_put_all);
    b.m(MAP, "def putIfAbsent(def,def)", map_put_if_absent);
    b.m(MAP, "def remove(def)", map_remove);
    b.m(MAP, "boolean remove(def,def)", map_remove2);
    b.m(MAP, "def replace(def,def)", map_replace);
    b.m(MAP, "boolean replace(def,def,def)", map_replace3);
    aug(b, MAP, "void replaceAll(BiFunction)", map_replace_all, "java.util.Map.replaceAll(java.util.function.BiFunction)");
    b.m(MAP, "int size()", map_size);
    b.m(MAP, "Collection values()", map_values);
    let m_inner = "java.util.Map.entrySet()";
    aug(b, MAP, "Object getByPath(String)", get_by_path1, "java.lang.Object.getClass()");
    aug(b, MAP, "Object getByPath(String,Object)", get_by_path2, "java.lang.Object.getClass()");
    aug(b, MAP, "List collect(BiFunction)", map_collect, m_inner);
    aug(b, MAP, "def collect(Collection,BiFunction)", map_collect2, m_inner);
    aug(b, MAP, "int count(BiPredicate)", map_count, m_inner);
    aug(b, MAP, "def each(BiConsumer)", map_each, m_inner);
    aug(b, MAP, "boolean every(BiPredicate)", map_every, m_inner);
    aug(b, MAP, "Map.Entry find(BiPredicate)", map_find, m_inner);
    aug(b, MAP, "Map findAll(BiPredicate)", map_find_all, m_inner);
    aug(b, MAP, "def findResult(BiFunction)", map_find_result, m_inner);
    aug(b, MAP, "def findResult(def,BiFunction)", map_find_result2, m_inner);
    aug(b, MAP, "List findResults(BiFunction)", map_find_results, m_inner);
    aug(b, MAP, "Map groupBy(BiFunction)", map_group_by, m_inner);
    b.m(SORTED_MAP, "Comparator comparator()", sorted_comparator);
    b.m(SORTED_MAP, "def firstKey()", smap_first_key);
    b.m(SORTED_MAP, "SortedMap headMap(def)", sset_head);
    b.m(SORTED_MAP, "def lastKey()", smap_last_key);
    b.m(SORTED_MAP, "SortedMap subMap(def,def)", sset_sub);
    b.m(SORTED_MAP, "SortedMap tailMap(def)", sset_tail);
    b.m(NAVIGABLE_MAP, "Map.Entry ceilingEntry(def)", smap_ceiling_entry);
    b.m(NAVIGABLE_MAP, "def ceilingKey(def)", smap_ceiling_key);
    b.m(NAVIGABLE_MAP, "NavigableSet descendingKeySet()", smap_descending_key_set);
    b.m(NAVIGABLE_MAP, "NavigableMap descendingMap()", smap_descending_map);
    b.m(NAVIGABLE_MAP, "Map.Entry firstEntry()", smap_first_entry);
    b.m(NAVIGABLE_MAP, "Map.Entry floorEntry(def)", smap_floor_entry);
    b.m(NAVIGABLE_MAP, "def floorKey(def)", smap_floor_key);
    b.m(NAVIGABLE_MAP, "NavigableMap headMap(def,boolean)", sset_head2);
    b.m(NAVIGABLE_MAP, "Map.Entry higherEntry(def)", smap_higher_entry);
    b.m(NAVIGABLE_MAP, "def higherKey(def)", smap_higher_key);
    b.m(NAVIGABLE_MAP, "Map.Entry lastEntry()", smap_last_entry);
    b.m(NAVIGABLE_MAP, "Map.Entry lowerEntry(def)", smap_lower_entry);
    // NavigableMap.lowerKey is not in Elasticsearch's allowlist (lowerEntry is)
    b.m(NAVIGABLE_MAP, "NavigableSet navigableKeySet()", smap_key_set_view);
    b.m(NAVIGABLE_MAP, "Map.Entry pollFirstEntry()", smap_poll_first);
    b.m(NAVIGABLE_MAP, "Map.Entry pollLastEntry()", smap_poll_last);
    b.m(NAVIGABLE_MAP, "NavigableMap subMap(def,boolean,def,boolean)", sset_sub4);
    b.m(NAVIGABLE_MAP, "NavigableMap tailMap(def,boolean)", sset_tail2);

    // Map.Entry
    b.s(MAP_ENTRY, "Comparator comparingByKey()", entry_comparing_by_key);
    b.s(MAP_ENTRY, "Comparator comparingByKey(Comparator)", entry_comparing_by_key_cmp);
    b.s(MAP_ENTRY, "Comparator comparingByValue()", entry_comparing_by_value);
    b.s(MAP_ENTRY, "Comparator comparingByValue(Comparator)", entry_comparing_by_value_cmp);
    b.m(MAP_ENTRY, "boolean equals(Object)", entry_equals);
    b.m(MAP_ENTRY, "def getKey()", entry_get_key);
    b.m(MAP_ENTRY, "def getValue()", entry_get_value);
    b.m(MAP_ENTRY, "int hashCode()", entry_hash);
    b.m(MAP_ENTRY, "def setValue(def)", entry_set_value);
    b.c(SIMPLE_ENTRY, "(def,def)", simple_entry_new);
    b.c(SIMPLE_ENTRY, "(Map.Entry)", simple_entry_from);
    b.c(SIMPLE_IMMUTABLE_ENTRY, "(def,def)", simple_immutable_entry_new);
    b.c(SIMPLE_IMMUTABLE_ENTRY, "(Map.Entry)", simple_immutable_entry_from);

    // implementations
    b.c(ARRAY_LIST, "()", arraylist_new);
    b.c(ARRAY_LIST, "(Collection)", arraylist_from);
    b.m(ARRAY_LIST, "def clone()", list_clone);
    b.m(ARRAY_LIST, "void trimToSize()", noop);
    b.c(LINKED_LIST, "()", linkedlist_new);
    b.c(LINKED_LIST, "(Collection)", linkedlist_from);
    b.m(LINKED_LIST, "def clone()", list_clone);
    b.c(VECTOR, "()", vector_new);
    b.c(VECTOR, "(Collection)", vector_from);
    b.m(VECTOR, "void addElement(def)", vector_add_element);
    b.m(VECTOR, "def elementAt(int)", vector_element_at);
    b.m(VECTOR, "def firstElement()", vector_first);
    b.m(VECTOR, "void insertElementAt(def,int)", vector_insert_at);
    b.m(VECTOR, "def lastElement()", vector_last);
    b.m(VECTOR, "int lastIndexOf(def,int)", vector_last_index_of2);
    b.m(VECTOR, "void removeAllElements()", coll_clear);
    b.m(VECTOR, "boolean removeElement(def)", vector_remove_element);
    b.m(VECTOR, "void removeElementAt(int)", vector_remove_at);
    b.m(VECTOR, "void setElementAt(def,int)", vector_set_at);
    b.m(VECTOR, "def clone()", list_clone);
    b.c(STACK, "()", stack_new);
    b.m(STACK, "def push(def)", deque_push);
    b.m(STACK, "def pop()", deque_pop);
    b.m(STACK, "def peek()", stack_peek);
    b.m(STACK, "boolean empty()", stack_empty);
    b.m(STACK, "int search(def)", stack_search);
    b.c(ARRAY_DEQUE, "()", deque_new);
    b.c(ARRAY_DEQUE, "(Collection)", deque_from);
    b.m(ARRAY_DEQUE, "ArrayDeque clone()", list_clone);
    b.c(PRIORITY_QUEUE, "()", pq_new);
    b.c(PRIORITY_QUEUE, "(Comparator)", pq_new_cmp);
    b.c(HASH_MAP, "()", hashmap_new);
    b.c(HASH_MAP, "(Map)", hashmap_from);
    b.m(HASH_MAP, "def clone()", list_clone);
    b.c(LINKED_HASH_MAP, "()", linkedhashmap_new);
    b.c(LINKED_HASH_MAP, "(Map)", linkedhashmap_from);
    b.c(TREE_MAP, "()", treemap_new);
    b.c(TREE_MAP, "(Comparator)", treemap_new_cmp);
    b.m(TREE_MAP, "def clone()", list_clone);
    b.c(HASHTABLE, "()", hashtable_new);
    b.c(HASHTABLE, "(Map)", hashtable_from);
    b.m(HASHTABLE, "def clone()", list_clone);
    b.c(IDENTITY_HASH_MAP, "()", identity_map_new);
    b.c(IDENTITY_HASH_MAP, "(Map)", hashmap_from);
    b.m(IDENTITY_HASH_MAP, "def clone()", list_clone);
    b.c(HASH_SET, "()", hashset_new);
    b.c(HASH_SET, "(Collection)", hashset_from);
    b.m(HASH_SET, "def clone()", list_clone);
    b.c(LINKED_HASH_SET, "()", linkedhashset_new);
    b.c(LINKED_HASH_SET, "(Collection)", linkedhashset_from);
    b.c(TREE_SET, "()", treeset_new);
    b.c(TREE_SET, "(Comparator)", treeset_new_cmp);
    b.m(TREE_SET, "def clone()", list_clone);

    // Collections
    b.sf(COLLECTIONS, "List EMPTY_LIST", col_empty_list_field);
    b.sf(COLLECTIONS, "Map EMPTY_MAP", col_empty_map_field);
    b.sf(COLLECTIONS, "Set EMPTY_SET", col_empty_set_field);
    b.s(COLLECTIONS, "boolean addAll(Collection,def[])", col_add_all);
    b.s(COLLECTIONS, "int binarySearch(List,def)", col_binary_search);
    b.s(COLLECTIONS, "int binarySearch(List,def,Comparator)", col_binary_search);
    b.s(COLLECTIONS, "void copy(List,List)", col_copy);
    b.s(COLLECTIONS, "boolean disjoint(Collection,Collection)", col_disjoint);
    b.s(COLLECTIONS, "Iterator emptyIterator()", col_empty_iterator);
    b.s(COLLECTIONS, "List emptyList()", col_empty_list);
    b.s(COLLECTIONS, "ListIterator emptyListIterator()", col_empty_iterator);
    b.s(COLLECTIONS, "Map emptyMap()", col_empty_map);
    b.s(COLLECTIONS, "NavigableMap emptyNavigableMap()", col_empty_map);
    b.s(COLLECTIONS, "NavigableSet emptyNavigableSet()", col_empty_set);
    b.s(COLLECTIONS, "Set emptySet()", col_empty_set);
    b.s(COLLECTIONS, "SortedMap emptySortedMap()", col_empty_map);
    b.s(COLLECTIONS, "SortedSet emptySortedSet()", col_empty_set);
    b.s(COLLECTIONS, "void fill(List,def)", col_fill);
    b.s(COLLECTIONS, "int frequency(Collection,def)", col_frequency);
    b.s(COLLECTIONS, "int indexOfSubList(List,List)", col_index_of_sub_list);
    b.s(COLLECTIONS, "int lastIndexOfSubList(List,List)", col_last_index_of_sub_list);
    b.s(COLLECTIONS, "def max(Collection)", col_max);
    b.s(COLLECTIONS, "def max(Collection,Comparator)", col_max);
    b.s(COLLECTIONS, "def min(Collection)", col_min);
    b.s(COLLECTIONS, "def min(Collection,Comparator)", col_min);
    b.s(COLLECTIONS, "List nCopies(int,def)", col_n_copies);
    b.s(COLLECTIONS, "boolean replaceAll(List,def,def)", col_replace_all);
    b.s(COLLECTIONS, "void reverse(List)", col_reverse);
    b.s(COLLECTIONS, "Comparator reverseOrder()", col_reverse_order);
    b.s(COLLECTIONS, "Comparator reverseOrder(Comparator)", col_reverse_order_cmp);
    b.s(COLLECTIONS, "void rotate(List,int)", col_rotate);
    let m = b.s(COLLECTIONS, "void shuffle(List)", col_shuffle);
    b.nondeterministic(m);
    b.s(COLLECTIONS, "void shuffle(List,Random)", col_shuffle);
    b.s(COLLECTIONS, "Set singleton(def)", col_singleton);
    b.s(COLLECTIONS, "List singletonList(def)", col_singleton_list);
    b.s(COLLECTIONS, "Map singletonMap(def,def)", col_singleton_map);
    b.s(COLLECTIONS, "void sort(List)", col_sort);
    b.s(COLLECTIONS, "void sort(List,Comparator)", col_sort);
    b.s(COLLECTIONS, "void swap(List,int,int)", col_swap);
    b.s(COLLECTIONS, "Collection unmodifiableCollection(Collection)", col_unmodifiable_collection);
    b.s(COLLECTIONS, "List unmodifiableList(List)", col_unmodifiable_list);
    b.s(COLLECTIONS, "Map unmodifiableMap(Map)", col_unmodifiable_map);
    b.s(COLLECTIONS, "NavigableMap unmodifiableNavigableMap(NavigableMap)", col_unmodifiable_map);
    b.s(COLLECTIONS, "NavigableSet unmodifiableNavigableSet(NavigableSet)", col_unmodifiable_set);
    b.s(COLLECTIONS, "Set unmodifiableSet(Set)", col_unmodifiable_set);
    b.s(COLLECTIONS, "SortedMap unmodifiableSortedMap(SortedMap)", col_unmodifiable_map);
    b.s(COLLECTIONS, "SortedSet unmodifiableSortedSet(SortedSet)", col_unmodifiable_set);

    // Arrays / Objects
    b.s(ARRAYS, "List asList(Object[])", arrays_as_list);
    b.s(ARRAYS, "boolean deepEquals(Object[],Object[])", arrays_deep_equals);
    b.s(ARRAYS, "int deepHashCode(Object[])", arrays_deep_hash);
    b.s(ARRAYS, "String deepToString(Object[])", arrays_deep_to_string);
    b.s(OBJECTS, "int compare(def,def,Comparator)", objects_compare);
    b.s(OBJECTS, "boolean deepEquals(Object,Object)", objects_deep_equals);
    b.s(OBJECTS, "boolean equals(Object,Object)", objects_equals);
    b.s(OBJECTS, "int hash(Object[])", objects_hash);
    b.s(OBJECTS, "int hashCode(Object)", objects_hash_code);
    b.s(OBJECTS, "boolean isNull(Object)", objects_is_null);
    b.s(OBJECTS, "boolean nonNull(Object)", objects_non_null);
    b.s(OBJECTS, "def requireNonNull(def)", objects_require_non_null);
    b.s(OBJECTS, "def requireNonNull(def,String)", objects_require_non_null2);
    b.s(OBJECTS, "String toString(Object)", objects_to_string);
    b.s(OBJECTS, "String toString(Object,String)", objects_to_string2);
}

