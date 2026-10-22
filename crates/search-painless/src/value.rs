//! Runtime values.
//!
//! A [`Value`] is 16 bytes: a tag and either a primitive or one reference-counted pointer.
//! Boxed and primitive Java numbers share a representation (an `int` and an `Integer` are both
//! `Value::Int`), `null` is its own variant, and strings, lists and maps get dedicated variants
//! because scripts touch them constantly. Everything else lives behind [`Obj`].
//!
//! Collections have Java's aliasing semantics (`Rc<RefCell<…>>`): a list stored in a map and
//! mutated through another variable changes in both places. Values are per-script-instance and
//! single-threaded; compiled scripts never hold them.
//!
//! Maps keep insertion order and remember the capacity a `java.util.HashMap` would have, so
//! iterating a `HashMap` (or printing it, or returning it) visits entries in exactly the order
//! the JVM would: by `(hash ^ hash >>> 16) & (capacity - 1)`, then insertion order within a
//! bucket. `LinkedHashMap` keeps insertion order and `TreeMap` keeps entries sorted.

use std::cell::{Cell, RefCell};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::java;
use crate::types::{cls, ClassId, Ty};

#[derive(Clone, Default)]
#[repr(u64)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Byte(i8),
    Short(i16),
    Char(u16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(Rc<JStr>),
    List(Rc<JList>),
    Map(Rc<JMap>),
    Set(Rc<JMap>),
    Obj(Rc<Obj>),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{:?}", s.as_str()),
            Value::Null => write!(f, "null"),
            v => write!(f, "{}", v.to_java_string()),
        }
    }
}

// ------------------------------------------------------------------------------------ strings

/// A Java string: UTF-8 storage, UTF-16 semantics for indexing, a cached `hashCode`.
pub struct JStr {
    s: Box<str>,
    ascii: bool,
    hash: Cell<Option<i32>>,
}

impl JStr {
    pub fn new(s: impl Into<Box<str>>) -> JStr {
        let s: Box<str> = s.into();
        let ascii = s.is_ascii();
        JStr { s, ascii, hash: Cell::new(None) }
    }
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.s
    }
    #[inline]
    pub fn is_ascii(&self) -> bool {
        self.ascii
    }
    /// `String.length()`.
    pub fn len16(&self) -> usize {
        if self.ascii {
            self.s.len()
        } else {
            java::utf16_len(&self.s)
        }
    }
    pub fn char_at(&self, i: usize) -> Option<u16> {
        if self.ascii {
            self.s.as_bytes().get(i).map(|&b| b as u16)
        } else {
            java::utf16_at(&self.s, i)
        }
    }
    pub fn byte_index(&self, i: usize) -> Option<usize> {
        if self.ascii {
            if i <= self.s.len() {
                Some(i)
            } else {
                None
            }
        } else {
            java::utf16_to_byte(&self.s, i)
        }
    }
    pub fn java_hash(&self) -> i32 {
        if let Some(h) = self.hash.get() {
            return h;
        }
        let h = java::string_hash(&self.s);
        self.hash.set(Some(h));
        h
    }
}

// ------------------------------------------------------------------------------------ lists

/// A `java.util.List` backed by a vector. `class` is the runtime class (`ArrayList`,
/// `LinkedList`, `Arrays$ArrayList`, an immutable list, …), which also decides whether the list
/// can change size or contents.
pub struct JList {
    pub class: ClassId,
    pub v: RefCell<Vec<Value>>,
    /// `modCount`: bumped on structural changes so iterators fail fast.
    pub mods: Cell<u32>,
    /// For a `subList` view: the list it is a window into. The view keeps its own copy of the
    /// window, refreshed from the parent before and written back after each library call on it
    /// (see `lib_coll::view_enter`).
    pub view: Option<Box<SubView>>,
}

/// The link of a `subList` view to its parent.
pub struct SubView {
    pub parent: Rc<JList>,
    pub offset: usize,
    /// The window's length in the parent.
    pub len: Cell<usize>,
    /// The parent's `modCount` this view was last in sync with.
    pub expected: Cell<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutability {
    Mutable,
    /// Elements can be replaced but the size is fixed (`Arrays.asList`).
    FixedSize,
    Immutable,
}

impl JList {
    pub fn new(class: ClassId, v: Vec<Value>) -> JList {
        JList { class, v: RefCell::new(v), mods: Cell::new(0), view: None }
    }
    pub fn array_list(v: Vec<Value>) -> Value {
        Value::List(Rc::new(JList::new(cls::ARRAY_LIST, v)))
    }
    pub fn mutability(&self) -> Mutability {
        match self.class {
            cls::ARRAYS_LIST => Mutability::FixedSize,
            cls::IMMUTABLE_LIST | cls::UNMODIFIABLE_LIST | cls::EMPTY_LIST | cls::SINGLETON_LIST | cls::INGEST_LIST => Mutability::Immutable,
            _ => Mutability::Mutable,
        }
    }
    pub fn bump(&self) {
        self.mods.set(self.mods.get().wrapping_add(1));
    }
    pub fn len(&self) -> usize {
        self.v.borrow().len()
    }
    pub fn is_empty(&self) -> bool {
        self.v.borrow().is_empty()
    }
    pub fn get(&self, i: usize) -> Option<Value> {
        self.v.borrow().get(i).cloned()
    }
}

// Containers drop their contents through `guard::drop_nested`: values nest without bound, and
// a recursive drop of a deeply nested one would overflow the native stack.
impl Drop for JList {
    fn drop(&mut self) {
        let v = std::mem::take(self.v.get_mut());
        if v.iter().any(crate::guard::nests) {
            crate::guard::drop_nested(crate::guard::Deferred::Values(v));
        }
    }
}

// ------------------------------------------------------------------------------------ maps

/// A map key: a value hashed and compared with Java's `hashCode`/`equals`.
#[derive(Clone)]
pub struct Key(pub Value);

impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        self.0.java_equals(&other.0)
    }
}
impl Eq for Key {}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_i32(self.0.java_hash());
    }
}

/// Spreads a Java hash code for the hash table (Java hashes of small integers are the integers
/// themselves; hashbrown needs high bits).
#[derive(Default)]
pub struct JHasher(u64);
impl Hasher for JHasher {
    fn finish(&self) -> u64 {
        self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(26)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) ^ b as u64;
        }
    }
    fn write_i32(&mut self, i: i32) {
        self.0 = i as u32 as u64;
    }
}

pub type JIndexMap = IndexMap<Key, Value, BuildHasherDefault<JHasher>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapOrder {
    /// `java.util.HashMap` / `HashSet` bucket order.
    Hash,
    /// Insertion order (`LinkedHashMap`, documents parsed from JSON).
    Insertion,
    /// Sorted by key (`TreeMap`, `TreeSet`).
    Sorted,
}

