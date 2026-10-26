//! Query jumbling (`queryjumblefuncs.c`): what makes two statements the same entry, and
//! where their constants are.
//!
//! PostgreSQL jumbles a query after parse analysis (`Query`), a utility statement as parsed.
//! The engine jumbles the parse tree for both, reading a query's tree as analysis would
//! leave it where that decides the grouping: select-list aliases and schema qualifications
//! don't count, constants count by type with their locations recorded, `x IN (...)` is
//! `x = ANY (ARRAY[...])`, lists of two or more constants are squashed, ORDER BY / GROUP BY
//! positions are values, `FOR UPDATE OF` every relation is plain `FOR UPDATE`. A utility
//! statement's constants count by value and aren't normalized (but the locations
//! `query_jumble_location` marks are: SAVEPOINT's name, DEALLOCATE's, a SET's value).

use serde_json::Value as J;

/// A constant (or external parameter) location: `LocationLen`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    pub location: i32,
    /// A squashed list's length (-1: one token, measured when normalizing).
    pub len: i32,
    pub squashed: bool,
    pub extern_param: bool,
}

/// `JumbleState`.
#[derive(Debug, Default)]
pub struct Jumble {
    buf: Vec<u8>,
    pub locs: Vec<Loc>,
    pub highest_extern_param: i32,
    pub squashed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// An analyzed query's tree.
    Query,
    /// A utility statement's raw tree.
    Utility,
}

impl Jumble {
    fn put(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn record(&mut self, location: i64, extern_param: bool) {
        if location >= 0 {
            self.locs.push(Loc { location: location as i32, len: -1, squashed: false, extern_param });
        }
    }

    /// The query id: a hash of what was jumbled (1 / 2 for a zero hash, as PostgreSQL).
    fn id(&self, utility: bool) -> i64 {
        match crate::types::hash::hash_bytes_extended(&self.buf, 0) as i64 {
            0 if utility => 2,
            0 => 1,
            h => h,
        }
    }
}

/// The statement kinds that are queries (planned and run by the executor).
pub fn is_query(node: &pg_query::NodeEnum) -> bool {
    use pg_query::NodeEnum as N;
    matches!(node, N::SelectStmt(_) | N::InsertStmt(_) | N::UpdateStmt(_) | N::DeleteStmt(_) | N::MergeStmt(_))
}

/// A relation's columns, by its (possibly qualified) name: what an INSERT without a column
/// list targets.
/// A table's columns: names and type names (`pg_type.typname`).
pub type Columns<'a> = &'a dyn Fn(Option<&str>, &str) -> Option<Vec<(String, String)>>;

/// The declared parameter types of the one procedure `schema.name` with `n` parameters
/// (None: no such procedure, or more than one).
pub type Procs<'a> = &'a dyn Fn(Option<&str>, &str, usize) -> Option<Vec<String>>;

/// Jumble a statement: its query id and constant locations.
///
/// `vars`: names that are parameters (a function's arguments, PL/pgSQL variables), with
/// their parameter numbers.
pub fn jumble(node: &pg_query::NodeEnum, columns: Columns, procs: Procs, vars: &[(String, i32)]) -> (i64, Jumble) {
    let v = serde_json::to_value(node).unwrap_or(J::Null);
    let mut j = Jumble::default();
    let utility = !is_query(node);
    let mut w = Walker { j: &mut j, from_rels: Vec::new(), scope: Vec::new(), columns, procs, target: None, vars };
    if utility {
        w.j.put("Query:utility");
        w.value(&v, Mode::Utility);
    } else {
        w.j.put("Query");
        w.value(&v, Mode::Query);
    }
    if j.squashed {
        j.highest_extern_param = 0;
    }
    (j.id(utility), j)
}

struct Walker<'a> {
    j: &'a mut Jumble,
    /// The relations of the FROM clauses being walked (FOR UPDATE OF them all is plain).
    from_rels: Vec<Vec<String>>,
    /// The relations seen so far, by the name they are known by, with their columns.
    scope: Vec<(String, Vec<(String, String)>)>,
    columns: Columns<'a>,
    procs: Procs<'a>,
    /// The columns of the INSERT / MERGE target being walked.
    target: Option<Vec<(String, String)>>,
    vars: &'a [(String, i32)],
}

