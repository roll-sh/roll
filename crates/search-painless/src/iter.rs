//! Iterators over collections, maps' views, arrays and streams.
//!
//! List iterators read the live list (so `set` during iteration is visible) and fail fast with
//! a `ConcurrentModificationException` when the list changes size behind them, like
//! `ArrayList.Itr`; map and set iterators do the same against the map's modification count.
//! `remove()` writes through to the source collection.

use std::cell::RefCell;
use std::rc::Rc;

use crate::types::{cls, ClassId};
use crate::value::{JEntry, JList, JMap, Obj, ViewKind, Value};
use crate::vm::{iae, Throw, Thrown, Vm, VR};

pub enum Source {
    None,
    /// A primitive array (for-each over a `def` array converts elements by its component type).
    PrimArray(crate::ir::Prim),
    List(Rc<JList>),
    Map(Rc<JMap>, ViewKind),
    Set(Rc<JMap>),
}

pub struct JIter {
    pub class: ClassId,
    pub items: Vec<Value>,
    pub pos: usize,
    pub source: Source,
    pub mods: u32,
    /// Whether `remove()` is allowed (an element was returned and not yet removed).
    pub can_remove: bool,
}

impl JIter {
    pub fn class(&self) -> ClassId {
        self.class
    }

    pub fn of_values(class: ClassId, items: Vec<Value>) -> JIter {
        JIter { class, items, pos: 0, source: Source::None, mods: 0, can_remove: false }
    }

    fn check(&self) -> Result<(), Thrown> {
        let cur = match &self.source {
            Source::None | Source::PrimArray(_) => return Ok(()),
            Source::List(l) => l.mods.get(),
            Source::Map(m, _) | Source::Set(m) => m.mods.get(),
        };
        if cur != self.mods {
            return Err(self.cme());
        }
        Ok(())
    }

    /// `ConcurrentModificationException` with the JDK frames of the failing iterator.
    fn cme(&self) -> Thrown {
        use crate::value::MapOrder;
        let hash = |kind: &str, line: u32| -> Vec<String> {
            let mut f = vec!["java.base/java.util.HashMap$HashIterator.nextNode(HashMap.java:1631)".to_string(), format!("java.base/java.util.HashMap${kind}.next(HashMap.java:{line})")];
            if kind == "EntryIterator" {
                f.push("java.base/java.util.HashMap$EntryIterator.next(HashMap.java:1662)".into());
            }
            f
        };
        let linked = |kind: &str, line: u32| -> Vec<String> {
            let mut f = vec!["java.base/java.util.LinkedHashMap$LinkedHashIterator.nextNode(LinkedHashMap.java:1024)".to_string(), format!("java.base/java.util.LinkedHashMap${kind}.next(LinkedHashMap.java:{line})")];
            if kind == "LinkedEntryIterator" {
                f.push("java.base/java.util.LinkedHashMap$LinkedEntryIterator.next(LinkedHashMap.java:1056)".into());
            }
            f
        };
        let frames: Vec<String> = match &self.source {
            Source::List(l) if l.class == cls::LINKED_LIST => vec![
                "java.base/java.util.LinkedList$ListItr.checkForComodification(LinkedList.java:978)".into(),
                "java.base/java.util.LinkedList$ListItr.next(LinkedList.java:900)".into(),
            ],
            Source::List(_) => vec![
                "java.base/java.util.ArrayList$Itr.checkForComodification(ArrayList.java:1104)".into(),
                "java.base/java.util.ArrayList$Itr.next(ArrayList.java:1058)".into(),
            ],
            Source::Map(m, kind) => match (m.order() == MapOrder::Insertion, kind) {
                (false, ViewKind::Keys) => hash("KeyIterator", 1654),
                (false, ViewKind::Values) => hash("ValueIterator", 1659),
                (false, ViewKind::Entries) => hash("EntryIterator", 1664),
                (true, ViewKind::Keys) => linked("LinkedKeyIterator", 1047),
                (true, ViewKind::Values) => linked("LinkedValueIterator", 1053),
                (true, ViewKind::Entries) => linked("LinkedEntryIterator", 1059),
            },
            Source::Set(m) if m.order() == MapOrder::Insertion => linked("LinkedKeyIterator", 1047),
            Source::Set(_) => hash("KeyIterator", 1654),
            Source::None | Source::PrimArray(_) => Vec::new(),
        };
        let mut t = Throw::new(cls::CONCURRENT_MODIFICATION_EXCEPTION, None);
        t.frames = frames;
        t
    }