pub struct MapData {
    pub m: JIndexMap,
    /// Table capacity a `java.util.HashMap` would have (0 before the first insert).
    pub cap: u32,
    /// Initial capacity requested by a constructor.
    pub init_cap: u32,
}

/// `java.util.Map` (and, as [`Value::Set`], `java.util.Set`: keys only).
pub struct JMap {
    pub class: ClassId,
    pub data: RefCell<MapData>,
    pub mods: Cell<u32>,
    /// Comparator of a `TreeMap`/`TreeSet` built with one (a function value).
    pub comparator: Option<Value>,
    /// Elasticsearch's `params` in search contexts resolve `_source`/`doc` lazily through the host.
    pub lazy_host: bool,
}

impl Drop for JMap {
    fn drop(&mut self) {
        let d = self.data.get_mut();
        if d.m.iter().any(|(k, v)| crate::guard::nests(&k.0) || crate::guard::nests(v)) {
            crate::guard::drop_nested(crate::guard::Deferred::Map(std::mem::take(&mut d.m)));
        }
    }
}

/// `HashMap.tableSizeFor`.
pub fn table_size_for(c: u32) -> u32 {
    if c <= 1 {
        1
    } else {
        c.next_power_of_two().min(1 << 30)
    }
}

impl JMap {
    pub fn new(class: ClassId) -> JMap {
        JMap {
            class,
            data: RefCell::new(MapData { m: JIndexMap::default(), cap: 0, init_cap: 0 }),
            mods: Cell::new(0),
            comparator: None,
            lazy_host: false,
        }
    }
    pub fn with_capacity(class: ClassId, n: usize) -> JMap {
        let m = JMap::new(class);
        m.data.borrow_mut().m.reserve(n);
        m
    }
    pub fn hash_map() -> JMap {
        JMap::new(cls::HASH_MAP)
    }
    pub fn linked() -> JMap {
        JMap::new(cls::LINKED_HASH_MAP)
    }
    pub fn order(&self) -> MapOrder {
        match self.class {
            cls::HASH_MAP | cls::HASH_SET | cls::HASHTABLE | cls::IDENTITY_HASH_MAP => MapOrder::Hash,
            cls::TREE_MAP | cls::TREE_SET => MapOrder::Sorted,
            _ => MapOrder::Insertion,
        }
    }
    pub fn mutability(&self) -> Mutability {
        match self.class {
            cls::IMMUTABLE_MAP | cls::IMMUTABLE_SET | cls::UNMODIFIABLE_MAP | cls::UNMODIFIABLE_SET | cls::EMPTY_MAP | cls::EMPTY_SET | cls::SINGLETON_MAP | cls::SINGLETON_SET | cls::INGEST_DATA => {
                Mutability::Immutable
            }
            _ => Mutability::Mutable,
        }
    }
    pub fn bump(&self) {
        self.mods.set(self.mods.get().wrapping_add(1));
    }
    pub fn len(&self) -> usize {
        self.data.borrow().m.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.borrow().m.is_empty()
    }
    pub fn get(&self, k: &Value) -> Option<Value> {
        self.data.borrow().m.get(&Key(k.clone())).cloned()
    }
    pub fn get_str(&self, k: &str) -> Option<Value> {
        let key = Key(Value::str(k));
        self.data.borrow().m.get(&key).cloned()
    }
    pub fn contains(&self, k: &Value) -> bool {
        self.data.borrow().m.contains_key(&Key(k.clone()))
    }

    /// Records one insertion for `HashMap` capacity tracking.
    fn grow(d: &mut MapData) {
        if d.cap == 0 {
            d.cap = if d.init_cap > 0 { d.init_cap } else { 16 };
        }
        while d.m.len() as u64 * 4 > d.cap as u64 * 3 && d.cap < (1 << 30) {
            d.cap *= 2;
        }
    }

    /// Inserts without key-order maintenance (callers handle sorted maps). Returns the previous
    /// value.
    pub fn insert_raw(&self, k: Value, v: Value) -> Option<Value> {
        let mut d = self.data.borrow_mut();
        let prev = d.m.insert(Key(k), v);
        if prev.is_none() {
            JMap::grow(&mut d);
            drop(d);
            self.bump();
        }
        prev
    }

    /// Inserts at a sorted position (`TreeMap`), given the index from a binary search.
    pub fn insert_at(&self, idx: usize, k: Value, v: Value) {
        let mut d = self.data.borrow_mut();
        d.m.shift_insert(idx, Key(k), v);
        drop(d);
        self.bump();
    }

    pub fn remove(&self, k: &Value) -> Option<Value> {
        let r = self.data.borrow_mut().m.shift_remove(&Key(k.clone()));
        if r.is_some() {
            self.bump();
        }
        r
    }

    pub fn clear(&self) {
        self.data.borrow_mut().m.clear();
        self.bump();
    }

    /// Sets the capacity a `new HashMap(m)` / `new HashSet(c)` copy starts with.
    pub fn presize_for_copy(&self, n: usize, set: bool) {
        let mut d = self.data.borrow_mut();
        if set {
            // HashSet(Collection): new HashMap<>(Math.max((int) (c.size()/.75f) + 1, 16))
            let want = ((n as f32 / 0.75) as u32 + 1).max(16);
            d.init_cap = table_size_for(want);
        } else if n > 0 {
            let ft = (n as f32 / 0.75) + 1.0;
            d.init_cap = table_size_for(ft as u32);
        }
    }

    /// Entry indices in the order the JVM would iterate them.
    pub fn order_indices(&self) -> Vec<usize> {
        let d = self.data.borrow();
        let n = d.m.len();
        if self.order() != MapOrder::Hash || n <= 1 {
            return (0..n).collect();
        }
        let mask = d.cap.max(1) - 1;
        let mut idx: Vec<(u32, usize)> = d
            .m
            .keys()
            .enumerate()
            .map(|(i, k)| {
                let h = k.0.java_hash() as u32;
                ((h ^ (h >> 16)) & mask, i)
            })
            .collect();
        idx.sort_unstable();
        idx.into_iter().map(|(_, i)| i).collect()
    }

    /// Snapshot of the entries in iteration order.
    pub fn entries(&self) -> Vec<(Value, Value)> {
        let order = self.order_indices();
        let d = self.data.borrow();
        order
            .into_iter()
            .map(|i| {
                let (k, v) = d.m.get_index(i).unwrap();
                (k.0.clone(), v.clone())
            })
            .collect()
    }

    pub fn keys(&self) -> Vec<Value> {
        let order = self.order_indices();
        let d = self.data.borrow();
        order.into_iter().map(|i| d.m.get_index(i).unwrap().0 .0.clone()).collect()
    }

    pub fn values(&self) -> Vec<Value> {
        let order = self.order_indices();
        let d = self.data.borrow();
        order.into_iter().map(|i| d.m.get_index(i).unwrap().1.clone()).collect()
    }
}