/// A parse node's type and fields: tagged (`{"SelectStmt": {...}}`) or, for a struct held
/// directly (`relation: RangeVar`), recognised by its fields.
fn typed(v: &J) -> Option<(&str, &serde_json::Map<String, J>)> {
    let o = v.as_object()?;
    if o.len() == 1 {
        let (k, inner) = o.iter().next()?;
        if k == "node" {
            return typed(inner);
        }
        if k.starts_with(|c: char| c.is_ascii_uppercase()) {
            return Some((k.as_str(), inner.as_object()?));
        }
    }
    let has = |f: &str| o.contains_key(f);
    let name = if has("relname") && has("relpersistence") {
        "RangeVar"
    } else if has("aliasname") && has("colnames") {
        "Alias"
    } else if has("names") && has("typemod") && has("array_bounds") {
        "TypeName"
    } else if has("funcname") && has("agg_star") {
        "FuncCall"
    } else if has("ctename") && has("ctequery") {
        "CommonTableExpr"
    } else if has("target_list") && has("larg") {
        "SelectStmt"
    } else {
        return None;
    };
    Some((name, o))
}

/// A parse location field (ParseLoc), which jumbling skips.
fn is_location(field: &str) -> bool {
    field == "location" || field.ends_with("_location") || field == "stmt_len" || field.ends_with("list_start") || field.ends_with("list_end")
}

fn int(v: &J) -> i64 {
    v.as_i64().unwrap_or(-1)
}

fn string_list(v: &J) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| typed(n).and_then(|(t, o)| (t == "String").then(|| o.get("sval")?.as_str().map(str::to_string)).flatten()))
                .collect()
        })
        .unwrap_or_default()
}

impl Walker<'_> {
    fn value(&mut self, v: &J, mode: Mode) {
        match v {
            J::Null => self.j.put("~"),
            J::Bool(b) => self.j.put(if *b { "t" } else { "f" }),
            J::Number(n) => self.j.put(&n.to_string()),
            J::String(s) => self.j.put(s),
            J::Array(a) => {
                self.j.put("[");
                for x in a {
                    self.value(x, mode);
                }
                self.j.put("]");
            }
            J::Object(o) => match typed(v) {
                Some((t, f)) => self.node(t, f, mode),
                None => self.fields("", o, mode, &[]),
            },
        }
    }

    /// The fields of a node (but its locations and `skip`), by name.
    fn fields(&mut self, ty: &str, o: &serde_json::Map<String, J>, mode: Mode, skip: &[&str]) {
        self.j.put(ty);
        for (k, v) in o {
            if is_location(k) || skip.contains(&k.as_str()) {
                continue;
            }
            self.j.put(k);
            self.value(v, mode);
        }
    }

    fn node(&mut self, t: &str, o: &serde_json::Map<String, J>, mode: Mode) {
        let f = |k: &str| o.get(k).unwrap_or(&J::Null);
        match (mode, t) {
            (Mode::Utility, "AConst") => {
                self.j.put("A_Const");
                self.value(f("isnull"), mode);
                self.value(f("val"), mode);
            }
            (Mode::Utility, "TransactionStmt") => {
                self.fields(t, o, mode, &["savepoint_name", "gid"]);
                self.j.record(int(f("location")), false);
            }
            (Mode::Utility, "DeallocateStmt") => {
                self.fields(t, o, mode, &["name"]);
                self.j.record(int(f("location")), false);
            }
            (Mode::Utility, "VariableSetStmt") => {
                self.j.put(t);
                self.value(f("kind"), mode);
                self.value(f("name"), mode);
                if f("jumble_args").as_bool() == Some(true) {
                    self.value(f("args"), mode);
                }
                self.value(f("is_local"), mode);
                self.j.record(int(f("location")), false);
            }
            // Statements holding an analyzed query.
            (Mode::Utility, "ExplainStmt" | "CreateTableAsStmt" | "DeclareCursorStmt") => {
                self.fields(t, o, mode, &["query"]);
                self.query(f("query"));
            }
            // CALL: the function call as analyzed (funcexpr); the raw one is ignored.
            // (Its arguments are the procedure's parameters' types once analyzed, a NULL's
            // too, when the procedure is known.)
            (Mode::Utility, "CallStmt") => {
                self.j.put(t);
                if !self.call_args(f("funccall")) {
                    self.value(f("funccall"), Mode::Query);
                }
            }
            (Mode::Utility, _) => self.fields(t, o, mode, &[]),
            (Mode::Query, _) => self.query_node(t, o),
        }
    }

    /// A CALL's procedure and constant arguments as analysis types them (funcexpr and
    /// outargs: each a Const of its parameter's type): false when the procedure isn't one
    /// known procedure, or an argument is named.
    fn call_args(&mut self, call: &J) -> bool {
        let Some(("FuncCall", o)) = typed(call) else { return false };
        let mut names = string_list(o.get("funcname").unwrap_or(&J::Null));
        let Some(name) = names.pop() else { return false };
        let args = o.get("args").and_then(J::as_array).cloned().unwrap_or_default();
        if args.iter().any(|a| matches!(typed(a), Some(("NamedArgExpr", _)))) {
            return false;
        }
        let Some(types) = (self.procs)(names.last().map(String::as_str), &name, args.len()) else {
            return false;
        };
        self.j.put("FuncExpr");
        self.j.put(&name);
        for (a, ty) in args.iter().zip(&types) {
            match typed(a) {
                Some(("AConst", c)) => self.constant(ty, int(c.get("location").unwrap_or(&J::Null))),
                _ => self.value(a, Mode::Query),
            }
        }
        true
    }

    /// An analyzed query (a nested statement's).
    fn query(&mut self, v: &J) {
        self.j.put("Query");
        self.value(v, Mode::Query);
    }
}