    /// The JDK frames of this iterator's `next` (`nextNode` for hash iterators), with the
    /// lines where `next` fails on an exhausted iterator.
    fn next_frames(&self) -> Vec<String> {
        use crate::value::MapOrder;
        let hash = |kind: &str, line: u32| -> Vec<String> {
            let mut f = vec!["java.base/java.util.HashMap$HashIterator.nextNode(HashMap.java:1633)".to_string(), format!("java.base/java.util.HashMap${kind}.next(HashMap.java:{line})")];
            if kind == "EntryIterator" {
                f.push("java.base/java.util.HashMap$EntryIterator.next(HashMap.java:1662)".into());
            }
            f
        };
        let linked = |kind: &str, line: u32| -> Vec<String> {
            let mut f = vec!["java.base/java.util.LinkedHashMap$LinkedHashIterator.nextNode(LinkedHashMap.java:1026)".to_string(), format!("java.base/java.util.LinkedHashMap${kind}.next(LinkedHashMap.java:{line})")];
            if kind == "LinkedEntryIterator" {
                f.push("java.base/java.util.LinkedHashMap$LinkedEntryIterator.next(LinkedHashMap.java:1056)".into());
            }
            f
        };
        match &self.source {
            Source::List(l) if l.class == cls::LINKED_LIST => vec!["java.base/java.util.LinkedList$ListItr.next(LinkedList.java:902)".into()],
            Source::List(_) => vec!["java.base/java.util.ArrayList$Itr.next(ArrayList.java:1061)".into()],
            Source::Map(m, kind) => match (m.order() == MapOrder::Insertion, kind) {
                (false, ViewKind::Keys) => hash("KeyIterator", 1654),
                (false, ViewKind::Values) => hash("ValueIterator", 1659),
                (false, ViewKind::Entries) => hash("EntryIterator", 1664),
                (true, ViewKind::Keys) => linked("LinkedKeyIterator", 1047),
                (true, ViewKind::Values) => linked("LinkedValueIterator", 1053),
                (true, ViewKind::Entries) => linked("LinkedEntryIterator", 1059),
            },
            Source::Set(m) if m.order() == MapOrder::Insertion => linked("LinkedKeyIterator", 1047),
            Source::Set(_) => hash("KeyIterator", 1654),
            Source::None | Source::PrimArray(_) => Vec::new(),
        }
    }

    /// `NoSuchElementException` of `next()` past the end.
    pub fn exhausted(&self) -> Thrown {
        let mut t = Throw::new(cls::NO_SUCH_ELEMENT_EXCEPTION, None);
        t.frames = self.next_frames();
        t
    }

    /// `IllegalStateException` of `remove()` without a preceding `next()`.
    fn illegal_remove(&self) -> Thrown {
        use crate::value::MapOrder;
        let frame = match &self.source {
            Source::List(l) if l.class == cls::LINKED_LIST => "java.base/java.util.LinkedList$ListItr.remove(LinkedList.java:935)",
            Source::List(_) => "java.base/java.util.ArrayList$Itr.remove(ArrayList.java:1071)",
            Source::Map(m, _) | Source::Set(m) if m.order() == MapOrder::Insertion => "java.base/java.util.LinkedHashMap$LinkedHashIterator.remove(LinkedHashMap.java:1035)",
            Source::Map(..) | Source::Set(_) => "java.base/java.util.HashMap$HashIterator.remove(HashMap.java:1643)",
            Source::None | Source::PrimArray(_) => "",
        };
        let t = Throw::new(cls::ILLEGAL_STATE_EXCEPTION, None);
        if frame.is_empty() { t } else { t.with_frames(&[frame]) }
    }

    pub fn has_next(&self) -> bool {
        match &self.source {
            Source::List(l) => self.pos < l.len(),
            _ => self.pos < self.items.len(),
        }
    }

    pub fn next(&mut self) -> Result<Option<Value>, Thrown> {
        match &self.source {
            Source::List(l) => {
                let l = l.clone();
                if self.pos >= l.len() {
                    // a structural change still fails fast on the next call, like ArrayList.Itr
                    if l.mods.get() != self.mods {
                        return Err(self.cme());
                    }
                    return Ok(None);
                }
                self.check()?;
                let v = l.get(self.pos);
                self.pos += 1;
                self.can_remove = true;
                Ok(v)
            }
            _ => {
                if self.pos >= self.items.len() {
                    return Ok(None);
                }
                self.check()?;
                let v = self.items[self.pos].clone();
                self.pos += 1;
                self.can_remove = true;
                Ok(Some(v))
            }
        }
    }