// ------------------------------------------------------------------------------------ arrays

/// Typed storage of a Java array.
#[derive(Clone)]
pub enum ArrayData {
    Bool(Vec<bool>),
    Byte(Vec<i8>),
    Short(Vec<i16>),
    Char(Vec<u16>),
    Int(Vec<i32>),
    Long(Vec<i64>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Ref(Vec<Value>),
}

impl ArrayData {
    pub fn len(&self) -> usize {
        match self {
            ArrayData::Bool(v) => v.len(),
            ArrayData::Byte(v) => v.len(),
            ArrayData::Short(v) => v.len(),
            ArrayData::Char(v) => v.len(),
            ArrayData::Int(v) => v.len(),
            ArrayData::Long(v) => v.len(),
            ArrayData::Float(v) => v.len(),
            ArrayData::Double(v) => v.len(),
            ArrayData::Ref(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, i: usize) -> Value {
        match self {
            ArrayData::Bool(v) => Value::Bool(v[i]),
            ArrayData::Byte(v) => Value::Byte(v[i]),
            ArrayData::Short(v) => Value::Short(v[i]),
            ArrayData::Char(v) => Value::Char(v[i]),
            ArrayData::Int(v) => Value::Int(v[i]),
            ArrayData::Long(v) => Value::Long(v[i]),
            ArrayData::Float(v) => Value::Float(v[i]),
            ArrayData::Double(v) => Value::Double(v[i]),
            ArrayData::Ref(v) => v[i].clone(),
        }
    }
    /// Stores an already type-checked value.
    pub fn set(&mut self, i: usize, x: Value) {
        match (self, x) {
            (ArrayData::Bool(v), Value::Bool(b)) => v[i] = b,
            (ArrayData::Byte(v), x) => v[i] = x.as_i64().unwrap_or(0) as i8,
            (ArrayData::Short(v), x) => v[i] = x.as_i64().unwrap_or(0) as i16,
            (ArrayData::Char(v), x) => v[i] = x.as_i64().unwrap_or(0) as u16,
            (ArrayData::Int(v), x) => v[i] = x.as_i64().unwrap_or(0) as i32,
            (ArrayData::Long(v), x) => v[i] = x.as_i64().unwrap_or(0),
            (ArrayData::Float(v), x) => v[i] = x.as_f64().unwrap_or(0.0) as f32,
            (ArrayData::Double(v), x) => v[i] = x.as_f64().unwrap_or(0.0),
            (ArrayData::Ref(v), x) => v[i] = x,
            (ArrayData::Bool(v), _) => v[i] = false,
        }
    }
    pub fn to_values(&self) -> Vec<Value> {
        (0..self.len()).map(|i| self.get(i)).collect()
    }
}

/// Bytes one element of an array of `elem` takes in [`ArrayData`].
pub fn elem_bytes(elem: Ty) -> u64 {
    if elem.dims > 0 {
        return std::mem::size_of::<Value>() as u64;
    }
    match elem.class {
        cls::BOOLEAN | cls::BYTE => 1,
        cls::SHORT | cls::CHAR => 2,
        cls::INT | cls::FLOAT => 4,
        cls::LONG | cls::DOUBLE => 8,
        _ => std::mem::size_of::<Value>() as u64,
    }
}

/// A Java array: element type (the array type minus one dimension) and storage.
pub struct JArray {
    pub elem: Ty,
    pub data: RefCell<ArrayData>,
}

impl JArray {
    pub fn new(elem: Ty, len: usize) -> JArray {
        let data = if elem.dims > 0 {
            ArrayData::Ref(vec![Value::Null; len])
        } else {
            match elem.class {
                cls::BOOLEAN => ArrayData::Bool(vec![false; len]),
                cls::BYTE => ArrayData::Byte(vec![0; len]),
                cls::SHORT => ArrayData::Short(vec![0; len]),
                cls::CHAR => ArrayData::Char(vec![0; len]),
                cls::INT => ArrayData::Int(vec![0; len]),
                cls::LONG => ArrayData::Long(vec![0; len]),
                cls::FLOAT => ArrayData::Float(vec![0.0; len]),
                cls::DOUBLE => ArrayData::Double(vec![0.0; len]),
                _ => ArrayData::Ref(vec![Value::Null; len]),
            }
        };
        JArray { elem, data: RefCell::new(data) }
    }
    pub fn from_data(elem: Ty, data: ArrayData) -> JArray {
        JArray { elem, data: RefCell::new(data) }
    }
    pub fn len(&self) -> usize {
        self.data.borrow().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The array's own type.
    pub fn ty(&self) -> Ty {
        Ty { class: self.elem.class, dims: self.elem.dims + 1 }
    }
    /// JVM type descriptor (`[I`, `[Ljava.lang.String;`) used in `toString`.
    pub fn descriptor(&self) -> String {
        let ty = self.ty();
        let mut s = "[".repeat(ty.dims as usize);
        match ty.class {
            cls::BOOLEAN => s.push('Z'),
            cls::BYTE => s.push('B'),
            cls::SHORT => s.push('S'),
            cls::CHAR => s.push('C'),
            cls::INT => s.push('I'),
            cls::LONG => s.push('J'),
            cls::FLOAT => s.push('F'),
            cls::DOUBLE => s.push('D'),
            c => {
                s.push('L');
                s.push_str(crate::lookup::java_binary_name(if c == cls::DEF { cls::OBJECT } else { c }));
                s.push(';');
            }
        }
        s
    }
}

// ------------------------------------------------------------------------------------ objects

/// Everything that is not a primitive, string, list, map or set.
pub enum Obj {
    Array(JArray),
    /// `StringBuilder` / `StringBuffer` (`class` tells which).
    Builder { class: ClassId, s: RefCell<String> },
    Exception(JException),
    Function(crate::func::FnRef),
    Iterator(RefCell<crate::iter::JIter>),
    /// A `Map.Entry`: live entries write through to their map.
    Entry(JEntry),
    /// `keySet()` / `values()` / `entrySet()` view of a map.
    MapView { map: Rc<JMap>, kind: ViewKind },
    Optional { class: ClassId, value: Option<Value> },
    Pattern(std::sync::Arc<crate::regex::Regex>),
    Matcher(RefCell<crate::regex::MatcherState>),
    Stream(RefCell<crate::stream::StreamState>),
    Collector(Rc<crate::stream::CollectorDef>),
    Random(RefCell<crate::lib_util::JRandom>),
    StringJoiner(RefCell<crate::lib_util::Joiner>),
    Tokenizer(RefCell<crate::lib_util::Tokenizer>),
    Stats(RefCell<crate::lib_util::Stats>),
    Uuid(u64, u64),
    Base64Codec { encoder: bool, url: bool, mime: bool, padding: bool },
    Locale(Rc<str>),
    LocaleBuilder(RefCell<crate::locale::Builder>),
    /// A `Character.UnicodeBlock` (index into the block table).
    UnicodeBlock(u16),
    BitSet(RefCell<Vec<u64>>),
    HexFormat(Rc<crate::lib_util::HexFmt>),
    Time(crate::time::TimeVal),
    Formatter(Rc<crate::time::format::Formatter>),
    BigInt(Rc<crate::bignum::BigInt>),
    BigDec(Rc<crate::bignum::BigDec>),
    /// `MathContext` / `RoundingMode`.
    Math(crate::bignum::MathObj),
    Es(crate::es::EsObj),
    /// `java.text` number formats and their companions, currencies.
    Text(crate::text::TextObj),
    /// `TimeZone`, `Calendar` and the legacy `java.text` date formats.
    Legacy(Box<crate::time::legacy::LegacyObj>),
    /// A host-provided object exposed to scripts.
    Host(Box<dyn crate::host::HostObject>),
}

impl Drop for JArray {
    fn drop(&mut self) {
        if let ArrayData::Ref(v) = self.data.get_mut()
            && v.iter().any(crate::guard::nests)
        {
            crate::guard::drop_nested(crate::guard::Deferred::Values(std::mem::take(v)));
        }
    }
}

impl Drop for Obj {
    fn drop(&mut self) {
        use crate::guard::{drop_nested, nests, Deferred};
        // move nested values out so the fields' own drop is shallow
        let inner = match self {
            Obj::Optional { value, .. } => value.take(),
            Obj::Entry(e) => {
                let v = std::mem::take(e.value.get_mut());
                if nests(&e.key) || nests(&v) {
                    drop_nested(Deferred::Values(vec![std::mem::take(&mut e.key), v]));
                }
                None
            }
            Obj::Function(f) => match &mut f.target {
                crate::func::Target::Script { captures, .. } => {
                    if captures.iter().any(nests) {
                        drop_nested(Deferred::Values(std::mem::take(captures)));
                    }
                    None
                }
                crate::func::Target::Method { bound, .. } => bound.take(),
                crate::func::Target::Capture { recv, .. } => Some(std::mem::take(recv)),
                _ => None,
            },
            Obj::Iterator(it) => {
                let items = std::mem::take(&mut it.get_mut().items);
                if items.iter().any(nests) {
                    drop_nested(Deferred::Values(items));
                }
                None
            }
            _ => None,
        };
        if let Some(v) = inner
            && nests(&v)
        {
            drop_nested(Deferred::One(v));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewKind {
    Keys,
    Values,
    Entries,
}

pub struct JEntry {
    pub class: ClassId,
    pub key: Value,
    pub value: RefCell<Value>,
    /// The map a live entry writes `setValue` through to.
    pub map: Option<Rc<JMap>>,
    /// The write-script `ctx` an entry of `ctx.entrySet()` writes through to (validated).
    pub ctx: Option<Rc<crate::es::write::Ctx>>,
}

/// An exception object (`new IllegalArgumentException("…")`, or caught in a `catch`).
pub struct JException {
    pub class: ClassId,
    pub message: Option<String>,
    /// Where the exception was created: a Java exception's stack trace is captured by its
    /// constructor, so a rethrown or later-thrown exception reports that location (and the
    /// library frames above it), not the `throw`.
    pub origin: RefCell<Option<Box<ExceptionOrigin>>>,
    /// `ParseException.getErrorOffset()`.
    pub error_offset: Option<i32>,
}

/// The script offset and library frames of an exception's construction.
pub struct ExceptionOrigin {
    pub offset: u32,
    pub frames: Vec<String>,
}

impl JException {
    pub fn new(class: ClassId, message: Option<String>) -> JException {
        JException { class, message, origin: RefCell::new(None), error_offset: None }
    }
}

// ------------------------------------------------------------------------------------ Value

/// A `ZonedDateTime` in UTC at epoch milliseconds (ingest timestamps, `_now`).
pub fn zoned_date_time_millis(ms: i64) -> Value {
    crate::time::zoned_utc_millis(ms)
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::new(JStr::new(s)))
    }
    pub fn string(s: String) -> Value {
        Value::Str(Rc::new(JStr::new(s)))
    }
    /// A Java `double[]` (`values` of moving-function scripts).
    pub fn double_array(v: Vec<f64>) -> Value {
        Value::obj(Obj::Array(JArray::from_data(Ty::DOUBLE, ArrayData::Double(v))))
    }
    pub fn obj(o: Obj) -> Value {
        Value::Obj(Rc::new(o))
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    /// Whether the value owns reference-counted data (dropping it does work).
    #[inline(always)]
    pub fn is_heap(&self) -> bool {
        matches!(self, Value::Str(_) | Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Obj(_))
    }

    /// The runtime class of a (non-null) value.
    pub fn class(&self) -> ClassId {
        match self {
            Value::Null => cls::OBJECT,
            Value::Bool(_) => cls::BOOLEAN_BOX,
            Value::Byte(_) => cls::BYTE_BOX,
            Value::Short(_) => cls::SHORT_BOX,
            Value::Char(_) => cls::CHARACTER,
            Value::Int(_) => cls::INTEGER,
            Value::Long(_) => cls::LONG_BOX,
            Value::Float(_) => cls::FLOAT_BOX,
            Value::Double(_) => cls::DOUBLE_BOX,
            Value::Str(_) => cls::STRING,
            Value::List(l) => l.class,
            Value::Map(m) => m.class,
            Value::Set(m) => m.class,
            Value::Obj(o) => o.class(),
        }
    }

    /// Runtime type including array dimensions.
    pub fn runtime_ty(&self) -> Ty {
        match self {
            Value::Obj(o) => match &**o {
                Obj::Array(a) => a.ty(),
                _ => Ty::of(o.class()),
            },
            v => Ty::of(v.class()),
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Byte(v) => Some(v as i64),
            Value::Short(v) => Some(v as i64),
            Value::Char(v) => Some(v as i64),
            Value::Int(v) => Some(v as i64),
            Value::Long(v) => Some(v),
            Value::Float(v) => Some(v as i64),
            Value::Double(v) => Some(v as i64),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Byte(v) => Some(v as f64),
            Value::Short(v) => Some(v as f64),
            Value::Char(v) => Some(v as f64),
            Value::Int(v) => Some(v as f64),
            Value::Long(v) => Some(v as f64),
            Value::Float(v) => Some(v as f64),
            Value::Double(v) => Some(v),
            _ => None,
        }
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// `Object.equals` (and `Objects.equals` with nulls).
    /// `(doubleValue(), longValue())` of a `Number` that is not a primitive box (`BigInteger`,
    /// `BigDecimal`, `ScriptHeuristic.LongAccessor`): `DefMath` treats those as numbers too.
    pub fn other_number(&self) -> Option<(f64, i64)> {
        let Value::Obj(o) = self else { return None };
        match &**o {
            Obj::BigInt(b) => Some((b.to_f64(), b.low_i64())),
            Obj::BigDec(d) => Some((d.to_f64(), d.to_big_int().low_i64())),
            Obj::Es(crate::es::EsObj::LongAccessor(v)) => Some((v.get() as f64, v.get())),
            _ => None,
        }
    }

    pub fn java_equals(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (Null, Null) => true,
            (Null, _) | (_, Null) => false,
            (Bool(a), Bool(b)) => a == b,
            (Byte(a), Byte(b)) => a == b,
            (Short(a), Short(b)) => a == b,
            (Char(a), Char(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Long(a), Long(b)) => a == b,
            (Float(a), Float(b)) => java::float_hash(*a) == java::float_hash(*b),
            (Double(a), Double(b)) => {
                let x = if a.is_nan() { 0x7ff8000000000000 } else { a.to_bits() };
                let y = if b.is_nan() { 0x7ff8000000000000 } else { b.to_bits() };
                x == y
            }
            (Str(a), Str(b)) => Rc::ptr_eq(a, b) || a.as_str() == b.as_str(),
            (List(_) | Map(_) | Set(_) | Obj(_), _) if crate::guard::too_deep() => false,
            (List(a), _) => {
                if let List(b) = other
                    && Rc::ptr_eq(a, b) {
                        return true;
                    }
                // the ingest-condition views inherit `Object.equals`
                if a.class == cls::INGEST_LIST {
                    return false;
                }
                match other.list_snapshot() {
                    Some(bv) => {
                        let av = a.v.borrow().clone();
                        av.len() == bv.len() && av.iter().zip(bv.iter()).all(|(x, y)| x.java_equals(y))
                    }
                    None => false,
                }
            }
            (Map(a), Map(b)) => {
                if Rc::ptr_eq(a, b) {
                    return true;
                }
                if a.class == cls::INGEST_DATA || a.len() != b.len() {
                    return false;
                }
                let entries = a.entries();
                entries.iter().all(|(k, v)| match b.get(k) {
                    Some(bv) => v.java_equals(&bv),
                    None => v.is_null() && b.contains(k),
                })
            }
            (Set(a), _) => match other.set_snapshot() {
                Some(bv) => {
                    if let Set(b) = other
                        && Rc::ptr_eq(a, b) {
                            return true;
                        }
                    a.len() == bv.len() && bv.iter().all(|x| a.contains(x))
                }
                None => false,
            },
            (Obj(a), Obj(b)) => Rc::ptr_eq(a, b) || a.equals(b, other),
            (Obj(a), _) => a.equals_value(other),
            _ => false,
        }
    }

    /// Elements of anything list-like (lists and list-like objects), for equality.
    pub fn list_snapshot(&self) -> Option<Vec<Value>> {
        match self {
            Value::List(l) => Some(l.v.borrow().clone()),
            Value::Obj(o) => match &**o {
                Obj::Es(e) => e.list_values(),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn set_snapshot(&self) -> Option<Vec<Value>> {
        match self {
            Value::Set(s) => Some(s.keys()),
            Value::Obj(o) => match &**o {
                Obj::MapView { map, kind: ViewKind::Keys } => Some(map.keys()),
                Obj::MapView { map, kind: ViewKind::Entries } => Some(
                    map.entries()
                        .into_iter()
                        .map(|(k, v)| Value::obj(Obj::Entry(JEntry { class: cls::HASH_MAP_NODE, key: k, value: RefCell::new(v), map: None, ctx: None })))
                        .collect(),
                ),
                _ => None,
            },
            _ => None,
        }
    }

    /// `Object.hashCode`.
    pub fn java_hash(&self) -> i32 {
        use Value::*;
        match self {
            Null => 0,
            Bool(b) => {
                if *b {
                    1231
                } else {
                    1237
                }
            }
            Byte(v) => *v as i32,
            Short(v) => *v as i32,
            Char(v) => *v as i32,
            Int(v) => *v,
            Long(v) => java::long_hash(*v),
            Float(v) => java::float_hash(*v),
            Double(v) => java::double_hash(*v),
            Str(s) => s.java_hash(),
            List(l) if l.class == cls::INGEST_LIST => identity_hash(&**l),
            Map(m) if m.class == cls::INGEST_DATA => identity_hash(&**m),
            List(_) | Map(_) | Set(_) if crate::guard::too_deep() => 0,
            List(l) => {
                let v = l.v.borrow();
                let mut h: i32 = 1;
                for x in v.iter() {
                    h = h.wrapping_mul(31).wrapping_add(x.java_hash());
                }
                h
            }
            Map(m) => {
                let d = m.data.borrow();
                d.m.iter().fold(0i32, |acc, (k, v)| acc.wrapping_add(k.0.java_hash() ^ v.java_hash()))
            }
            Set(m) => {
                let d = m.data.borrow();
                d.m.keys().fold(0i32, |acc, k| acc.wrapping_add(k.0.java_hash()))
            }
            Obj(o) => o.hash_code(o),
        }
    }

    /// `String.valueOf(Object)`.
    pub fn to_java_string(&self) -> String {
        let _scope = crate::guard::OuterScope::enter();
        let mut out = String::new();
        self.write_java_string(&mut out, 0);
        out
    }

    pub fn write_java_string(&self, out: &mut String, depth: usize) {
        use std::fmt::Write;
        use Value::*;
        match self {
            Null => out.push_str("null"),
            Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Byte(v) => {
                let _ = write!(out, "{v}");
            }
            Short(v) => {
                let _ = write!(out, "{v}");
            }
            Char(c) => out.push_str(&java::char_to_string(*c)),
            Int(v) => {
                let _ = write!(out, "{v}");
            }
            Long(v) => {
                let _ = write!(out, "{v}");
            }
            Float(f) => out.push_str(&java::fmt_float(*f)),
            Double(d) => out.push_str(&java::fmt_double(*d)),
            Str(s) => out.push_str(s.as_str()),
            // the ingest-condition views inherit `Object.toString`
            List(l) if l.class == cls::INGEST_LIST => {
                let _ = write!(out, "{}@{:x}", crate::lookup::java_binary_name(l.class), identity_hash(&**l) as u32);
            }
            Map(m) if m.class == cls::INGEST_DATA => {
                let _ = write!(out, "{}@{:x}", crate::lookup::java_binary_name(m.class), identity_hash(&**m) as u32);
            }
            List(_) | Map(_) | Set(_) if crate::guard::too_deep() => {}
            List(l) => {
                out.push('[');
                let items = l.v.borrow().clone();
                for (i, x) in items.iter().enumerate() {
                    if crate::guard::too_long(out.len()) {
                        return;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if let List(inner) = x
                        && Rc::ptr_eq(inner, l) {
                            out.push_str("(this Collection)");
                            continue;
                        }
                    x.write_java_string(out, depth + 1);
                }
                out.push(']');
            }
            Map(m) => {
                out.push('{');
                for (i, (k, v)) in m.entries().iter().enumerate() {
                    if crate::guard::too_long(out.len()) {
                        return;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    match k {
                        Map(km) if Rc::ptr_eq(km, m) => out.push_str("(this Map)"),
                        _ => k.write_java_string(out, depth + 1),
                    }
                    out.push('=');
                    match v {
                        Map(vm) if Rc::ptr_eq(vm, m) => out.push_str("(this Map)"),
                        _ => v.write_java_string(out, depth + 1),
                    }
                }
                out.push('}');
            }
            Set(m) => {
                out.push('[');
                for (i, k) in m.keys().iter().enumerate() {
                    if crate::guard::too_long(out.len()) {
                        return;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    k.write_java_string(out, depth + 1);
                }
                out.push(']');
            }
            Obj(o) => o.write_string(out, depth),
        }
    }

    /// The canonical type name of the runtime class (`java.util.ArrayList`).
    pub fn class_name(&self) -> String {
        crate::lookup::type_name(self.runtime_ty())
    }

    // ---- conversions to and from JSON

    /// Builds a value from JSON. Objects become maps of class `map_class` (pass
    /// `LINKED_HASH_MAP` for documents, which keep their key order), arrays become `ArrayList`s,
    /// integers `Integer` when they fit and `Long` otherwise, other numbers `Double`.
    /// A JSON document as script values with `HashMap`s (what `Json.load`, `params` and
    /// `_source` look like to scripts).
    pub fn from_json_map(j: &serde_json::Value) -> Value {
        Value::from_json(j, cls::HASH_MAP)
    }

    pub fn from_json(j: &serde_json::Value, map_class: ClassId) -> Value {
        match j {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if let Ok(i) = i32::try_from(i) {
                        Value::Int(i)
                    } else {
                        Value::Long(i)
                    }
                } else if n.as_u64().is_some() {
                    // beyond Long: Jackson produces a BigInteger
                    crate::bignum::big_integer_value(&n.to_string())
                } else {
                    Value::Double(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::str(s),
            serde_json::Value::Array(a) => JList::array_list(a.iter().map(|x| Value::from_json(x, map_class)).collect()),
            serde_json::Value::Object(o) => {
                // the XContent parser fills a default-capacity map entry by entry
                let m = JMap::with_capacity(map_class, o.len());
                for (k, v) in o {
                    m.insert_raw(Value::str(k), Value::from_json(v, map_class));
                }
                Value::Map(Rc::new(m))
            }
        }
    }

    /// Converts to JSON the way Elasticsearch's XContent writes script results: maps in
    /// iteration order, collections and arrays as arrays, dates as ISO strings, other objects
    /// through `toString`.
    ///
    /// Containers nested deeper than [`MAX_JSON_DEPTH`] levels become `null` (a deep
    /// `serde_json::Value` could not even be dropped safely), and so does a value whose JSON
    /// would be larger than the object size limit (lists that contain the same list many
    /// times); [`Value::try_to_json`] reports both as Elasticsearch would.
    pub fn to_json(&self) -> serde_json::Value {
        self.try_to_json().unwrap_or(serde_json::Value::Null)
    }

    /// [`Value::to_json`], failing on values nested deeper than Elasticsearch's writer allows
    /// (`stream_constraints_exception`) or too large to build (`out_of_memory_error`).
    pub fn try_to_json(&self) -> Result<serde_json::Value, ToJsonError> {
        let _scope = crate::guard::OuterScope::enter();
        let mut budget = crate::guard::max_object_bytes();
        match self.to_json_at(0, &mut budget) {
            Some(j) => Ok(j),
            None if budget == 0 => Err(ToJsonError { kind: "out_of_memory_error", reason: "Java heap space".into() }),
            None => Err(ToJsonError { kind: "stream_constraints_exception", reason: nesting_depth_message(MAX_JSON_DEPTH + 1) }),
        }
    }

    /// One JSON node, charged against `budget` bytes (0 once exhausted).
    fn to_json_at(&self, depth: usize, budget: &mut usize) -> Option<serde_json::Value> {
        use serde_json::Value as J;
        let cost = std::mem::size_of::<J>() + if let Value::Str(s) = self { s.as_str().len() } else { 0 };
        if *budget < cost {
            *budget = 0;
            return None;
        }
        *budget -= cost;
        if matches!(self, Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Obj(_)) && (depth >= MAX_JSON_DEPTH || crate::guard::stack_exhausted()) {
            return None;
        }
        Some(match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Byte(v) => J::from(*v),
            Value::Short(v) => J::from(*v),
            Value::Char(c) => J::String(java::char_to_string(*c)),
            Value::Int(v) => J::from(*v),
            Value::Long(v) => J::from(*v),
            Value::Float(f) => serde_json::Number::from_f64(*f as f64).map(J::Number).unwrap_or(J::String(java::fmt_float(*f))),
            Value::Double(d) => serde_json::Number::from_f64(*d).map(J::Number).unwrap_or(J::String(java::fmt_double(*d))),
            Value::Str(s) => J::String(s.as_str().to_string()),
            Value::List(l) => J::Array(l.v.borrow().iter().map(|v| v.to_json_at(depth + 1, budget)).collect::<Option<_>>()?),
            Value::Set(s) => J::Array(s.keys().iter().map(|v| v.to_json_at(depth + 1, budget)).collect::<Option<_>>()?),
            Value::Map(m) => {
                let mut o = serde_json::Map::new();
                for (k, v) in m.entries() {
                    o.insert(k.to_java_string(), v.to_json_at(depth + 1, budget)?);
                }
                J::Object(o)
            }
            Value::Obj(o) => o.to_json(depth, budget)?,
        })
    }
}

impl Obj {
    pub fn class(&self) -> ClassId {
        match self {
            Obj::Array(_) => cls::OBJECT,
            Obj::Builder { class, .. } => *class,
            Obj::Exception(e) => e.class,
            Obj::Function(f) => f.class(),
            Obj::Iterator(it) => it.borrow().class(),
            Obj::Entry(e) => e.class,
            Obj::MapView { map, kind } => match (map.order(), kind) {
                (MapOrder::Insertion, ViewKind::Keys) => cls::LINKED_KEY_SET,
                (MapOrder::Insertion, ViewKind::Values) => cls::LINKED_VALUES,
                (MapOrder::Insertion, ViewKind::Entries) => cls::LINKED_ENTRY_SET,
                (MapOrder::Sorted, ViewKind::Keys) => cls::TREE_MAP_KEY_SET,
                (MapOrder::Sorted, ViewKind::Values) => cls::TREE_MAP_VALUES,
                (MapOrder::Sorted, ViewKind::Entries) => cls::TREE_MAP_ENTRY_SET,
                (MapOrder::Hash, ViewKind::Keys) => cls::HASH_MAP_KEY_SET,
                (MapOrder::Hash, ViewKind::Values) => cls::HASH_MAP_VALUES,
                (MapOrder::Hash, ViewKind::Entries) => cls::HASH_MAP_ENTRY_SET,
            },
            Obj::Optional { class, .. } => *class,
            Obj::Pattern(_) => cls::PATTERN,
            Obj::Matcher(_) => cls::MATCHER,
            Obj::Stream(s) => s.borrow().class(),
            Obj::Collector(_) => cls::COLLECTOR_IMPL,
            Obj::Random(_) => cls::RANDOM,
            Obj::StringJoiner(_) => cls::STRING_JOINER,
            Obj::Tokenizer(_) => cls::STRING_TOKENIZER,
            Obj::Stats(s) => s.borrow().class(),
            Obj::Uuid(..) => cls::UUID,
            Obj::Base64Codec { encoder, .. } => {
                if *encoder {
                    cls::BASE64_ENCODER
                } else {
                    cls::BASE64_DECODER
                }
            }
            Obj::Locale(_) => cls::LOCALE,
            Obj::LocaleBuilder(_) => cls::LOCALE_BUILDER,
            Obj::UnicodeBlock(_) => cls::UNICODE_BLOCK,
            Obj::Math(m) => m.class(),
            Obj::BitSet(_) => cls::BIT_SET,
            Obj::HexFormat(_) => cls::HEX_FORMAT,
            Obj::Time(t) => t.class(),
            Obj::Formatter(_) => cls::DATE_TIME_FORMATTER,
            Obj::BigInt(_) => cls::BIG_INTEGER,
            Obj::BigDec(_) => cls::BIG_DECIMAL,
            Obj::Es(e) => e.class(),
            Obj::Host(h) => h.class(),
            Obj::Text(t) => t.class(),
            Obj::Legacy(l) => l.class(),
        }
    }

    fn equals(&self, other: &Obj, other_value: &Value) -> bool {
        match (self, other) {
            (Obj::Entry(a), Obj::Entry(b)) => a.key.java_equals(&b.key) && a.value.borrow().java_equals(&b.value.borrow()),
            (Obj::Optional { class: c1, value: a }, Obj::Optional { class: c2, value: b }) => {
                c1 == c2
                    && match (a, b) {
                        (Some(x), Some(y)) => x.java_equals(y),
                        (None, None) => true,
                        _ => false,
                    }
            }
            (Obj::Uuid(a1, b1), Obj::Uuid(a2, b2)) => a1 == a2 && b1 == b2,
            (Obj::Locale(a), Obj::Locale(b)) => a == b,
            (Obj::UnicodeBlock(a), Obj::UnicodeBlock(b)) => a == b,
            (Obj::Math(a), Obj::Math(b)) => a == b,
            (Obj::Time(a), Obj::Time(b)) => a.java_equals(b),
            (Obj::BigInt(a), Obj::BigInt(b)) => a == b,
            (Obj::BigDec(a), Obj::BigDec(b)) => a.java_equals(b),
            (Obj::BitSet(a), Obj::BitSet(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                let n = a.len().max(b.len());
                (0..n).all(|i| a.get(i).copied().unwrap_or(0) == b.get(i).copied().unwrap_or(0))
            }
            (Obj::Es(a), _) => a.equals(other_value),
            (Obj::Host(h), _) => h.equals(other_value),
            (Obj::Text(t), _) => t.equals(other_value),
            (Obj::Legacy(a), Obj::Legacy(b)) => a.equals(b).unwrap_or(std::ptr::eq(self, other)),
            _ => false,
        }
    }

    fn equals_value(&self, other: &Value) -> bool {
        match self {
            Obj::Es(e) => e.equals(other),
            Obj::MapView { map, kind: ViewKind::Keys } => match other.set_snapshot() {
                Some(b) => b.len() == map.len() && b.iter().all(|k| map.contains(k)),
                None => false,
            },
            _ => false,
        }
    }

    fn hash_code(&self, rc: &Obj) -> i32 {
        if crate::guard::too_deep() {
            return 0;
        }
        match self {
            Obj::Entry(e) => e.key.java_hash() ^ e.value.borrow().java_hash(),
            Obj::Optional { value, .. } => value.as_ref().map(|v| v.java_hash()).unwrap_or(0),
            Obj::Uuid(m, l) => {
                let h = m ^ l;
                ((h >> 32) as i32) ^ (h as i32)
            }
            Obj::Locale(s) => crate::locale::Loc::parse(s).hash(),
            Obj::Math(m) => m.java_hash(),
            Obj::Time(t) => t.java_hash(),
            Obj::BigInt(b) => b.java_hash(),
            Obj::BigDec(b) => b.java_hash(),
            Obj::Es(e) => e.java_hash(),
            Obj::Text(t) => t.hash().unwrap_or_else(|| identity_hash(rc)),
            Obj::Legacy(l) => l.java_hash().unwrap_or_else(|| identity_hash(rc)),
            Obj::Exception(_) | Obj::Builder { .. } | Obj::Array(_) => identity_hash(rc),
            _ => identity_hash(rc),
        }
    }

    pub fn write_string(&self, out: &mut String, depth: usize) {
        if crate::guard::too_deep() {
            return;
        }
        match self {
            Obj::Array(a) => {
                out.push_str(&a.descriptor());
                out.push('@');
                out.push_str(&format!("{:x}", identity_hash(self) as u32));
            }
            Obj::Builder { s, .. } => out.push_str(&s.borrow()),
            Obj::Exception(e) => {
                out.push_str(crate::lookup::java_binary_name(e.class));
                if let Some(m) = &e.message {
                    out.push_str(": ");
                    out.push_str(m);
                }
            }
            Obj::Entry(e) => {
                e.key.write_java_string(out, depth + 1);
                out.push('=');
                e.value.borrow().write_java_string(out, depth + 1);
            }
            Obj::MapView { map, kind } => {
                out.push('[');
                match kind {
                    ViewKind::Keys => {
                        for (i, k) in map.keys().iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            k.write_java_string(out, depth + 1);
                        }
                    }
                    ViewKind::Values => {
                        for (i, v) in map.values().iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            v.write_java_string(out, depth + 1);
                        }
                    }
                    ViewKind::Entries => {
                        for (i, (k, v)) in map.entries().iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            k.write_java_string(out, depth + 1);
                            out.push('=');
                            v.write_java_string(out, depth + 1);
                        }
                    }
                }
                out.push(']');
            }
            Obj::Optional { class, value } => {
                let name = match *class {
                    cls::OPTIONAL_INT => "OptionalInt",
                    cls::OPTIONAL_LONG => "OptionalLong",
                    cls::OPTIONAL_DOUBLE => "OptionalDouble",
                    _ => "Optional",
                };
                match value {
                    Some(v) => {
                        out.push_str(name);
                        out.push('[');
                        v.write_java_string(out, depth + 1);
                        out.push(']');
                    }
                    None => {
                        out.push_str(name);
                        out.push_str(".empty");
                    }
                }
            }
            Obj::Pattern(p) => out.push_str(p.source()),
            Obj::Matcher(m) => out.push_str(&m.borrow().to_java_string()),
            Obj::StringJoiner(j) => out.push_str(&j.borrow().to_string()),
            Obj::Stats(s) => out.push_str(&s.borrow().to_java_string()),
            Obj::Uuid(m, l) => out.push_str(&crate::lib_util::uuid_string(*m, *l)),
            Obj::Locale(s) => out.push_str(&crate::locale::Loc::display_key(s)),
            Obj::UnicodeBlock(i) => out.push_str(crate::charblocks::data().blocks[*i as usize]),
            Obj::LocaleBuilder(_) => {
                out.push_str("java.util.Locale$Builder@");
                out.push_str(&format!("{:x}", identity_hash(self) as u32));
            }
            Obj::Math(m) => out.push_str(&m.to_java_string()),
            Obj::BitSet(b) => {
                out.push('{');
                let mut first = true;
                for (wi, w) in b.borrow().iter().enumerate() {
                    for bit in 0..64 {
                        if w & (1u64 << bit) != 0 {
                            if !first {
                                out.push_str(", ");
                            }
                            first = false;
                            out.push_str(&(wi * 64 + bit).to_string());
                        }
                    }
                }
                out.push('}');
            }
            Obj::Time(t) => out.push_str(&t.to_java_string()),
            Obj::Formatter(f) => out.push_str(&f.describe()),
            Obj::BigInt(b) => out.push_str(&b.to_string()),
            Obj::BigDec(b) => out.push_str(&b.to_java_string()),
            Obj::Es(e) => e.write_string(out, depth),
            Obj::Host(h) => out.push_str(&h.to_java_string()),
            Obj::Text(t) => t.write_string(out, || identity_hash(self) as u32),
            Obj::Legacy(l) => {
                if !l.write_string(out) {
                    out.push_str(crate::lookup::java_binary_name(self.class()));
                    out.push('@');
                    out.push_str(&format!("{:x}", identity_hash(self) as u32));
                }
            }
            Obj::HexFormat(h) => out.push_str(&h.to_java_string()),
            Obj::Function(_) | Obj::Iterator(_) | Obj::Stream(_) | Obj::Collector(_) | Obj::Random(_) | Obj::Tokenizer(_) | Obj::Base64Codec { .. } => {
                out.push_str(crate::lookup::java_binary_name(self.class()));
                out.push('@');
                out.push_str(&format!("{:x}", identity_hash(self) as u32));
            }
        }
    }

    fn to_json(&self, depth: usize, budget: &mut usize) -> Option<serde_json::Value> {
        Some(match self {
            Obj::Array(a) => serde_json::Value::Array(a.data.borrow().to_values().iter().map(|v| v.to_json_at(depth + 1, budget)).collect::<Option<_>>()?),
            Obj::MapView { map, kind: ViewKind::Keys } => serde_json::Value::Array(map.keys().iter().map(|v| v.to_json_at(depth + 1, budget)).collect::<Option<_>>()?),
            Obj::MapView { map, kind: ViewKind::Values } => serde_json::Value::Array(map.values().iter().map(|v| v.to_json_at(depth + 1, budget)).collect::<Option<_>>()?),
            Obj::Time(t) => serde_json::Value::String(t.to_xcontent_string()),
            Obj::Es(e) => e.to_json(),
            Obj::Host(h) => h.to_json(),
            other => {
                let mut s = String::new();
                other.write_string(&mut s, 0);
                serde_json::Value::String(s)
            }
        })
    }
}

/// Why a value cannot become JSON: Elasticsearch's exception type and reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToJsonError {
    pub kind: &'static str,
    pub reason: String,
}

/// Nesting depth Elasticsearch's JSON writer allows (`StreamWriteConstraints`).
pub const MAX_JSON_DEPTH: usize = 1000;

/// Jackson's `StreamWriteConstraints` message for writing a container at `depth`.
pub fn nesting_depth_message(depth: usize) -> String {
    format!("Document nesting depth ({depth}) exceeds the maximum allowed ({MAX_JSON_DEPTH}, from `StreamWriteConstraints.getMaxNestingDepth()`)")
}

/// A stable stand-in for `System.identityHashCode`.
pub fn identity_hash<T: ?Sized>(p: &T) -> i32 {
    let a = p as *const T as *const u8 as usize as u64;
    let h = a.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((h >> 33) as i32) & 0x7fff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&str, i32)]) -> Value {
        let m = JMap::hash_map();
        for (k, v) in pairs {
            m.insert_raw(Value::str(k), Value::Int(*v));
        }
        Value::Map(Rc::new(m))
    }

    #[test]
    fn hash_map_iterates_in_java_order() {
        // Java: {a=1, b=2, c=3} regardless of insertion order (hashes 97, 98, 99)
        assert_eq!(hm(&[("c", 3), ("a", 1), ("b", 2)]).to_java_string(), "{a=1, b=2, c=3}");
        // "abc" hashes to 96354, "x" to 120 → buckets 2 and 8 with capacity 16
        assert_eq!(hm(&[("x", 1), ("abc", 2)]).to_java_string(), "{abc=2, x=1}");
    }

    #[test]
    fn linked_map_keeps_insertion_order() {
        let m = JMap::linked();
        m.insert_raw(Value::str("z"), Value::Int(1));
        m.insert_raw(Value::str("a"), Value::Int(2));
        assert_eq!(Value::Map(Rc::new(m)).to_java_string(), "{z=1, a=2}");
    }

    #[test]
    fn equality_follows_java() {
        assert!(Value::Int(1).java_equals(&Value::Int(1)));
        assert!(!Value::Int(1).java_equals(&Value::Long(1)));
        assert!(Value::Double(f64::NAN).java_equals(&Value::Double(f64::NAN)));
        assert!(!Value::Double(0.0).java_equals(&Value::Double(-0.0)));
        let a = JList::array_list(vec![Value::Int(1), Value::str("x")]);
        let b = Value::List(Rc::new(JList::new(cls::LINKED_LIST, vec![Value::Int(1), Value::str("x")])));
        assert!(a.java_equals(&b));
        assert_eq!(a.java_hash(), b.java_hash());
    }

    #[test]
    fn list_to_string() {
        let l = JList::array_list(vec![Value::Long(1), Value::Float(2.5), Value::Byte(3), Value::Char(65), Value::str("x"), Value::Null, Value::Bool(true)]);
        assert_eq!(l.to_java_string(), "[1, 2.5, 3, A, x, null, true]");
    }
}