/// The type a constant has after analysis, as far as the literal tells: an integer is int4
/// (int8 when bigger), a decimal numeric, a string unknown until it meets its context.
fn const_class(o: &serde_json::Map<String, J>) -> String {
    // (NULL, like a string, takes its type from where it is.)
    if o.get("isnull").and_then(J::as_bool) == Some(true) {
        return "unknown".into();
    }
    let Some(val) = o.get("val").and_then(J::as_object).and_then(|v| v.iter().next()) else {
        return "unknown".into();
    };
    match val.0.as_str() {
        "Ival" => "int4".into(),
        "Fval" => {
            let s = val.1.get("fval").and_then(J::as_str).unwrap_or("");
            if s.parse::<i64>().is_ok() { "int8".into() } else { "numeric".into() }
        }
        "Boolval" => "bool".into(),
        "Bsval" => "bit".into(),
        _ => "unknown".into(),
    }
}

/// A type name as analysis resolves it (`pg_catalog.` dropped), with its modifiers.
fn type_name(o: &serde_json::Map<String, J>) -> String {
    let mut names = string_list(o.get("names").unwrap_or(&J::Null));
    if names.len() > 1 && names[0] == "pg_catalog" {
        names.remove(0);
    }
    let mut s = names.join(".");
    if let Some(m) = o.get("typmods").and_then(J::as_array).filter(|m| !m.is_empty()) {
        s.push_str(&serde_json::to_string(m).unwrap_or_default());
    }
    if let Some(b) = o.get("array_bounds").and_then(J::as_array).filter(|b| !b.is_empty()) {
        s.push_str(&"[]".repeat(b.len()));
    }
    s
}

/// What `IsSquashableConstant` accepts, read off the parse tree: constants, external
/// parameters, and casts of them.
fn squashable(v: &J) -> bool {
    match typed(v) {
        Some(("AConst", _)) | Some(("ParamRef", _)) => true,
        Some(("TypeCast", o)) => o.get("arg").is_some_and(squashable),
        Some(("FuncCall", o)) => cast_call(o).is_some(),
        _ => false,
    }
}