    pub fn remove(&mut self) -> Result<(), Thrown> {
        if !self.can_remove {
            return Err(self.illegal_remove());
        }
        self.check()?;
        self.can_remove = false;
        match &self.source {
            Source::None | Source::PrimArray(_) => Err(crate::vm::uoe()),
            Source::List(l) => {
                if l.mutability() != crate::value::Mutability::Mutable {
                    if l.class == cls::INGEST_LIST {
                        return Err(crate::lib_coll::ingest_view_error("iteratorRemove"));
                    }
                    return Err(crate::vm::uoe());
                }
                self.pos -= 1;
                l.v.borrow_mut().remove(self.pos);
                l.bump();
                self.mods = l.mods.get();
                crate::lib_coll::view_exit(l);
                Ok(())
            }
            Source::Map(m, kind) => {
                let item = &self.items[self.pos - 1];
                let key = match kind {
                    ViewKind::Keys => item.clone(),
                    ViewKind::Entries => match item {
                        Value::Obj(o) => match &**o {
                            Obj::Entry(e) => e.key.clone(),
                            _ => Value::Null,
                        },
                        _ => Value::Null,
                    },
                    ViewKind::Values => {
                        // remove the entry holding this value (first match in iteration order)
                        let entries = m.entries();
                        match entries.iter().find(|(_, v)| v.java_equals(item)) {
                            Some((k, _)) => k.clone(),
                            None => return Ok(()),
                        }
                    }
                };
                m.remove(&key);
                self.mods = m.mods.get();
                Ok(())
            }
            Source::Set(m) => {
                let key = self.items[self.pos - 1].clone();
                m.remove(&key);
                self.mods = m.mods.get();
                Ok(())
            }
        }
    }
}

/// An iterator over a map's entries as live `Map.Entry` objects.
pub fn entry_values(m: &Rc<JMap>) -> Vec<Value> {
    let class = match m.order() {
        crate::value::MapOrder::Sorted => cls::TREE_MAP_ENTRY,
        crate::value::MapOrder::Insertion => cls::LINKED_ENTRY,
        crate::value::MapOrder::Hash => cls::HASH_MAP_NODE,
    };
    m.entries()
        .into_iter()
        .map(|(k, v)| Value::obj(Obj::Entry(JEntry { class, key: k, value: RefCell::new(v), map: Some(m.clone()), ctx: None })))
        .collect()
}

/// `iterable.iterator()` (and what a for-each loop iterates).
pub fn iter_of(vm: &mut Vm, v: &Value) -> Result<JIter, Thrown> {
    Ok(match v {
        Value::List(l) => JIter { class: cls::ARRAY_LIST_ITR, items: Vec::new(), pos: 0, mods: l.mods.get(), source: Source::List(l.clone()), can_remove: false },
        Value::Set(s) => JIter { class: cls::HASH_MAP_ITERATOR, items: s.keys(), pos: 0, mods: s.mods.get(), source: Source::Set(s.clone()), can_remove: false },
        Value::Obj(o) => match &**o {
            Obj::MapView { map, kind } => {
                let items = match kind {
                    ViewKind::Keys => map.keys(),
                    ViewKind::Values => map.values(),
                    ViewKind::Entries => entry_values(map),
                };
                JIter { class: cls::HASH_MAP_ITERATOR, items, pos: 0, mods: map.mods.get(), source: Source::Map(map.clone(), *kind), can_remove: false }
            }
            Obj::Array(a) => {
                let mut it = JIter::of_values(cls::GENERIC_ITERATOR, a.data.borrow().to_values());
                if let Some(p) = crate::ir::Prim::of(a.elem)
                    && p.ty() == a.elem {
                        it.source = Source::PrimArray(p);
                    }
                it
            }
            Obj::Iterator(_) => return Err(iae(format!("Cannot iterate over [{}]", crate::lookup::class_name(o.class())))),
            Obj::Es(e) => match e.iter_values(vm)? {
                Some(items) => JIter::of_values(cls::GENERIC_ITERATOR, items),
                None => return Err(iae(format!("Cannot iterate over [{}]", crate::lookup::class_name(o.class())))),
            },
            Obj::Host(h) => match h.iter() {
                Some(items) => JIter::of_values(cls::GENERIC_ITERATOR, items),
                None => return Err(iae(format!("Cannot iterate over [{}]", crate::lookup::class_name(o.class())))),
            },
            _ => match crate::lib_coll::elements_opt(vm, v)? {
                Some(items) => JIter::of_values(cls::GENERIC_ITERATOR, items),
                None => return Err(iae(format!("Cannot iterate over [{}]", crate::lookup::class_name(o.class())))),
            },
        },
        Value::Null => return Err(crate::vm::npe("Cannot invoke \"Object.getClass()\" because \"receiver\" is null")),
        other => return Err(iae(format!("Cannot iterate over [{}]", crate::lookup::class_name(other.class())))),
    })
}