/// Whether IN-list values have a common type (`select_common_type`), as far as their
/// literals and casts tell: numbers mix, other known types only with themselves.
fn common_type(items: &[J]) -> bool {
    let known = |v: &J| -> Option<String> {
        match typed(v)? {
            ("TypeCast", o) => typed(o.get("type_name")?).map(|(_, t)| type_name(t)),
            ("AConst", a) => Some(const_class(a)).filter(|c| c != "unknown"),
            ("FuncCall", o) => cast_call(o),
            _ => None,
        }
    };
    let numeric = |t: &str| matches!(t, "int2" | "int4" | "int8" | "numeric" | "float4" | "float8");
    let types: Vec<String> = items.iter().filter_map(known).collect();
    types.windows(2).all(|w| w[0] == w[1] || (numeric(&w[0]) && numeric(&w[1])))
}

/// A value's type as far as its literal or cast tells (`None`: unknown, or not told).
fn value_type(v: &J) -> Option<String> {
    match typed(v)? {
        ("TypeCast", o) => typed(o.get("type_name")?).map(|(_, t)| {
            let mut names = string_list(t.get("names").unwrap_or(&J::Null));
            let base = names.pop().unwrap_or_default();
            let array = t.get("array_bounds").and_then(J::as_array).is_some_and(|b| !b.is_empty());
            if array { format!("_{base}") } else { base }
        }),
        ("AConst", a) => Some(const_class(a)).filter(|c| c != "unknown"),
        ("FuncCall", o) => cast_call(o),
        _ => None,
    }
}

/// The element type of an array (an IN list's, with the type of the value it is
/// compared with): empty when an element's type isn't told by a literal or cast.
fn element_type(elems: &[J], with: Option<String>) -> String {
    let mut types = Vec::new();
    for v in elems {
        match value_type(v) {
            Some(t) => types.push(t),
            // (An untyped literal takes the others' type.)
            None if matches!(typed(v), Some(("AConst", _))) => {}
            None => return String::new(),
        }
    }
    resolve_common(with.into_iter().chain(types))
}

/// `select_common_type` over known types: the first, unless a later number type is one
/// it implicitly casts to (oid, the numeric category's preferred type, takes the integers);
/// all unknown is text.
fn resolve_common(types: impl IntoIterator<Item = String>) -> String {
    const NUMBERS: [&str; 6] = ["int2", "int4", "int8", "numeric", "float4", "float8"];
    let rank = |t: &str| NUMBERS.iter().position(|n| *n == t);
    let integer = |t: &str| matches!(t, "int2" | "int4" | "int8");
    let mut common: Option<String> = None;
    for t in types {
        common = Some(match common {
            Some(c) if integer(&c) && t == "oid" => t,
            Some(c) => match (rank(&c), rank(&t)) {
                (Some(a), Some(b)) if b > a => t,
                _ => c,
            },
            None => t,
        });
    }
    common.unwrap_or_else(|| "text".into())
}

/// A function-call cast analysis turns into the constant itself (`int4(1)`): its class.
fn cast_call(o: &serde_json::Map<String, J>) -> Option<String> {
    let name = string_list(o.get("funcname")?).pop()?;
    let args = o.get("args")?.as_array()?;
    let [arg] = args.as_slice() else { return None };
    let (t, a) = typed(arg)?;
    if t != "AConst" || o.get("over").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let class = const_class(a);
    (class == name || (class == "unknown" && name == "text")).then_some(name)
}

impl Walker<'_> {
    fn constant(&mut self, class: &str, location: i64) {
        self.j.put("Const");
        self.j.put(class);
        self.j.record(location, false);
    }

    fn query_node(&mut self, t: &str, o: &serde_json::Map<String, J>) {
        let q = Mode::Query;
        let f = |k: &str| o.get(k).unwrap_or(&J::Null);
        match t {
            "AConst" => self.constant(&const_class(o), int(f("location"))),
            "ParamRef" => {
                let n = int(f("number"));
                self.j.put("Param");
                self.j.put(&n.to_string());
                self.j.record(int(f("location")), true);
                self.j.highest_extern_param = self.j.highest_extern_param.max(n as i32);
            }
            "TypeCast" => {
                let tn = typed(f("type_name")).map(|(_, o)| type_name(o)).unwrap_or_default();
                match typed(f("arg")) {
                    // A literal read as the type, or one already of it: the constant.
                    Some(("AConst", a)) if const_class(a) == "unknown" || const_class(a) == tn => {
                        self.constant(&tn, int(a.get("location").unwrap_or(&J::Null)))
                    }
                    _ => {
                        self.j.put("Cast");
                        self.j.put(&tn);
                        self.value(f("arg"), q);
                    }
                }
            }
            "FuncCall" => match cast_call(o) {
                Some(class) => {
                    let arg = &f("args").as_array().map(|a| a[0].clone()).unwrap_or(J::Null);
                    let loc = typed(arg).map_or(-1, |(_, a)| int(a.get("location").unwrap_or(&J::Null)));
                    self.constant(&class, loc)
                }
                None => {
                    self.j.put("Func");
                    self.j.put(string_list(f("funcname")).last().map_or("", String::as_str));
                    self.fields("", o, q, &["funcname", "funcformat"]);
                }
            },
            // A variable of the function running the statement: a parameter.
            "ColumnRef" if !self.vars.is_empty() => {
                let names = string_list(f("fields"));
                let var = match names.as_slice() {
                    [n] => self.vars.iter().rev().find(|(v, _)| v.eq_ignore_ascii_case(n)).map(|v| v.1),
                    _ => None,
                };
                match var {
                    Some(id) => {
                        self.j.put("Param");
                        self.j.put(&id.to_string());
                        self.j.record(int(f("location")), true);
                        self.j.highest_extern_param = self.j.highest_extern_param.max(id);
                    }
                    None => self.fields(t, o, q, &[]),
                }
            }
            "AExpr" => self.aexpr(o),
            "AArrayExpr" => {
                let ty = element_type(f("elements").as_array().map(Vec::as_slice).unwrap_or(&[]), None);
                self.array(f("elements"), &ty, int(f("list_start")), int(f("list_end")))
            }
            "ResTarget" => self.fields(t, o, q, &[]),
            "SelectStmt" => self.select(o),
            "SortBy" => {
                self.j.put(t);
                self.position(f("node"));
                self.fields("", o, q, &["node"]);
            }
            "RangeVar" => self.rte(o),
            "Alias" => {
                self.j.put("eref");
                self.value(f("aliasname"), q);
            }
            "InsertStmt" | "UpdateStmt" | "DeleteStmt" | "MergeStmt" => {
                // (A column list naming every column, in order, is the list analysis
                // makes without one.)
                let rel = typed(f("relation")).map(|(_, r)| r);
                let schema = rel.and_then(|r| r.get("schemaname")).and_then(J::as_str).filter(|s| !s.is_empty());
                let name = rel.and_then(|r| r.get("relname")).and_then(J::as_str).unwrap_or("");
                let saved = std::mem::replace(&mut self.target, (self.columns)(schema, name));
                self.j.put(t);
                for (k, v) in o {
                    if is_location(k) || k == "override" {
                        continue;
                    }
                    self.j.put(k);
                    match k.as_str() {
                        "returning_list" => self.targets(v),
                        "cols" => self.insert_cols(v),
                        _ => self.value(v, q),
                    }
                }
                self.target = saved;
            }
            "MergeWhenClause" => {
                self.j.put(t);
                for (k, v) in o {
                    if is_location(k) {
                        continue;
                    }
                    self.j.put(k);
                    if k == "target_list" && f("command_type").as_i64() == Some(pg_query::protobuf::CmdType::CmdInsert as i64) {
                        self.insert_cols(v);
                    } else {
                        self.value(v, q);
                    }
                }
            }
            "LockingClause" => self.locking(o),
            _ => self.fields(t, o, q, &[]),
        }
    }

    /// A FROM relation: its range table entry (the name it is known by, `eref`, not its
    /// schema), and whether it includes inheritance children.
    fn rte(&mut self, o: &serde_json::Map<String, J>) {
        let alias = o.get("alias").and_then(|a| a.get("aliasname")).and_then(J::as_str);
        let name = alias.or_else(|| o.get("relname").and_then(J::as_str)).unwrap_or("");
        self.j.put("RTE");
        self.j.put(name);
        self.value(o.get("inh").unwrap_or(&J::Null), Mode::Query);
        if let Some(rels) = self.from_rels.last_mut() {
            rels.push(name.to_string());
        }
        let schema = o.get("schemaname").and_then(J::as_str).filter(|s| !s.is_empty());
        let rel = o.get("relname").and_then(J::as_str).unwrap_or("");
        if let Some(cols) = (self.columns)(schema, rel) {
            self.scope.push((name.to_string(), cols));
        }
    }

    /// The type of a column reference, when it names a column of a relation in scope.
    fn column_type(&self, v: &J) -> Option<String> {
        let Some(("ColumnRef", o)) = typed(v) else { return None };
        let names = string_list(o.get("fields")?);
        let (rel, col) = match names.as_slice() {
            [c] => (None, c),
            [r, c] => (Some(r.as_str()), c),
            _ => return None,
        };
        self.scope.iter().rev().filter(|(n, _)| rel.is_none_or(|r| r == n)).find_map(|(_, cols)| {
            cols.iter().find(|(n, _)| n == col).map(|(_, t)| t.clone())
        })
    }

    /// An INSERT's column list: none when it names every column of the target in order.
    fn insert_cols(&mut self, v: &J) {
        let names: Vec<String> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| typed(t).and_then(|(_, r)| r.get("name")?.as_str().map(str::to_string)))
            .collect();
        let plain = v.as_array().into_iter().flatten().all(|t| {
            typed(t).is_some_and(|(_, r)| r.get("indirection").and_then(J::as_array).is_none_or(Vec::is_empty))
        });
        if plain && self.target.as_ref().is_some_and(|c| c.iter().map(|c| &c.0).eq(names.iter())) {
            // (As an empty list.)
            self.j.put("[");
            self.j.put("]");
            return;
        }
        self.value(v, Mode::Query);
    }

    /// A select list or RETURNING list: the target entries' names don't count.
    fn targets(&mut self, v: &J) {
        self.j.put("[");
        for t in v.as_array().into_iter().flatten() {
            match typed(t) {
                Some(("ResTarget", r)) => self.fields("TE", r, Mode::Query, &["name"]),
                _ => self.value(t, Mode::Query),
            }
        }
        self.j.put("]");
    }

    /// An ORDER BY / GROUP BY item: an integer constant is a select-list position.
    fn position(&mut self, v: &J) {
        match typed(v) {
            Some(("AConst", a)) if const_class(a) == "int4" => {
                self.j.put("SortGroupRef");
                self.value(a.get("val").unwrap_or(&J::Null), Mode::Query);
            }
            _ => self.value(v, Mode::Query),
        }
    }

    /// `a op b`, `a IN (...)`, `a op ANY / ALL (...)`: an IN list is the array of an
    /// `= ANY` (`<> ALL` for NOT IN), a one-value list the comparison itself.
    fn aexpr(&mut self, o: &serde_json::Map<String, J>) {
        use pg_query::protobuf::AExprKind as K;
        let q = Mode::Query;
        let f = |k: &str| o.get(k).unwrap_or(&J::Null);
        let kind = int(f("kind")) as i32;
        let op = string_list(f("name")).pop().unwrap_or_default();
        if kind == K::AexprIn as i32 {
            let items = match typed(f("rexpr")) {
                Some(("List", l)) => l.get("items").cloned().unwrap_or(J::Array(Vec::new())),
                _ => J::Array(vec![f("rexpr").clone()]),
            };
            if let Some([one]) = items.as_array().map(Vec::as_slice) {
                self.j.put("OpExpr");
                self.j.put(&op);
                self.value(f("lexpr"), q);
                self.value(one, q);
                return;
            }
            // Values with no common type: the comparisons ORed (NOT IN: ANDed).
            if !common_type(items.as_array().map(Vec::as_slice).unwrap_or(&[])) {
                self.j.put(if op == "<>" { "BoolExpr:AND" } else { "BoolExpr:OR" });
                for it in items.as_array().into_iter().flatten() {
                    self.j.put("OpExpr");
                    self.j.put(&op);
                    self.value(f("lexpr"), q);
                    self.value(it, q);
                }
                return;
            }
            self.j.put("ScalarArrayOpExpr");
            self.j.put(&op);
            self.j.put(if op == "<>" { "all" } else { "any" });
            self.value(f("lexpr"), q);
            let elems = items.as_array().map(Vec::as_slice).unwrap_or(&[]);
            let ty = element_type(elems, self.column_type(f("lexpr")));
            self.array(&items, &ty, int(f("rexpr_list_start")), int(f("rexpr_list_end")));
            return;
        }
        if kind == K::AexprOpAny as i32 || kind == K::AexprOpAll as i32 {
            self.j.put("ScalarArrayOpExpr");
            self.j.put(&op);
            self.j.put(if kind == K::AexprOpAll as i32 { "all" } else { "any" });
            self.value(f("lexpr"), q);
            self.value(f("rexpr"), q);
            return;
        }
        self.j.put("AExpr");
        self.j.put(&kind.to_string());
        self.j.put(&op);
        self.value(f("lexpr"), q);
        self.value(f("rexpr"), q);
    }

    /// An ARRAY[...] (or IN list): `_jumbleElements` squashes two or more constants into
    /// one location, the list's text between its brackets.
    fn array(&mut self, elements: &J, element_type: &str, start: i64, end: i64) {
        self.j.put("ArrayExpr");
        self.j.put(element_type);
        let elems = elements.as_array().map(Vec::as_slice).unwrap_or(&[]);
        if elems.len() >= 2 && elems.iter().all(squashable) && start > 0 && end > 0 {
            self.j.locs.push(Loc { location: start as i32 + 1, len: (end - start - 1) as i32, squashed: true, extern_param: false });
            self.j.squashed = true;
            return;
        }
        self.value(elements, Mode::Query);
    }

    fn select(&mut self, o: &serde_json::Map<String, J>) {
        let q = Mode::Query;
        self.j.put("SelectStmt");
        self.from_rels.push(Vec::new());
        // (FROM first: FOR UPDATE OF is read against its relations.)
        for k in ["from_clause", "with_clause", "into_clause", "distinct_clause", "target_list", "where_clause", "group_clause", "group_distinct", "having_clause", "window_clause", "values_lists", "sort_clause", "limit_offset", "limit_count", "limit_option", "locking_clause", "op", "all", "larg", "rarg"] {
            let v = o.get(k).unwrap_or(&J::Null);
            self.j.put(k);
            match k {
                "target_list" => self.targets(v),
                "group_clause" => {
                    self.j.put("[");
                    for g in v.as_array().into_iter().flatten() {
                        self.position(g);
                    }
                    self.j.put("]");
                }
                _ => self.value(v, q),
            }
        }
        self.from_rels.pop();
    }

    /// FOR UPDATE / SHARE: `OF` every relation, in order, marks what the plain clause does.
    fn locking(&mut self, o: &serde_json::Map<String, J>) {
        let q = Mode::Query;
        let f = |k: &str| o.get(k).unwrap_or(&J::Null);
        self.j.put("LockingClause");
        self.value(f("strength"), q);
        self.value(f("wait_policy"), q);
        let named: Vec<String> = f("locked_rels")
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| typed(r).and_then(|(_, o)| o.get("relname")?.as_str().map(str::to_string)))
            .collect();
        let all = self.from_rels.last().is_some_and(|rels| *rels == named);
        self.j.put(&if all { String::new() } else { named.join(",") });
    }
}