pub fn iterator_of(vm: &mut Vm, v: &Value) -> VR {
    let it = iter_of(vm, v)?;
    Ok(Value::obj(Obj::Iterator(RefCell::new(it))))
}

/// Advances the iterator of a for-each loop over a `def` value into a primitive variable
/// (Elasticsearch's `ValueIterator.nextInt()` & co.): primitive arrays convert their elements
/// with Java's primitive casts, anything else unboxes `Number`s (`((Number) next).intValue()`),
/// `Boolean`s and `Character`s, failing with `Def.castException`'s message.
pub fn next_prim(vm: &mut Vm, it: &Value, to: crate::ir::Prim) -> Result<Option<Value>, Thrown> {
    use crate::ir::Prim;
    let Value::Obj(o) = it else { return Ok(None) };
    let Obj::Iterator(i) = &**o else { return Ok(None) };
    let elem = match i.borrow().source {
        Source::PrimArray(p) => Some(p),
        _ => None,
    };
    let Some(v) = next(vm, it)? else { return Ok(None) };
    if elem.is_none() {
        return value_iterator_convert(v, to).map(Some);
    }
    let fail = |v: &Value| crate::vm::cce(format!("cannot cast def [{}] to {}", crate::cast::unboxed_name(v), to.java_name()));
    Ok(Some(match (elem, to) {
        // a primitive array's iterator overrides every primitive accessor but boolean ones
        (Some(Prim::Bool), Prim::Bool) => v,
        (Some(Prim::Bool), _) => return Err(fail(&v)),
        (Some(_), Prim::Bool) => return Err(fail(&v)),
        _ => crate::cast::convert(&v, to),
    }))
}

/// `ValueIterator.next<Prim>()` of an iterator over boxed values: what a for-each loop with
/// a primitive variable over a `def` iterable assigns.
pub fn value_iterator_convert(v: Value, to: crate::ir::Prim) -> Result<Value, Thrown> {
    use crate::ir::Prim;
    let fail = |v: &Value| crate::vm::cce(format!("cannot cast def [{}] to {}", crate::cast::unboxed_name(v), to.java_name()));
    let unbox_npe = || {
        let m = match to {
            Prim::Bool => "java.lang.Boolean.booleanValue()",
            Prim::Char => "java.lang.Character.charValue()",
            Prim::Byte => "java.lang.Number.byteValue()",
            Prim::Short => "java.lang.Number.shortValue()",
            Prim::Int => "java.lang.Number.intValue()",
            Prim::Long => "java.lang.Number.longValue()",
            Prim::Float => "java.lang.Number.floatValue()",
            Prim::Double => "java.lang.Number.doubleValue()",
        };
        crate::vm::npe(format!("Cannot invoke \"{m}\" because \"next\" is null"))
    };
    Ok(match to {
        _ if v.is_null() => return Err(unbox_npe()),
        Prim::Bool => match v {
            Value::Bool(_) => v,
            _ => return Err(fail(&v)),
        },
        Prim::Char => match v {
            Value::Char(_) => v,
            _ => return Err(fail(&v)),
        },
        _ => match v {
            Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => crate::cast::convert(&v, to),
            _ => match v.other_number() {
                Some((d, l)) => match to {
                    Prim::Double => Value::Double(d),
                    Prim::Float => Value::Float(d as f32),
                    _ => crate::cast::convert(&Value::Long(l), to),
                },
                None => return Err(fail(&v)),
            },
        },
    })
}

/// Advances an iterator value; `None` when exhausted.
pub fn next(_vm: &mut Vm, it: &Value) -> Result<Option<Value>, Thrown> {
    match it {
        Value::Obj(o) => match &**o {
            Obj::Iterator(i) => i.borrow_mut().next(),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}
