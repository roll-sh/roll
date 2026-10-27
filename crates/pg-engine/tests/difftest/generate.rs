//! Random schemas, data and statements for the differential tester (`tests/difftest.rs`).
//!
//! Everything comes from one seeded generator, so a seed reproduces a run exactly. The
//! generator keeps the statements it makes deterministic in PostgreSQL itself: no volatile
//! functions, float data that sums exactly in any order, aggregates whose result depends on
//! input order only with an ORDER BY over the aggregated value, LIMIT / DISTINCT ON / row-order
//! window functions only under an ORDER BY that fixes the rows.

use std::fmt::Write as _;

#[path = "round4.rs"]
mod round4;

/// splitmix64: small, fast and good enough to drive a generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ty {
    Int,
    Big,
    Small,
    Num,
    Flt,
    Text,
    Bool,
    Date,
    Ts,
    Tstz,
    Intv,
    IntArr,
    TextArr,
    Jsonb,
    Json,
}

use Ty::*;

/// Types the generator asks for when it needs "some comparable value".
const CMP_TYPES: &[Ty] = &[
    Int, Int, Int, Big, Num, Num, Flt, Text, Text, Text, Bool, Date, Ts, Tstz, Intv, IntArr,
    TextArr, Jsonb,
];
const COLUMN_TYPES: &[Ty] = &[
    Int, Int, Int, Big, Small, Num, Num, Flt, Text, Text, Text, Bool, Date, Ts, Tstz, Intv,
    IntArr, TextArr, Jsonb, Json,
];

impl Ty {
    pub fn sql(self) -> &'static str {
        match self {
            Int => "int4",
            Big => "int8",
            Small => "int2",
            Num => "numeric",
            Flt => "float8",
            Text => "text",
            Bool => "bool",
            Date => "date",
            Ts => "timestamp",
            Tstz => "timestamptz",
            Intv => "interval",
            IntArr => "int4[]",
            TextArr => "text[]",
            Jsonb => "jsonb",
            Json => "json",
        }
    }
    /// Has `=` and an ordering (json has neither).
    pub fn comparable(self) -> bool {
        self != Json
    }
    fn numeric_like(self) -> bool {
        matches!(self, Int | Big | Small | Num | Flt)
    }
}

#[derive(Clone, Debug)]
pub struct Col {
    pub name: String,
    pub ty: Ty,
    /// An explicit collation of a text column.
    pub coll: Option<&'static str>,
    /// A generated column (round 3): DML doesn't write it.
    pub generated: bool,
    /// The domain the column is declared as (round 3).
    pub dom: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub cols: Vec<Col>,
    /// The primary key column (`id`), when the table has one.
    pub key: Option<String>,
    /// Partitioned tables and inheritance parents: DML on them routes rows.
    pub is_view: bool,
    /// Rows may be inserted, updated, deleted (not a view or an inheritance child's parent
    /// with differing columns).
    pub writable: bool,
    /// Row and statement triggers on it write `audit3` (round 3).
    pub audited: bool,
}

#[derive(Clone, Debug)]
pub struct Func {
    pub name: String,
    pub args: Vec<Ty>,
    pub ret: Ty,
}

#[derive(Default)]
pub struct Schema {
    pub tables: Vec<Table>,
    pub funcs: Vec<Func>,
    /// Set-returning functions: name, argument types, output columns.
    pub srfs: Vec<(String, Vec<Ty>, Vec<(String, Ty)>)>,
    /// Round 3: the composite type `ct3 (a text, b int)` and its table `tc3 (id, c, ca, n)`
    /// exist, ct3's field `a` under this collation; and the range type `ctr3` over it.
    pub composite: Option<(&'static str, bool)>,
    /// Round 4: the tables `tj4` and `tp4` exist (`round4::schema`).
    pub round4: bool,
}

/// A column visible to an expression: its qualified reference and type.
#[derive(Clone, Debug)]
pub struct SCol {
    pub expr: String,
    pub ty: Ty,
    pub coll: Option<&'static str>,
}

/// A generated query: its text, whether its ORDER BY fixes the order of the rows it returns,
/// and its output columns.
pub struct Query {
    pub sql: String,
    pub ordered: bool,
    pub cols: Vec<(String, Ty)>,
}

/// A statement in a run.
pub enum Stmt {
    /// A query compared row for row (the oracles also run on it).
    Query(Query),
    /// DML; after it, the table's contents are compared too.
    Dml { sql: String, table: String, ordered: bool },
    /// Anything else (settings, DO blocks, transactions), compared as is.
    Other(String),
}

/// What an expression may refer to.
#[derive(Clone, Default)]
struct Cx {
    /// Columns (or grouped expressions) usable as values.
    atoms: Vec<SCol>,
    /// Outer query columns (correlation), used less often.
    outer: Vec<SCol>,
    /// When set, aggregates may appear, over these columns.
    agg: Option<Vec<SCol>>,
    /// Window functions may appear (in a select list over `agg` or `atoms`).
    window: bool,
    /// Columns a window's ORDER BY can use to order rows totally (all of the level's columns).
    total: Vec<String>,
}

pub struct Gen<'a> {
    pub rng: Rng,
    pub schema: &'a Schema,
    alias: usize,
    /// Nesting depth of sub-selects, to keep statements small.
    sub_depth: u32,
    /// CTE names visible to the query being built: name, columns.
    ctes: Vec<(String, Vec<(String, Ty)>)>,
    /// The wider grammar of round 18 (`wide_expr`, more statement kinds): seeds from
    /// `WIDE_SEEDS` on, so the smoke seeds keep their statements.
    pub wide: bool,
}

/// The first seed generated with the wider grammar.
pub const WIDE_SEEDS: u64 = 1000;
/// The first seed generated with round 3's schema objects and statements: composite types
/// with collated fields, domains, generated columns, triggers, NULL-heavy data and a
/// nondeterministic collation.
pub const R3_SEEDS: u64 = 3000;
/// The first seed generated with round 4's tables and statements (`round4.rs`): JSON and
/// SQL/JSON, window frames, LATERAL set-returning functions, MERGE and RETURNING old / new over
/// sub-partitioned tables with default partitions, numeric edges, time zones.
pub const R4_SEEDS: u64 = 4000;

const TEXTS: &[&str] = &[
    "a", "b", "A", "B", "ab", "abc", "", " x", "x ", "é", "Z", "a%", "a_b", "10", "9", "ß",
    "Ab", "ba", "aa", "z", "Ä", "foo", "bar", "a,b",
];
const COLLS: &[&str] = &["C", "POSIX", "und-x-icu", "en-x-icu", "default"];
/// Round 3's column collations: a nondeterministic one too (`ci3`).
const COLLS3: &[&str] = &["C", "POSIX", "und-x-icu", "en-x-icu", "ci3", "ci3"];
/// Time zones the wider grammar converts through (DST changes, offsets, POSIX-style names).
const TZS: &[&str] = &["UTC", "America/New_York", "Europe/Berlin", "Asia/Kolkata", "Australia/Lord_Howe", "+03", "EST5EDT", "Pacific/Chatham"];

pub fn literal(rng: &mut Rng, ty: Ty) -> String {
    match ty {
        Int => match rng.below(20) {
            0 => "2147483647".into(),
            1 => "(-2147483648)".into(),
            2..=4 => format!("({})", -rng.range(1, 5)),
            _ => rng.range(0, 12).to_string(),
        },
        Big => match rng.below(15) {
            0 => "9223372036854775807::int8".into(),
            1 => format!("({})::int8", -rng.range(1, 5)),
            _ => format!("{}::int8", rng.range(0, 12)),
        },
        Small => match rng.below(15) {
            0 => "32767::int2".into(),
            _ => format!("{}::int2", rng.range(-3, 12)),
        },
        Num => {
            let v = *rng.pick(&[
                "0", "1", "1.5", "-0.25", "2.50", "3", "10.00", "100", "0.001", "-7.125", "99.99",
                "1e3", "12345678.9",
            ]);
            if rng.chance(0.15) {
                "'NaN'::numeric".into()
            } else {
                format!("{v}::numeric")
            }
        }
        Flt => {
            let v = *rng.pick(&["0", "0.5", "-1.25", "2", "3.75", "-0", "100", "0.125"]);
            match rng.below(25) {
                0 => "'NaN'::float8".into(),
                1 => "'Infinity'::float8".into(),
                _ => format!("{v}::float8"),
            }
        }
        Text => {
            let t = *rng.pick(TEXTS);
            format!("'{t}'")
        }
        Bool => rng.pick(&["true", "false"]).to_string(),
        Date => {
            if rng.chance(0.05) {
                "'infinity'::date".into()
            } else {
                format!(
                    "'{:04}-{:02}-{:02}'::date",
                    rng.pick(&[1999, 2000, 2024, 2025]),
                    rng.range(1, 12),
                    rng.range(1, 28)
                )
            }
        }
        Ts => format!(
            "'{:04}-{:02}-{:02} {:02}:{:02}:00'::timestamp",
            rng.pick(&[1999, 2000, 2024, 2025]),
            rng.range(1, 12),
            rng.range(1, 28),
            rng.range(0, 23),
            rng.pick(&[0, 30, 59])
        ),
        Tstz => format!(
            "'{:04}-{:02}-{:02} {:02}:00:00{}'::timestamptz",
            rng.pick(&[2000, 2024]),
            rng.range(1, 12),
            rng.range(1, 28),
            rng.range(0, 23),
            rng.pick(&["+00", "-05", "+02:30"])
        ),
        Intv => format!(
            "'{}'::interval",
            rng.pick(&[
                "1 day", "2 hours", "-1 mon", "1 year 2 days", "00:30:00", "3 days 04:00:00",
                "0", "-2 days", "1 mon -1 day"
            ])
        ),
        IntArr => format!(
            "'{}'::int4[]",
            rng.pick(&["{1,2}", "{}", "{NULL,3}", "{3,1,2}", "{5}", "{1,1,2}", "{2,NULL}"])
        ),
        TextArr => format!(
            "'{}'::text[]",
            rng.pick(&["{a,b}", "{}", "{b,NULL}", "{A,a}", "{\"x y\",z}", "{ab}"])
        ),
        Jsonb | Json => format!(
            "'{}'::{}",
            rng.pick(&[
                r#"{"a": 1, "b": "x"}"#,
                "[1, 2, 3]",
                "1",
                "null",
                r#""s""#,
                r#"{"a": {"b": 2}}"#,
                r#"{"a": null}"#,
                r#"{"b": [1, "a"]}"#,
                "[]",
                "{}",
                r#"{"a": 2.50}"#
            ]),
            ty.sql()
        ),
    }
}

/// A stored value for a column, as a literal (NULL sometimes).
fn value(rng: &mut Rng, c: &Col) -> String {
    value_r(rng, c, 0.15)
}

/// A value of `c`, NULL with probability `nulls`.
fn value_r(rng: &mut Rng, c: &Col, nulls: f64) -> String {
    if c.name != "id" && rng.chance(nulls) {
        return "NULL".into();
    }
    match c.ty {
        Int => rng.range(-2, 9).to_string(),
        Big => rng.range(-2, 20).to_string(),
        Small => rng.range(0, 6).to_string(),
        // numeric(10,2): one scale for every stored value.
        Num => format!("{}.{:02}", rng.range(-3, 12), rng.pick(&[0, 0, 25, 50, 75, 5])),
        Flt => format!("{}", rng.range(-8, 16) as f64 / 4.0),
        Text => format!("'{}'", rng.pick(TEXTS)),
        Bool => rng.pick(&["true", "false"]).to_string(),
        Date => format!("'2024-{:02}-{:02}'", rng.range(1, 3), rng.range(1, 28)),
        Ts => format!(
            "'2024-01-{:02} {:02}:{:02}:00'",
            rng.range(1, 9),
            rng.pick(&[0, 6, 12, 23]),
            rng.pick(&[0, 30])
        ),
        Tstz => format!(
            "'2024-01-{:02} {:02}:00:00+00'",
            rng.range(1, 9),
            rng.pick(&[0, 6, 12, 23])
        ),
        Intv => format!(
            "'{}'",
            rng.pick(&["1 day", "2 hours", "1 mon", "24:00:00", "30 days", "0", "-1 day"])
        ),
        IntArr => format!(
            "'{}'",
            rng.pick(&["{1,2}", "{}", "{NULL,3}", "{3,1,2}", "{5}", "{1,1,2}", "{2}"])
        ),
        TextArr => format!("'{}'", rng.pick(&["{a,b}", "{}", "{b,NULL}", "{A,a}", "{ab}", "{z}"])),
        Jsonb | Json => format!(
            "'{}'",
            rng.pick(&[
                r#"{"a": 1, "b": "x"}"#,
                "[1, 2, 3]",
                "1",
                "null",
                r#""s""#,
                r#"{"a": {"b": 2}}"#,
                r#"{"a": 3}"#,
                r#"{"a": "1"}"#,
                "[]",
                r#"{"b": [1, "a"]}"#
            ])
        ),
    }
}

fn col_type_sql(c: &Col) -> String {
    if let Some(d) = c.dom {
        return d.to_string();
    }
    let base = match c.ty {
        Num => "numeric(10,2)".to_string(),
        t => t.sql().to_string(),
    };
    match c.coll {
        Some(k) => format!("{base} COLLATE \"{k}\""),
        None => base,
    }
}

impl<'a> Gen<'a> {
    pub fn new(seed: u64, schema: &'a Schema) -> Gen<'a> {
        Gen {
            rng: Rng::new(seed.wrapping_mul(31).wrapping_add(7)),
            schema,
            alias: 0,
            sub_depth: 0,
            ctes: Vec::new(),
            wide: seed >= WIDE_SEEDS,
        }
    }

    fn fresh(&mut self, p: &str) -> String {
        self.alias += 1;
        format!("{p}{}", self.alias)
    }

    // ------------------------------------------------------------------ expressions

    fn atoms_of<'c>(&mut self, cx: &'c Cx, ty: Ty) -> Vec<&'c SCol> {
        let mut v: Vec<&SCol> = cx.atoms.iter().filter(|c| c.ty == ty).collect();
        if (v.is_empty() || self.rng.chance(0.15)) && !cx.outer.is_empty() {
            v.extend(cx.outer.iter().filter(|c| c.ty == ty));
        }
        v
    }

    fn leaf(&mut self, cx: &Cx, ty: Ty) -> String {
        let atoms = self.atoms_of(cx, ty);
        if !atoms.is_empty() && self.rng.chance(0.8) {
            let a = atoms[self.rng.below(atoms.len())];
            return a.expr.clone();
        }
        if self.rng.chance(0.08) {
            return format!("NULL::{}", ty.sql());
        }
        literal(&mut self.rng, ty)
    }

    /// An expression of type `ty`.
    fn expr(&mut self, cx: &Cx, ty: Ty, depth: u32) -> String {
        if depth == 0 || self.rng.chance(0.3) {
            return self.leaf(cx, ty);
        }
        let d = depth - 1;
        // Constructs every type has.
        match self.rng.below(30) {
            0 => {
                return format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    self.expr(cx, Bool, d),
                    self.expr(cx, ty, d),
                    self.expr(cx, ty, d)
                );
            }
            1 => {
                return format!(
                    "COALESCE({}, {})",
                    self.expr(cx, ty, d),
                    self.expr(cx, ty, d)
                );
            }
            2 if ty.comparable() => {
                return format!("NULLIF({}, {})", self.expr(cx, ty, d), self.expr(cx, ty, d));
            }
            3 if self.sub_depth < 2 && ty.comparable() => return self.scalar_subquery(cx, ty),
            4 if cx.agg.is_some() => {
                if let Some(a) = self.aggregate(cx, ty, d) {
                    return a;
                }
            }
            5 if cx.window => {
                if let Some(w) = self.window_fn(cx, ty, d) {
                    return w;
                }
            }
            6 if ty.comparable() && ty != Bool && ty != Jsonb => {
                let f = *self.rng.pick(&["GREATEST", "LEAST"]);
                return format!("{f}({}, {})", self.expr(cx, ty, d), self.expr(cx, ty, d));
            }
            7 => {
                let fs: Vec<&Func> = self
                    .schema
                    .funcs
                    .iter()
                    .filter(|f| f.ret == ty)
                    .collect();
                if !fs.is_empty() {
                    let f = fs[self.rng.below(fs.len())].clone();
                    let args: Vec<String> = f.args.iter().map(|t| self.expr(cx, *t, d)).collect();
                    return format!("{}({})", f.name, args.join(", "));
                }
            }
            9 | 10 if self.wide => {
                if let Some(e) = self.wide_expr(cx, ty, d) {
                    return e;
                }
            }
            8 if ty != Bool && ty != Text && self.rng.chance(0.3) => {
                // A detour through text.
                return format!("({})::text::{}", self.expr(cx, ty, d), ty.sql());
            }
            _ => {}
        }
        match ty {
            Int => self.int_expr(cx, d),
            Big => match self.rng.below(5) {
                0 => format!("({} * {})", self.expr(cx, Big, d), self.expr(cx, Int, d)),
                1 => format!("({})::int8", self.expr(cx, Int, d)),
                2 => format!("({} + {})", self.expr(cx, Big, d), self.expr(cx, Big, d)),
                3 if cx.agg.is_some() => self
                    .aggregate(cx, Big, d)
                    .unwrap_or_else(|| self.leaf(cx, Big)),
                _ => format!("abs({})", self.expr(cx, Big, d)),
            },
            Small => match self.rng.below(3) {
                0 => format!("({})::int2", self.expr(cx, Int, d)),
                1 => format!("({} + {})", self.expr(cx, Small, d), self.expr(cx, Small, d)),
                _ => self.leaf(cx, Small),
            },
            Num => self.num_expr(cx, d),
            Flt => match self.rng.below(5) {
                0 => format!("({} + {})", self.expr(cx, Flt, d), self.expr(cx, Flt, d)),
                1 => format!("({} * {})", self.expr(cx, Flt, d), self.expr(cx, Flt, d)),
                2 => format!("({})::float8", self.expr(cx, Int, d)),
                3 => format!("abs({})", self.expr(cx, Flt, d)),
                _ => format!("(- {})", self.expr(cx, Flt, d)),
            },
            Text => self.text_expr(cx, d),
            Bool => self.bool_expr(cx, d),
            Date => match self.rng.below(5) {
                0 => format!("({} + {})", self.expr(cx, Date, d), self.expr(cx, Int, d)),
                1 => format!("({} - {})", self.expr(cx, Date, d), self.expr(cx, Int, d)),
                2 => format!("({})::date", self.expr(cx, Ts, d)),
                3 => format!(
                    "make_date(2024, {}, 1)",
                    self.rng.range(1, 12)
                ),
                _ => self.leaf(cx, Date),
            },
            Ts => match self.rng.below(5) {
                0 => format!("({} + {})", self.expr(cx, Ts, d), self.expr(cx, Intv, d)),
                1 => format!("({})::timestamp", self.expr(cx, Date, d)),
                2 => format!(
                    "date_trunc('{}', {})",
                    self.rng.pick(&["day", "month", "hour", "year", "week"]),
                    self.expr(cx, Ts, d)
                ),
                3 => format!("({} - {})", self.expr(cx, Ts, d), self.expr(cx, Intv, d)),
                _ => self.leaf(cx, Ts),
            },
            Tstz => match self.rng.below(4) {
                0 => format!("({} + {})", self.expr(cx, Tstz, d), self.expr(cx, Intv, d)),
                1 => format!("({} AT TIME ZONE 'UTC')", self.expr(cx, Ts, d)),
                2 => format!(
                    "date_trunc('{}', {})",
                    self.rng.pick(&["day", "month", "hour"]),
                    self.expr(cx, Tstz, d)
                ),
                _ => self.leaf(cx, Tstz),
            },
            Intv => match self.rng.below(5) {
                0 => format!("({} - {})", self.expr(cx, Ts, d), self.expr(cx, Ts, d)),
                1 => format!("({} * {})", self.expr(cx, Intv, d), self.expr(cx, Int, d)),
                2 => format!("justify_hours({})", self.expr(cx, Intv, d)),
                3 => format!("({} + {})", self.expr(cx, Intv, d), self.expr(cx, Intv, d)),
                _ => self.leaf(cx, Intv),
            },
            IntArr => match self.rng.below(6) {
                0 => format!(
                    "ARRAY[{}, {}]",
                    self.expr(cx, Int, d),
                    self.expr(cx, Int, d)
                ),
                1 => format!("({} || {})", self.expr(cx, IntArr, d), self.expr(cx, Int, d)),
                2 => format!("array_remove({}, {})", self.expr(cx, IntArr, d), self.expr(cx, Int, d)),
                3 if cx.agg.is_some() => self
                    .aggregate(cx, IntArr, d)
                    .unwrap_or_else(|| self.leaf(cx, IntArr)),
                4 => format!("({})[1:2]", self.expr(cx, IntArr, d)),
                _ => format!("({} || {})", self.expr(cx, IntArr, d), self.expr(cx, IntArr, d)),
            },
            TextArr => match self.rng.below(4) {
                0 => format!("ARRAY[{}, {}]", self.expr(cx, Text, d), self.expr(cx, Text, d)),
                1 => format!("string_to_array({}, ',')", self.expr(cx, Text, d)),
                2 if cx.agg.is_some() => self
                    .aggregate(cx, TextArr, d)
                    .unwrap_or_else(|| self.leaf(cx, TextArr)),
                _ => format!("array_append({}, {})", self.expr(cx, TextArr, d), self.expr(cx, Text, d)),
            },
            Jsonb => match self.rng.below(7) {
                0 => format!(
                    "jsonb_build_object('k', {}, 'v', {})",
                    self.expr(cx, Int, d),
                    self.expr(cx, Text, d)
                ),
                1 => format!("to_jsonb({})", {
                    let t = *self.rng.pick(&[Int, Text, Num, Bool, IntArr, Date]);
                    self.expr(cx, t, d)
                }),
                2 => format!("({} -> '{}')", self.expr(cx, Jsonb, d), self.rng.pick(&["a", "b"])),
                3 => format!("({} || {})", self.expr(cx, Jsonb, d), self.expr(cx, Jsonb, d)),
                4 if cx.agg.is_some() => self
                    .aggregate(cx, Jsonb, d)
                    .unwrap_or_else(|| self.leaf(cx, Jsonb)),
                5 => format!("jsonb_strip_nulls({})", self.expr(cx, Jsonb, d)),
                6 if self.rng.chance(0.5) => match self.rng.below(5) {
                    0 => format!("jsonb_path_query_first({}, '$.a')", self.expr(cx, Jsonb, d)),
                    1 => format!("jsonb_set({}, '{{a}}', {})", self.expr(cx, Jsonb, d), self.expr(cx, Jsonb, d)),
                    2 => format!("({} #> '{{a,b}}')", self.expr(cx, Jsonb, d)),
                    3 => format!("jsonb_insert('{{\"z\": 0}}', '{{y}}', {})", self.expr(cx, Jsonb, d)),
                    _ => format!("(jsonb_build_array({}, {}) -> 1)", self.expr(cx, Jsonb, d), self.expr(cx, Num, d)),
                },
                _ => format!("({} - 'a')", self.expr(cx, Jsonb, d)),
            },
            Json => match self.rng.below(3) {
                0 => format!("json_build_array({}, {})", self.expr(cx, Int, d), self.expr(cx, Text, d)),
                1 => format!("to_json({})", self.expr(cx, Text, d)),
                _ => format!("({} -> 'a')", self.expr(cx, Json, d)),
            },
        }
    }

    /// Round 18's wider grammar (agent 45): date/time and time zones, intervals, numeric and
    /// float edge values, regular expressions and patterns, SQL/JSON and jsonpath,
    /// multidimensional arrays and slices, ranges and multiranges. None when it has nothing
    /// of type `ty`.
    fn wide_expr(&mut self, cx: &Cx, ty: Ty, d: u32) -> Option<String> {
        Some(match ty {
            Int => match self.rng.below(14) {
                0 => format!("extract(dow from {})::int", self.expr(cx, Date, d)),
                1 => format!("regexp_count({}, '[a-c]')", self.expr(cx, Text, d)),
                2 => format!("array_ndims({})", self.md_array(cx, d)),
                3 => format!("array_upper({}, {})", self.md_array(cx, d), self.rng.range(1, 2)),
                4 => format!("lower({})", self.range_expr(cx, d, "int4range")),
                5 => format!("upper({})", self.range_expr(cx, d, "int4range")),
                6 => format!("scale({})", self.expr(cx, Num, d)),
                7 => format!("(JSON_VALUE({}, '$.a' RETURNING int DEFAULT -1 ON ERROR))", self.expr(cx, Jsonb, d)),
                8 => format!("div({}, {})", self.expr(cx, Num, d), self.rng.pick(&["3", "0.5", "-2"])),
                9 => format!("(regexp_instr({}, 'b'))", self.expr(cx, Text, d)),
                10 => format!("extract(isodow from {})::int", self.expr(cx, Tstz, d)),
                11 => format!("(jsonb_array_length(jsonb_path_query_array({}, '$.*')))", self.expr(cx, Jsonb, d)),
                12 => format!("cardinality({})", self.md_array(cx, d)),
                _ => format!("({})::int", self.expr(cx, Flt, d)),
            },
            Num => match self.rng.below(14) {
                0 => format!("({} ^ {})", self.expr(cx, Num, d), self.rng.pick(&["2", "0.5", "-1", "3"])),
                1 => format!("trim_scale({})", self.expr(cx, Num, d)),
                2 => format!("round({}, {})", self.expr(cx, Num, d), self.rng.range(-2, 4)),
                3 => format!("sqrt(abs({}))", self.expr(cx, Num, d)),
                4 => format!("({} * 1e20::numeric)", self.expr(cx, Num, d)),
                5 => (*self.rng.pick(&["'NaN'::numeric", "'Infinity'::numeric", "'-Infinity'::numeric", "0.000000000000000000001", "123456789012345678901234567890.5"])).to_string(),
                6 => format!("exp(least({}, 20))", self.expr(cx, Num, d)),
                7 => format!("ln(abs({}) + 1)", self.expr(cx, Num, d)),
                8 => format!("extract(epoch from {})", self.expr(cx, Intv, d)),
                9 => format!("({} / {})", self.expr(cx, Num, d), self.expr(cx, Num, d)),
                10 => format!("({})::numeric(20, 10)", self.expr(cx, Flt, d)),
                11 => format!("floor({})", self.expr(cx, Num, d)),
                12 => format!("mod({}, {})", self.expr(cx, Num, d), self.rng.pick(&["'Infinity'::numeric", "0.3", "-7"])),
                _ => format!("extract(epoch from {})", self.expr(cx, Ts, d)),
            },
            Flt => match self.rng.below(10) {
                0 => (*self.rng.pick(&["'Infinity'::float8", "'-Infinity'::float8", "'NaN'::float8", "1e300::float8", "1e-300::float8", "-0::float8", "0.1::float8"])).to_string(),
                1 => format!("power({}, {})", self.expr(cx, Flt, d), self.rng.pick(&["2", "0.5", "-1"])),
                2 => format!("cbrt({})", self.expr(cx, Flt, d)),
                3 => format!("({} / 3::float8)", self.expr(cx, Flt, d)),
                4 => format!("({})::float4::float8", self.expr(cx, Flt, d)),
                5 => format!("round({})", self.expr(cx, Flt, d)),
                6 => format!("({})::float8", self.expr(cx, Num, d)),
                7 => format!("degrees({})", self.expr(cx, Flt, d)),
                8 => format!("extract(epoch from {})::float8", self.expr(cx, Tstz, d)),
                _ => format!("({} * 1e200::float8)", self.expr(cx, Flt, d)),
            },
            Text => match self.rng.below(18) {
                0 => format!("to_char({}, '{}')", self.expr(cx, Ts, d), self.rng.pick(&["YYYY-MM-DD HH24:MI:SS", "Dy DD Mon", "IYYY-IW", "FMMonth", "J", "HH12 AM", "Q WW"])),
                1 => format!("to_char({}, '{}')", self.expr(cx, Num, d), self.rng.pick(&["999D99", "FM9.999EEEE", "S9990", "RN", "0000", "L999"])),
                2 => format!("({})::text", self.expr(cx, Flt, d)),
                3 => format!("substring({} from '{}')", self.expr(cx, Text, d), self.rng.pick(&["[a-z]+", "(b|c)", "^.", "x*$"])),
                4 => format!("(regexp_match({}, '{}'))[1]", self.expr(cx, Text, d), self.rng.pick(&["(a)(b)?", "([[:alpha:]]+)", "(\\d)"])),
                5 => format!("regexp_substr({}, '[^,]+', 1, {})", self.expr(cx, Text, d), self.rng.range(1, 2)),
                6 => format!("translate({}, 'abc', 'xy')", self.expr(cx, Text, d)),
                7 => format!("overlay({} placing 'zz' from {} for 1)", self.expr(cx, Text, d), self.rng.range(1, 3)),
                8 => format!("btrim({}, ' a')", self.expr(cx, Text, d)),
                9 => format!("({} AT TIME ZONE '{}')::text", self.expr(cx, Tstz, d), self.rng.pick(TZS)),
                10 => {
                    let k = *self.rng.pick(&["int4range", "numrange", "daterange"]);
                    format!("({})::text", self.range_expr(cx, d, k))
                }
                11 => format!("({})::text", self.md_array(cx, d)),
                12 => format!("JSON_QUERY({}, '{}')::text", self.expr(cx, Jsonb, d), self.rng.pick(&["$.a", "$[*]", "$.a ? (@ > 1)", "strict $.b"])),
                13 => format!("({})::text", self.multirange_expr(cx, d)),
                14 => format!("upper({} COLLATE \"{}\")", self.expr(cx, Text, d), self.rng.pick(&["C", "en-x-icu", "tr-x-icu", "und-x-icu"])),
                15 => format!("array_dims({})", self.md_array(cx, d)),
                16 => format!("justify_interval({})::text", self.expr(cx, Intv, d)),
                _ => format!("normalize({})", self.expr(cx, Text, d)),
            },
            Bool => match self.rng.below(16) {
                0 => format!("({} SIMILAR TO '{}')", self.expr(cx, Text, d), self.rng.pick(&["%(a|b)%", "_x%", "[a-c]*", "%!_%' ESCAPE '!"])),
                1 => format!("({} ILIKE '{}')", self.expr(cx, Text, d), self.rng.pick(&["A%", "%B_", "\\%%"])),
                2 => format!("({} LIKE '{}' ESCAPE '#')", self.expr(cx, Text, d), self.rng.pick(&["#%%", "a#_b", "%"])),
                3 => format!("({} ~* '{}')", self.expr(cx, Text, d), self.rng.pick(&["^A", "b{1,2}", "[[:upper:]]"])),
                4 => format!("({} @> {})", self.range_expr(cx, d, "int4range"), self.expr(cx, Int, d)),
                5 => format!("({} && {})", self.range_expr(cx, d, "int4range"), self.range_expr(cx, d, "int4range")),
                6 => {
                    let k = *self.rng.pick(&["int4range", "numrange", "daterange"]);
                    format!("isempty({})", self.range_expr(cx, d, k))
                }
                7 => format!("({} @? '{}')", self.expr(cx, Jsonb, d), self.rng.pick(&["$.a", "$[*] ? (@ > 1)", "$.** ? (@ == \"a\")"])),
                8 => format!("({} @@ '{}')", self.expr(cx, Jsonb, d), self.rng.pick(&["$.a > 1", "exists($.b)", "$.a starts with \"x\""])),
                9 => format!("JSON_EXISTS({}, '{}')", self.expr(cx, Jsonb, d), self.rng.pick(&["$.a", "strict $.a.b", "$ ? (@.k > 0)"])),
                10 => format!("({} @> {})", self.md_array(cx, d), self.expr(cx, IntArr, d)),
                11 => format!("({} -|- {})", self.range_expr(cx, d, "int4range"), self.range_expr(cx, d, "int4range")),
                12 => format!("isfinite({})", self.expr(cx, Ts, d)),
                13 => format!("({} IS JSON {})", self.expr(cx, Text, d), self.rng.pick(&["", "OBJECT", "ARRAY", "SCALAR", "WITH UNIQUE KEYS"])),
                14 => format!("({} @> {})", self.multirange_expr(cx, d), self.expr(cx, Int, d)),
                _ => format!("({} < {})", self.range_expr(cx, d, "numrange"), self.range_expr(cx, d, "numrange")),
            },
            Date => match self.rng.below(5) {
                0 => format!("date_bin('{}', {}, '2000-01-01')::date", self.rng.pick(&["7 days", "1 day", "15 days"]), self.expr(cx, Ts, d)),
                1 => format!("lower({})", self.range_expr(cx, d, "daterange")),
                2 => format!("({} + make_interval(days => {}))::date", self.expr(cx, Date, d), self.rng.range(-40, 40)),
                3 => (*self.rng.pick(&["'infinity'::date", "'-infinity'::date", "'2000-02-29'::date", "'0001-01-01 BC'::date"])).to_string(),
                _ => format!("make_date({}, {}, {})", self.rng.range(-5, 2030), self.rng.range(1, 12), self.rng.range(1, 31)),
            },
            Ts => match self.rng.below(6) {
                0 => format!("({} AT TIME ZONE '{}')", self.expr(cx, Tstz, d), self.rng.pick(TZS)),
                1 => format!("date_bin('{}', {}, '2001-01-01')", self.rng.pick(&["15 minutes", "1 hour", "2 days"]), self.expr(cx, Ts, d)),
                2 => format!("({} + {} * interval '1 month')", self.expr(cx, Ts, d), self.expr(cx, Int, d)),
                3 => (*self.rng.pick(&["'infinity'::timestamp", "'2024-02-29 23:59:59.999999'::timestamp", "'epoch'::timestamp", "'1999-12-31 24:00'::timestamp"])).to_string(),
                4 => format!("make_timestamp(2024, {}, {}, {}, 30, {})", self.rng.range(1, 12), self.rng.range(1, 28), self.rng.range(0, 23), self.rng.pick(&["0", "59.5", "1.25"])),
                _ => format!("date_trunc('{}', {})", self.rng.pick(&["quarter", "decade", "millennium", "isoyear", "minute"]), self.expr(cx, Ts, d)),
            },
            Tstz => match self.rng.below(5) {
                0 => format!("({} AT TIME ZONE '{}')", self.expr(cx, Ts, d), self.rng.pick(TZS)),
                1 => format!("'2024-03-10 02:30:00 {}'::timestamptz", self.rng.pick(&["America/New_York", "Europe/Berlin", "+05:30", "PST8PDT", "UTC"])),
                2 => format!("date_trunc('day', {}, '{}')", self.expr(cx, Tstz, d), self.rng.pick(TZS)),
                3 => format!("({} + interval '{}')", self.expr(cx, Tstz, d), self.rng.pick(&["1 day", "24 hours", "1 mon -1 day", "-1.5 years"])),
                _ => format!("make_timestamptz(2024, {}, {}, 1, 2, 3, '{}')", self.rng.range(1, 12), self.rng.range(1, 28), self.rng.pick(TZS)),
            },
            Intv => match self.rng.below(8) {
                0 => format!("({} * {})", self.expr(cx, Intv, d), self.rng.pick(&["1.5", "-0.333", "1e-3"])),
                1 => format!("({} / {})", self.expr(cx, Intv, d), self.rng.pick(&["3", "0.7", "-2"])),
                2 => format!("justify_days({})", self.expr(cx, Intv, d)),
                3 => format!("age({}, {})", self.expr(cx, Ts, d), self.expr(cx, Ts, d)),
                4 => format!("make_interval(years => {}, mins => {})", self.rng.range(-2, 2), self.rng.range(-90, 90)),
                5 => (*self.rng.pick(&["'P1Y2M3DT4H5M6S'::interval", "'-1 day +02:03'::interval", "'1.5 months'::interval", "'infinity'::interval", "'178000000 years'::interval"])).to_string(),
                6 => format!("({} - {})", self.expr(cx, Tstz, d), self.expr(cx, Tstz, d)),
                _ => format!("(- {})", self.expr(cx, Intv, d)),
            },
            IntArr => match self.rng.below(6) {
                0 => format!("({})[{}:{}]", self.md_array(cx, d), self.rng.range(0, 2), self.rng.range(1, 3)),
                1 => format!("array_cat({}, {})", self.expr(cx, IntArr, d), self.expr(cx, IntArr, d)),
                2 => format!("trim_array({}, 1)", self.expr(cx, IntArr, d)),
                3 => format!("array_fill({}, ARRAY[{}])", self.expr(cx, Int, d), self.rng.range(0, 3)),
                4 => format!("(ARRAY(SELECT generate_series(1, {})))", self.rng.range(0, 4)),
                _ => format!("('[0:1]={{{},{}}}'::int4[])", self.rng.range(-3, 3), self.rng.range(-3, 3)),
            },
            Jsonb => match self.rng.below(7) {
                0 => format!("JSON_QUERY({}, '{}' {})", self.expr(cx, Jsonb, d), self.rng.pick(&["$.a", "$[*]", "lax $.a[1 to 2]"]), self.rng.pick(&["", "WITH WRAPPER", "WITH CONDITIONAL WRAPPER", "EMPTY ARRAY ON EMPTY"])),
                1 => format!("jsonb_path_query_array({}, '{}')", self.expr(cx, Jsonb, d), self.rng.pick(&["$.*", "$.a.double()", "$.** ? (@.type() == \"number\")", "$.k.datetime()"])),
                2 => format!("JSON_OBJECT('a': {}, 'b': {} {} RETURNING jsonb)", self.expr(cx, Int, d), self.expr(cx, Text, d), self.rng.pick(&["", "ABSENT ON NULL", "NULL ON NULL"])),
                3 => format!("JSON_ARRAY({}, {} RETURNING jsonb)", self.expr(cx, Num, d), self.expr(cx, Bool, d)),
                4 => format!("jsonb_path_query_first({}, '$.a ? (@ like_regex \"^a\" flag \"i\")')", self.expr(cx, Jsonb, d)),
                5 => format!("to_jsonb({})", self.md_array(cx, d)),
                _ => format!("JSON({}::text)::jsonb", self.expr(cx, Jsonb, d)),
            },
            _ => return None,
        })
    }

    /// A one- or two-dimensional int4[] (literals, ARRAY[] of rows, slices).
    fn md_array(&mut self, cx: &Cx, d: u32) -> String {
        match self.rng.below(4) {
            0 => format!("ARRAY[[{}, {}], [{}, {}]]", self.expr(cx, Int, d), self.rng.range(0, 5), self.rng.range(0, 5), self.expr(cx, Int, d)),
            1 => (*self.rng.pick(&["'{{1,2},{3,4}}'::int4[]", "'{}'::int4[]", "'[2:3]={7,8}'::int4[]", "'{{NULL,1}}'::int4[]"])).to_string(),
            2 => format!("(ARRAY[[1,2,3],[4,5,6]])[{}:{}]", self.rng.range(1, 2), self.rng.range(1, 3)),
            _ => self.expr(cx, IntArr, d),
        }
    }

    fn range_expr(&mut self, cx: &Cx, d: u32, kind: &str) -> String {
        let (lo, hi) = match kind {
            "int4range" => (self.expr(cx, Int, d), self.expr(cx, Int, d)),
            "numrange" => (self.expr(cx, Num, d), self.expr(cx, Num, d)),
            _ => (self.expr(cx, Date, d), self.expr(cx, Date, d)),
        };
        match self.rng.below(5) {
            0 => format!("{kind}({lo}, {hi}, '{}')", self.rng.pick(&["[]", "(]", "()", "[)"])),
            1 => format!("({kind}(LEAST({lo}, {hi}), GREATEST({lo}, {hi})) * {kind}(NULL, {hi}))"),
            2 => format!("({kind}(LEAST({lo}, {hi}), GREATEST({lo}, {hi})) + {kind}({hi}, NULL))"),
            3 => format!("range_merge({kind}(LEAST({lo}, {hi}), GREATEST({lo}, {hi})), 'empty'::{kind})"),
            _ => format!("{kind}(LEAST({lo}, {hi}), GREATEST({lo}, {hi}))"),
        }
    }

    fn multirange_expr(&mut self, cx: &Cx, d: u32) -> String {
        match self.rng.below(3) {
            0 => format!("int4multirange({}, {})", self.range_expr(cx, d, "int4range"), self.range_expr(cx, d, "int4range")),
            1 => format!("({}::int4multirange - {}::int4multirange)", self.range_expr(cx, d, "int4range"), self.range_expr(cx, d, "int4range")),
            _ => (*self.rng.pick(&["'{[1,3), [5,9)}'::int4multirange", "'{}'::int4multirange", "'{(,4], [6,)}'::int4multirange"])).to_string(),
        }
    }

    fn int_expr(&mut self, cx: &Cx, d: u32) -> String {
        match self.rng.below(24) {
            0 => format!("({} + {})", self.expr(cx, Int, d), self.expr(cx, Int, d)),
            1 => format!("({} - {})", self.expr(cx, Int, d), self.expr(cx, Int, d)),
            2 => format!("({} * {})", self.expr(cx, Int, d), self.expr(cx, Int, d)),
            3 => {
                // Mostly a nonzero divisor: a division by zero is an error whose appearance
                // depends on evaluation order.
                let div = if self.rng.chance(0.85) {
                    self.rng.range(1, 4).to_string()
                } else {
                    self.expr(cx, Int, d)
                };
                format!("({} / {div})", self.expr(cx, Int, d))
            }
            4 => format!("({} % {})", self.expr(cx, Int, d), self.rng.range(1, 4)),
            5 => format!("abs({})", self.expr(cx, Int, d)),
            6 => format!("(- {})", self.expr(cx, Int, d)),
            7 => format!("length({})", self.expr(cx, Text, d)),
            8 => format!("position({} in {})", self.expr(cx, Text, d), self.expr(cx, Text, d)),
            9 => format!("cardinality({})", self.expr(cx, IntArr, d)),
            10 => format!("array_length({}, 1)", self.expr(cx, IntArr, d)),
            11 => format!("extract(day from {})::int", self.expr(cx, Date, d)),
            12 => format!("({} - {})", self.expr(cx, Date, d), self.expr(cx, Date, d)),
            13 => format!("({})::int4", self.expr(cx, Big, d)),
            14 => format!("({})::int4", self.expr(cx, Num, d)),
            15 => format!("ascii({})", self.expr(cx, Text, d)),
            16 => format!("sign({})::int", self.expr(cx, Int, d)),
            17 => format!(
                "CASE {} WHEN {} THEN {} WHEN {} THEN {} END",
                self.expr(cx, Int, d),
                self.rng.range(0, 5),
                self.expr(cx, Int, d),
                self.rng.range(0, 5),
                self.expr(cx, Int, d)
            ),
            18 => format!("({})[1]", self.expr(cx, IntArr, d)),
            19 => format!("jsonb_array_length({})", self.expr(cx, Jsonb, d)),
            20 => format!("({}::jsonb ->> 'a')::int", self.expr(cx, Jsonb, d)),
            21 => format!("width_bucket({}, 0, 10, 4)", self.expr(cx, Num, d)),
            22 => format!("({} & {})", self.expr(cx, Int, d), self.expr(cx, Int, d)),
            23 if cx.agg.is_some() => self.aggregate(cx, Int, d).unwrap_or_else(|| self.leaf(cx, Int)),
            23 => match self.rng.below(6) {
                0 => format!("strpos({}, 'a')", self.expr(cx, Text, d)),
                1 => format!("(date_part('month', {}))::int", self.expr(cx, Ts, d)),
                2 => format!("array_position({}, {})", self.expr(cx, IntArr, d), self.expr(cx, Int, d)),
                3 => format!("gcd({}, 6)", self.expr(cx, Int, d)),
                4 => format!("({} << 1)", self.expr(cx, Int, d)),
                _ => format!("num_nonnulls({}, {})", self.expr(cx, Int, d), self.expr(cx, Text, d)),
            },
            _ => format!("char_length({})", self.expr(cx, Text, d)),
        }
    }

    fn num_expr(&mut self, cx: &Cx, d: u32) -> String {
        match self.rng.below(12) {
            0 => format!("({} + {})", self.expr(cx, Num, d), self.expr(cx, Num, d)),
            1 => format!("({} - {})", self.expr(cx, Num, d), self.expr(cx, Int, d)),
            2 => format!("({} * {})", self.expr(cx, Num, d), self.expr(cx, Num, d)),
            3 => format!("({} / {})", self.expr(cx, Num, d), self.rng.pick(&["3", "2.5", "7"])),
            4 => format!("round({}, {})", self.expr(cx, Num, d), self.rng.range(0, 3)),
            5 => format!("trunc({})", self.expr(cx, Num, d)),
            6 => format!("({})::numeric", self.expr(cx, Int, d)),
            7 => format!("abs({})", self.expr(cx, Num, d)),
            8 => format!("({})::numeric(6,1)", self.expr(cx, Num, d)),
            9 => format!("ceil({})", self.expr(cx, Num, d)),
            10 if cx.agg.is_some() => self.aggregate(cx, Num, d).unwrap_or_else(|| self.leaf(cx, Num)),
            _ => format!("mod({}, 3)", self.expr(cx, Num, d)),
        }
    }

    fn text_expr(&mut self, cx: &Cx, d: u32) -> String {
        match self.rng.below(24) {
            0 => format!("({} || {})", self.expr(cx, Text, d), self.expr(cx, Text, d)),
            1 => format!("upper({})", self.expr(cx, Text, d)),
            2 => format!("lower({})", self.expr(cx, Text, d)),
            3 => format!("substr({}, {}, {})", self.expr(cx, Text, d), self.rng.range(0, 3), self.rng.range(0, 3)),
            4 => format!("left({}, {})", self.expr(cx, Text, d), self.rng.range(-1, 2)),
            5 => format!("replace({}, 'a', 'x')", self.expr(cx, Text, d)),
            6 => format!("trim({})", self.expr(cx, Text, d)),
            7 => format!("lpad({}, 3, '*')", self.expr(cx, Text, d)),
            8 => format!("concat({}, {})", self.expr(cx, Text, d), self.expr(cx, Int, d)),
            9 => format!("concat_ws('-', {}, {})", self.expr(cx, Text, d), self.expr(cx, Text, d)),
            10 => {
                let t = *self.rng.pick(&[Int, Num, Date, Bool, Ts, Intv, IntArr, Flt, Jsonb]);
                format!("({})::text", self.expr(cx, t, d))
            }
            11 => format!("({} ->> '{}')", self.expr(cx, Jsonb, d), self.rng.pick(&["a", "b"])),
            12 => format!("({} COLLATE \"{}\")", self.expr(cx, Text, d), self.rng.pick(COLLS)),
            13 => format!("initcap({})", self.expr(cx, Text, d)),
            14 => format!("reverse({})", self.expr(cx, Text, d)),
            15 => format!("repeat({}, 2)", self.expr(cx, Text, d)),
            16 => format!("md5({})", self.expr(cx, Text, d)),
            17 => format!("split_part({}, ',', 1)", self.expr(cx, Text, d)),
            18 => format!("format('%s|%s', {}, {})", self.expr(cx, Text, d), self.expr(cx, Int, d)),
            19 => format!("array_to_string({}, ',', '?')", self.expr(cx, TextArr, d)),
            20 => format!("regexp_replace({}, '[ab]', 'y', 'g')", self.expr(cx, Text, d)),
            21 if cx.agg.is_some() => self.aggregate(cx, Text, d).unwrap_or_else(|| self.leaf(cx, Text)),
            22 => format!("to_char({}, 'FM999.00')", self.expr(cx, Num, d)),
            _ => format!("quote_literal({})", self.expr(cx, Text, d)),
        }
    }

    fn cmp_type(&mut self) -> Ty {
        *self.rng.pick(CMP_TYPES)
    }

    fn bool_expr(&mut self, cx: &Cx, d: u32) -> String {
        match self.rng.below(30) {
            0..=5 => {
                let t = self.cmp_type();
                let op = *self.rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
                // Comparisons between a type's atoms mostly; sometimes across integer widths.
                let rt = if t == Int && self.rng.chance(0.2) {
                    *self.rng.pick(&[Big, Small, Num, Flt])
                } else {
                    t
                };
                format!("{} {op} {}", self.expr(cx, t, d), self.expr(cx, rt, d))
            }
            6 => format!("({} AND {})", self.expr(cx, Bool, d), self.expr(cx, Bool, d)),
            7 => format!("({} OR {})", self.expr(cx, Bool, d), self.expr(cx, Bool, d)),
            8 => format!("NOT ({})", self.expr(cx, Bool, d)),
            9 => {
                let t = *self.rng.pick(COLUMN_TYPES);
                let n = if self.rng.chance(0.5) { "NOT " } else { "" };
                format!("{} IS {n}NULL", self.expr(cx, t, d))
            }
            10 => {
                let t = self.cmp_type();
                let n = if self.rng.chance(0.5) { "NOT " } else { "" };
                format!("{} IS {n}DISTINCT FROM {}", self.expr(cx, t, d), self.expr(cx, t, d))
            }
            11 => format!(
                "({}) IS {}",
                self.expr(cx, Bool, d),
                self.rng.pick(&["TRUE", "FALSE", "UNKNOWN", "NOT TRUE", "NOT FALSE", "NOT UNKNOWN"])
            ),
            12 => {
                let t = *self.rng.pick(&[Int, Num, Text, Date, Big]);
                let s = if self.rng.chance(0.3) { "SYMMETRIC " } else { "" };
                let n = if self.rng.chance(0.2) { "NOT " } else { "" };
                format!(
                    "{} {n}BETWEEN {s}{} AND {}",
                    self.expr(cx, t, d),
                    self.expr(cx, t, d),
                    self.expr(cx, t, d)
                )
            }
            13 => {
                let t = *self.rng.pick(&[Int, Text, Num, Big, Date]);
                let n = self.rng.range(1, 4);
                let items: Vec<String> = (0..n).map(|_| self.expr(cx, t, d.min(1))).collect();
                let not = if self.rng.chance(0.3) { "NOT " } else { "" };
                format!("{} {not}IN ({})", self.expr(cx, t, d), items.join(", "))
            }
            14 | 15 if self.sub_depth < 2 => {
                let t = *self.rng.pick(&[Int, Int, Text, Num, Big]);
                let not = if self.rng.chance(0.3) { "NOT " } else { "" };
                let lhs = self.expr(cx, t, d);
                let q = self.subselect(cx, &[t]);
                format!("{lhs} {not}IN ({q})")
            }
            16 | 17 if self.sub_depth < 2 => {
                let not = if self.rng.chance(0.3) { "NOT " } else { "" };
                let q = self.subselect(cx, &[Int]);
                format!("{not}EXISTS ({q})")
            }
            18 if self.sub_depth < 2 => {
                let t = *self.rng.pick(&[Int, Text, Num]);
                let op = *self.rng.pick(&["=", "<>", "<", ">="]);
                let q = *self.rng.pick(&["ANY", "ALL", "SOME"]);
                let lhs = self.expr(cx, t, d);
                let sq = self.subselect(cx, &[t]);
                format!("{lhs} {op} {q} ({sq})")
            }
            19 => {
                let op = *self.rng.pick(&["=", "<>", "<", ">"]);
                let q = *self.rng.pick(&["ANY", "ALL"]);
                if self.rng.chance(0.5) {
                    format!("{} {op} {q} ({})", self.expr(cx, Int, d), self.expr(cx, IntArr, d))
                } else {
                    format!("{} {op} {q} ({})", self.expr(cx, Text, d), self.expr(cx, TextArr, d))
                }
            }
            20 => {
                let op = *self.rng.pick(&["LIKE", "NOT LIKE", "ILIKE", "~", "~*", "!~", "SIMILAR TO"]);
                let pat = *self.rng.pick(&["'a%'", "'%b'", "'_'", "'%'", "'A%'", "'a_b'", "'^a'", "'[0-9]'", "'%\\%%'"]);
                format!("{} {op} {pat}", self.expr(cx, Text, d))
            }
            21 => {
                let op = *self.rng.pick(&["@>", "<@", "&&"]);
                if self.rng.chance(0.6) {
                    format!("{} {op} {}", self.expr(cx, IntArr, d), self.expr(cx, IntArr, d))
                } else {
                    format!("{} {op} {}", self.expr(cx, TextArr, d), self.expr(cx, TextArr, d))
                }
            }
            22 => match self.rng.below(3) {
                0 => format!("{} @> {}", self.expr(cx, Jsonb, d), self.expr(cx, Jsonb, d)),
                1 => format!("{} ? '{}'", self.expr(cx, Jsonb, d), self.rng.pick(&["a", "b", "k"])),
                _ => format!("{} ?| ARRAY['a', 'x']", self.expr(cx, Jsonb, d)),
            },
            23 => {
                let op = *self.rng.pick(&["=", "<", "<=", "<>", ">"]);
                format!(
                    "({}, {}) {op} ({}, {})",
                    self.expr(cx, Int, d),
                    self.expr(cx, Text, d),
                    self.expr(cx, Int, d),
                    self.expr(cx, Text, d)
                )
            }
            24 => format!("starts_with({}, {})", self.expr(cx, Text, d), self.expr(cx, Text, d)),
            25 => format!("isfinite({})", self.expr(cx, Date, d)),
            26 if cx.agg.is_some() => self.aggregate(cx, Bool, d).unwrap_or_else(|| self.leaf(cx, Bool)),
            27 => {
                // Collation-sensitive comparisons.
                let c = *self.rng.pick(COLLS);
                format!(
                    "{} {} {} COLLATE \"{c}\"",
                    self.expr(cx, Text, d),
                    self.rng.pick(&["<", "=", ">="]),
                    self.expr(cx, Text, d)
                )
            }
            _ => self.leaf(cx, Bool),
        }
    }

    /// An aggregate returning `ty` over `cx.agg`'s columns.
    fn aggregate(&mut self, cx: &Cx, ty: Ty, d: u32) -> Option<String> {
        let inner = Cx {
            atoms: cx.agg.clone()?,
            outer: cx.outer.clone(),
            ..Cx::default()
        };
        let d = d.min(2);
        let filter = if self.rng.chance(0.2) {
            format!(" FILTER (WHERE {})", self.expr(&inner, Bool, 1))
        } else {
            String::new()
        };
        let distinct = if self.rng.chance(0.15) { "DISTINCT " } else { "" };
        let s = match ty {
            Int | Big => match self.rng.below(6) {
                0 => format!("count(*){filter}"),
                1 => {
                    let t = *self.rng.pick(COLUMN_TYPES);
                    let dd = if t.comparable() { distinct } else { "" };
                    format!("count({dd}{}){filter}", self.expr(&inner, t, d))
                }
                2 if ty == Big => format!("sum({distinct}{}){filter}", self.expr(&inner, Int, d)),
                3 => format!(
                    "{}({}){filter}",
                    self.rng.pick(&["min", "max"]),
                    self.expr(&inner, ty, d)
                ),
                4 if ty == Int => format!(
                    "{}({}){filter}",
                    self.rng.pick(&["bit_and", "bit_or"]),
                    self.expr(&inner, Int, d)
                ),
                _ if ty == Int => format!("(count(*){filter})::int"),
                _ => format!("count({distinct}{}){filter}", self.expr(&inner, Int, d)),
            },
            Num => match self.rng.below(6) {
                4 => format!(
                    "percentile_disc({}) WITHIN GROUP (ORDER BY {}){filter}",
                    self.rng.pick(&["0", "0.25", "0.5", "1"]),
                    self.expr(&inner, Num, d)
                ),
                5 => format!("mode() WITHIN GROUP (ORDER BY {}){filter}", self.expr(&inner, Num, d)),
                0 => format!("sum({distinct}{}){filter}", self.expr(&inner, Num, d)),
                1 => format!("avg({distinct}{}){filter}", {
                    let t = *self.rng.pick(&[Int, Num, Big]);
                    self.expr(&inner, t, d)
                }),
                2 => format!("sum({}){filter}", self.expr(&inner, Big, d)),
                _ => format!("{}({}){filter}", self.rng.pick(&["min", "max"]), self.expr(&inner, Num, d)),
            },
            Flt => {
                // Only over stored values, which sum exactly in any order.
                let cols: Vec<&SCol> = inner.atoms.iter().filter(|c| c.ty == Flt).collect();
                let c = if cols.is_empty() {
                    "0.5::float8".to_string()
                } else {
                    cols[self.rng.below(cols.len())].expr.clone()
                };
                format!("{}({c}){filter}", self.rng.pick(&["sum", "avg", "min", "max"]))
            }
            Text => match self.rng.below(3) {
                0 => {
                    let e = self.expr(&inner, Text, d);
                    let dir = self.rng.pick(&["", " DESC"]);
                    format!("string_agg({distinct}{e}, ',' ORDER BY {e}{dir}){filter}")
                }
                _ => format!("{}({}){filter}", self.rng.pick(&["min", "max"]), self.expr(&inner, Text, d)),
            },
            Bool => format!(
                "{}({}){filter}",
                self.rng.pick(&["bool_and", "bool_or", "every"]),
                self.expr(&inner, Bool, d)
            ),
            IntArr => {
                let e = self.expr(&inner, Int, d);
                format!("array_agg({distinct}{e} ORDER BY {e} NULLS FIRST){filter}")
            }
            TextArr => {
                let e = self.expr(&inner, Text, d);
                format!("array_agg({e} ORDER BY {e}){filter}")
            }
            Jsonb => {
                let e = self.expr(&inner, Int, d);
                format!("jsonb_agg({e} ORDER BY {e}){filter}")
            }
            Date | Ts | Tstz | Intv | Small => format!(
                "{}({}){filter}",
                self.rng.pick(&["min", "max"]),
                self.expr(&inner, ty, d)
            ),
            Json => return None,
        };
        Some(s)
    }

    /// A window function returning `ty`. Order-sensitive ones get a total ORDER BY.
    fn window_fn(&mut self, cx: &Cx, ty: Ty, d: u32) -> Option<String> {
        let base = Cx {
            atoms: cx.atoms.clone(),
            outer: cx.outer.clone(),
            agg: cx.agg.clone(),
            ..Cx::default()
        };
        let part = if self.rng.chance(0.5) {
            let t = *self.rng.pick(&[Int, Text, Bool, Date]);
            format!("PARTITION BY {} ", self.leaf(&base, t))
        } else {
            String::new()
        };
        let ord_t = *self.rng.pick(&[Int, Text, Num, Date]);
        let ord = self.leaf(&base, ord_t);
        let total = if cx.total.is_empty() {
            None
        } else {
            Some(cx.total.join(", "))
        };
        let s = match (ty, self.rng.below(8)) {
            (Big, 0) | (Big, 1) => {
                let f = *self.rng.pick(&["rank()", "dense_rank()", "count(*)"]);
                format!("{f} OVER ({part}ORDER BY {ord})")
            }
            (Big, 2) => {
                let total = total?;
                format!("row_number() OVER ({part}ORDER BY {total})")
            }
            (Big, _) => {
                let frame = *self.rng.pick(&[
                    "",
                    " RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
                    " GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
                    " RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING",
                    " RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP",
                    " GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING EXCLUDE TIES",
                    " GROUPS BETWEEN 2 PRECEDING AND CURRENT ROW EXCLUDE GROUP",
                ]);
                let arg = if base.agg.is_some() { "count(*)".to_string() } else { self.expr(&base, Int, d.min(1)) };
                format!("sum({arg}) OVER ({part}ORDER BY {ord}{frame})")
            }
            (Int, 0) => {
                let total = total?;
                let off = self.rng.range(1, 2);
                format!(
                    "lag({}, {off}) OVER ({part}ORDER BY {total})",
                    self.leaf(&base, Int)
                )
            }
            (Int, 1) => {
                let total = total?;
                format!(
                    "{}({}) OVER ({part}ORDER BY {total} ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)",
                    self.rng.pick(&["first_value", "last_value", "max", "min"]),
                    self.leaf(&base, Int)
                )
            }
            (Int, 2) => format!(
                "{}({}) OVER ({part})",
                self.rng.pick(&["max", "min"]),
                self.leaf(&base, Int)
            ),
            (Int, 3) => {
                let total = total?;
                format!("ntile(3) OVER ({part}ORDER BY {total})")
            }
            (Int, 4) if matches!(ord_t, Int) => format!(
                "max({}) OVER ({part}ORDER BY {ord} RANGE BETWEEN 2 PRECEDING AND 1 FOLLOWING)",
                self.leaf(&base, Int)
            ),
            (Flt, _) => format!(
                "{}() OVER ({part}ORDER BY {ord})",
                self.rng.pick(&["percent_rank", "cume_dist"])
            ),
            (Text, 0) => {
                let total = total?;
                format!(
                    "lead({}, 1, 'none') OVER ({part}ORDER BY {total})",
                    self.leaf(&base, Text)
                )
            }
            (Num, 0) => format!(
                "avg({}) OVER ({part}ORDER BY {ord})",
                self.leaf(&base, Num)
            ),
            _ => return None,
        };
        Some(s)
    }

    fn scalar_subquery(&mut self, cx: &Cx, ty: Ty) -> String {
        self.sub_depth += 1;
        let outer = self.outer_of(cx);
        let (from, scope, _) = self.from_clause(&outer, 1);
        let inner = Cx {
            atoms: scope.clone(),
            outer: outer.clone(),
            ..Cx::default()
        };
        let wh = if self.rng.chance(0.7) {
            format!(" WHERE {}", self.expr(&inner, Bool, 2))
        } else {
            String::new()
        };
        let s = if self.rng.chance(0.5) || ty == Bool {
            let agg_cx = Cx {
                atoms: Vec::new(),
                outer: outer.clone(),
                agg: Some(scope.clone()),
                ..Cx::default()
            };
            let a = self
                .aggregate(&agg_cx, ty, 1)
                .unwrap_or_else(|| literal(&mut self.rng, ty));
            format!("(SELECT {a} FROM {from}{wh})")
        } else {
            let e = self.expr(&inner, ty, 1);
            format!("(SELECT {e} FROM {from}{wh} ORDER BY 1 LIMIT 1)")
        };
        self.sub_depth -= 1;
        s
    }

    /// The outer scope a sub-select sees: this level's columns and its own outer ones.
    fn outer_of(&self, cx: &Cx) -> Vec<SCol> {
        let mut o = cx.atoms.clone();
        if let Some(a) = &cx.agg {
            o.extend(a.iter().cloned());
        }
        o.extend(cx.outer.iter().cloned());
        o
    }

    /// A sub-select returning columns of `types` (for IN / EXISTS / ANY), maybe correlated.
    fn subselect(&mut self, cx: &Cx, types: &[Ty]) -> String {
        self.sub_depth += 1;
        let outer = self.outer_of(cx);
        let q = self.select(&outer, Some(types), false);
        self.sub_depth -= 1;
        q.sql
    }

    // ------------------------------------------------------------------ FROM

    fn table_ref(&mut self) -> (String, Vec<SCol>, Vec<String>) {
        let mut names: Vec<(String, Vec<(String, Ty, Option<&'static str>)>, Option<String>)> = self
            .schema
            .tables
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    t.cols.iter().map(|c| (c.name.clone(), c.ty, c.coll)).collect(),
                    t.key.clone(),
                )
            })
            .collect();
        for (n, cols) in &self.ctes {
            names.push((n.clone(), cols.iter().map(|(c, t)| (c.clone(), *t, None)).collect(), None));
        }
        let (name, cols, key) = names.swap_remove(self.rng.below(names.len()));
        let a = self.fresh("t");
        let only = if self.rng.chance(0.05) && !self.ctes.iter().any(|c| c.0 == name) { "ONLY " } else { "" };
        let scope: Vec<SCol> = cols
            .iter()
            .map(|(c, t, k)| SCol { expr: format!("{a}.{c}"), ty: *t, coll: *k })
            .collect();
        let keys = match key {
            Some(k) => vec![format!("{a}.{k}")],
            None => scope.iter().filter(|c| c.ty.comparable()).map(|c| c.expr.clone()).collect(),
        };
        (format!("{only}{name} {a}"), scope, keys)
    }

    /// One FROM item: a table, a sub-select, VALUES, a function. Returns the SQL, the
    /// columns and expressions that order its rows totally.
    fn from_item(&mut self, outer: &[SCol], lateral_scope: &[SCol]) -> (String, Vec<SCol>, Vec<String>) {
        let r = self.rng.below(20);
        if r < 12 || self.sub_depth >= 2 {
            return self.table_ref();
        }
        let a = self.fresh("s");
        match r {
            12..=14 => {
                // A sub-select; LATERAL when there is something to its left.
                let lateral = !lateral_scope.is_empty() && self.rng.chance(0.5);
                let mut o = outer.to_vec();
                if lateral {
                    o.extend(lateral_scope.iter().cloned());
                }
                self.sub_depth += 1;
                let q = self.select(&o, None, true);
                self.sub_depth -= 1;
                let names: Vec<String> = q.cols.iter().map(|c| c.0.clone()).collect();
                let scope: Vec<SCol> = q
                    .cols
                    .iter()
                    .map(|(n, t)| SCol { expr: format!("{a}.{n}"), ty: *t, coll: None })
                    .collect();
                let total = scope.iter().filter(|c| c.ty.comparable()).map(|c| c.expr.clone()).collect();
                let lat = if lateral { "LATERAL " } else { "" };
                let _ = names;
                (format!("{lat}({}) {a}", q.sql), scope, total)
            }
            15 | 16 => {
                let n = self.rng.range(1, 4);
                let tys: Vec<Ty> = (0..self.rng.range(1, 3)).map(|_| *self.rng.pick(&[Int, Text, Num, Bool, Date])).collect();
                let cx = Cx { outer: lateral_scope.to_vec(), ..Cx::default() };
                let rows: Vec<String> = (0..n)
                    .map(|_| {
                        let vs: Vec<String> = tys
                            .iter()
                            .map(|t| {
                                if self.rng.chance(0.15) {
                                    "NULL".into()
                                } else if !lateral_scope.is_empty() && self.rng.chance(0.3) {
                                    self.expr(&cx, *t, 1)
                                } else {
                                    literal(&mut self.rng, *t)
                                }
                            })
                            .collect();
                        format!("({})", vs.join(", "))
                    })
                    .collect();
                let names: Vec<String> = (0..tys.len()).map(|i| format!("v{i}")).collect();
                let scope: Vec<SCol> = names
                    .iter()
                    .zip(&tys)
                    .map(|(n, t)| SCol { expr: format!("{a}.{n}"), ty: *t, coll: None })
                    .collect();
                let total = scope.iter().map(|c| c.expr.clone()).collect();
                let lat = if !lateral_scope.is_empty() { "LATERAL " } else { "" };
                let _ = lat;
                (format!("(VALUES {}) {a}({})", rows.join(", "), names.join(", ")), scope, total)
            }
            17 => {
                let cx = Cx { atoms: lateral_scope.iter().filter(|c| c.ty == Int).cloned().collect(), ..Cx::default() };
                let hi = if !cx.atoms.is_empty() && self.rng.chance(0.5) {
                    format!("{} % 5", self.leaf(&cx, Int))
                } else {
                    self.rng.range(0, 6).to_string()
                };
                let scope = vec![SCol { expr: format!("{a}.g"), ty: Int, coll: None }];
                (format!("generate_series(1, {hi}) {a}(g)"), scope.clone(), vec![scope[0].expr.clone()])
            }
            18 => {
                let cx = Cx { atoms: lateral_scope.iter().filter(|c| c.ty == IntArr).cloned().collect(), ..Cx::default() };
                let arr = self.leaf(&cx, IntArr);
                let scope = vec![
                    SCol { expr: format!("{a}.u"), ty: Int, coll: None },
                    SCol { expr: format!("{a}.o"), ty: Big, coll: None },
                ];
                (
                    format!("unnest({arr}) WITH ORDINALITY {a}(u, o)"),
                    scope.clone(),
                    scope.iter().map(|c| c.expr.clone()).collect(),
                )
            }
            _ => {
                if self.schema.srfs.is_empty() {
                    return self.table_ref();
                }
                let (name, args, cols) = self.schema.srfs[self.rng.below(self.schema.srfs.len())].clone();
                let cx = Cx { atoms: lateral_scope.to_vec(), ..Cx::default() };
                let args: Vec<String> = args.iter().map(|t| self.expr(&cx, *t, 1)).collect();
                let scope: Vec<SCol> = cols
                    .iter()
                    .map(|(n, t)| SCol { expr: format!("{a}.{n}"), ty: *t, coll: None })
                    .collect();
                let total = scope.iter().filter(|c| c.ty.comparable()).map(|c| c.expr.clone()).collect();
                (format!("{name}({}) {a}", args.join(", ")), scope, total)
            }
        }
    }

    /// A join condition between two scopes: an equality of like-typed columns, plus extras.
    fn join_cond(&mut self, l: &[SCol], r: &[SCol], outer: &[SCol]) -> String {
        let mut pairs = Vec::new();
        for a in l {
            for b in r {
                if a.ty == b.ty && a.ty.comparable() {
                    pairs.push((a.expr.clone(), b.expr.clone()));
                }
            }
        }
        let mut both = l.to_vec();
        both.extend(r.iter().cloned());
        let cx = Cx { atoms: both, outer: outer.to_vec(), ..Cx::default() };
        let mut conds = Vec::new();
        if !pairs.is_empty() && self.rng.chance(0.85) {
            let (a, b) = pairs[self.rng.below(pairs.len())].clone();
            conds.push(format!("{a} = {b}"));
            if self.rng.chance(0.2) {
                let (a, b) = pairs[self.rng.below(pairs.len())].clone();
                conds.push(format!("{a} {} {b}", self.rng.pick(&["=", "<", "<>", ">="])));
            }
        }
        if conds.is_empty() || self.rng.chance(0.25) {
            conds.push(self.expr(&cx, Bool, 2));
        }
        conds.join(" AND ")
    }

    /// A FROM clause of 1..=n items joined.
    fn from_clause(&mut self, outer: &[SCol], max_items: usize) -> (String, Vec<SCol>, Vec<String>) {
        let (mut sql, mut scope, mut total) = self.from_item(outer, &[]);
        let n = 1 + self.rng.below(max_items.max(1));
        for _ in 1..n {
            let (rsql, rscope, rtotal) = self.from_item(outer, &scope);
            let kind = self.rng.below(12);
            let lateral = rsql.starts_with("LATERAL") || rsql.contains("generate_series") || rsql.contains("unnest(");
            let (j, nullable_l, nullable_r) = match kind {
                0..=3 => ("JOIN", false, false),
                4..=6 => ("LEFT JOIN", false, true),
                7 => ("RIGHT JOIN", true, false),
                8 if !lateral => ("FULL JOIN", true, true),
                9 => (",", false, false),
                10 => ("CROSS JOIN", false, false),
                _ => ("JOIN", false, false),
            };
            let _ = (nullable_l, nullable_r);
            if j == "," || j == "CROSS JOIN" {
                sql = format!("{sql} {j} {rsql}");
                if j == "," && self.rng.chance(0.3) {
                    // A comma join's condition goes in WHERE; the caller adds a WHERE anyway.
                }
            } else if j == "FULL JOIN" && rsql.starts_with("LATERAL") {
                sql = format!("{sql} JOIN {rsql} ON true");
            } else {
                let cond = self.join_cond(&scope, &rscope, outer);
                let using = self.using_cols(&scope, &rscope);
                if let Some(u) = using.filter(|_| self.rng.chance(0.25)) {
                    sql = format!("{sql} {j} {rsql} USING ({u})");
                } else {
                    sql = format!("({sql} {j} {rsql} ON {cond})");
                }
            }
            scope.extend(rscope);
            total.extend(rtotal);
        }
        (sql, scope, total)
    }

    /// A column name both sides have with the same type, for USING.
    fn using_cols(&mut self, l: &[SCol], r: &[SCol]) -> Option<String> {
        let name = |e: &str| e.rsplit('.').next().unwrap().to_string();
        let lq: Vec<&str> = l.iter().map(|c| c.expr.split('.').next().unwrap()).collect();
        // USING needs unqualified names unique on the left: only one left relation.
        if lq.iter().any(|q| *q != lq[0]) {
            return None;
        }
        let mut cands = Vec::new();
        for a in l {
            for b in r {
                if name(&a.expr) == name(&b.expr) && a.ty == b.ty && a.ty.comparable() && a.coll == b.coll {
                    cands.push(name(&a.expr));
                }
            }
        }
        if cands.is_empty() { None } else { Some(cands[self.rng.below(cands.len())].clone()) }
    }

    // ------------------------------------------------------------------ SELECT

    /// A SELECT. `want`: the output types (sub-selects); `named`: output columns get names
    /// (FROM sub-selects, CTEs). At the top level the ORDER BY may fix the row order.
    pub fn select(&mut self, outer: &[SCol], want: Option<&[Ty]>, named: bool) -> Query {
        let top = self.sub_depth == 0;
        // Set operations.
        if self.rng.chance(if top { 0.12 } else { 0.06 }) {
            let types: Vec<Ty> = match want {
                Some(w) => w.to_vec(),
                None => (0..self.rng.range(1, 3)).map(|_| *self.rng.pick(&[Int, Text, Num, Date, Bool])).collect(),
            };
            let op = *self.rng.pick(&["UNION", "UNION ALL", "INTERSECT", "EXCEPT", "INTERSECT ALL", "EXCEPT ALL", "UNION"]);
            self.sub_depth += 1;
            let a = self.select_core(outer, Some(&types), true, false);
            let b = self.select_core(outer, Some(&types), true, false);
            self.sub_depth -= 1;
            let cols = a.cols.clone();
            let mut sql = format!("({}) {op} ({})", a.sql, b.sql);
            let mut ordered = false;
            if top && self.rng.chance(0.6) {
                let ord: Vec<String> = (1..=cols.len()).map(|i| format!("{i}{}", self.dir())).collect();
                sql = format!("{sql} ORDER BY {}", ord.join(", "));
                ordered = true;
                if self.rng.chance(0.3) {
                    sql = format!("{sql} LIMIT {}", self.rng.range(0, 5));
                }
            }
            return Query { sql, ordered, cols };
        }
        // CTEs.
        let saved = self.ctes.len();
        let mut with = String::new();
        if self.rng.chance(if top { 0.15 } else { 0.04 }) {
            with = self.with_clause(outer);
        }
        let mut q = self.select_core(outer, want, named || top, top);
        self.ctes.truncate(saved);
        if !with.is_empty() {
            q.sql = format!("{with}{}", q.sql);
        }
        q
    }

    fn dir(&mut self) -> &'static str {
        self.rng.pick(&["", " DESC", " NULLS FIRST", " DESC NULLS LAST", ""])
    }

    fn with_clause(&mut self, outer: &[SCol]) -> String {
        let mut parts = Vec::new();
        let recursive = self.rng.chance(0.3);
        if recursive {
            let name = self.fresh("r");
            let lim = self.rng.range(2, 8);
            let union = self.rng.pick(&["UNION ALL", "UNION"]);
            let body = match self.rng.below(3) {
                0 => format!(
                    "SELECT 1, 'x'::text UNION ALL SELECT n + 1, x || 'y' FROM {name} WHERE n < {lim}"
                ),
                1 => {
                    // Over a table: walk from small values up.
                    let t = self.schema.tables.iter().filter(|t| t.cols.iter().any(|c| c.ty == Int)).collect::<Vec<_>>();
                    if t.is_empty() {
                        format!("SELECT 1, 'a'::text {union} SELECT n + 2, x FROM {name} WHERE n < {lim}")
                    } else {
                        let t = t[self.rng.below(t.len())];
                        let c = t.cols.iter().find(|c| c.ty == Int).unwrap().name.clone();
                        format!(
                            "SELECT {c}, 'a'::text FROM {} WHERE {c} IS NOT NULL {union} SELECT n + 1, x FROM {name} WHERE n < {lim}",
                            t.name
                        )
                    }
                }
                _ => format!(
                    "VALUES (0, ''::text) {union} SELECT (n * 2 + 1) % 7, x || n::text FROM {name} WHERE length(x) < {lim}"
                ),
            };
            parts.push(format!("{name}(n, x) AS ({body})"));
            self.ctes.push((name, vec![("n".into(), Int), ("x".into(), Text)]));
        }
        let n = self.rng.range(if recursive { 0 } else { 1 }, 2);
        for _ in 0..n {
            let name = self.fresh("w");
            let mat = *self.rng.pick(&["", "", "MATERIALIZED ", "NOT MATERIALIZED "]);
            self.sub_depth += 1;
            let q = self.select(outer, None, true);
            self.sub_depth -= 1;
            parts.push(format!("{name} AS {mat}({})", q.sql));
            self.ctes.push((name, q.cols));
        }
        let r = if recursive { "RECURSIVE " } else { "" };
        format!("WITH {r}{} ", parts.join(", "))
    }

    fn select_core(&mut self, outer: &[SCol], want: Option<&[Ty]>, named: bool, top: bool) -> Query {
        let max_items = if self.sub_depth == 0 { 3 } else { 2 };
        let (from, scope, total) = self.from_clause(outer, max_items);
        let base = Cx { atoms: scope.clone(), outer: outer.to_vec(), ..Cx::default() };
        let mut sql = String::from("SELECT ");
        let where_ = if self.rng.chance(0.65) {
            Some(self.expr(&base, Bool, 3))
        } else {
            None
        };
        let mode = self.rng.below(10);
        let types: Vec<Ty> = match want {
            Some(w) => w.to_vec(),
            None => (0..self.rng.range(1, 4)).map(|_| *self.rng.pick(COLUMN_TYPES)).collect(),
        };
        let mut items = Vec::new();
        let mut group = String::new();
        let mut having = String::new();
        let mut distinct = String::new();
        let mut orderable = true;
        let mut distinct_on: Option<Vec<String>> = None;
        if mode < 3 {
            // Aggregation: group keys are atoms of the select list.
            let ngroup = self.rng.below(3);
            let mut keys: Vec<SCol> = Vec::new();
            for _ in 0..ngroup {
                let c = scope[self.rng.below(scope.len())].clone();
                if !c.ty.comparable() || keys.iter().any(|k| k.expr == c.expr) {
                    continue;
                }
                keys.push(c);
            }
            if self.rng.chance(0.15) && !keys.is_empty() {
                // An expression key.
                let k = Cx { atoms: scope.clone(), ..Cx::default() };
                let e = format!("({})", self.expr(&k, Int, 1));
                keys.push(SCol { expr: e, ty: Int, coll: None });
            }
            let cx = Cx {
                atoms: keys.clone(),
                outer: outer.to_vec(),
                agg: Some(scope.clone()),
                window: top && self.rng.chance(0.2),
                total: keys.iter().map(|k| k.expr.clone()).collect(),
            };
            for t in &types {
                let e = if self.rng.chance(0.6) {
                    self.aggregate(&cx, *t, 2).unwrap_or_else(|| self.expr(&cx, *t, 2))
                } else {
                    self.expr(&cx, *t, 2)
                };
                items.push(e);
            }
            if !keys.is_empty() {
                let ks: Vec<String> = keys.iter().map(|k| k.expr.clone()).collect();
                group = match self.rng.below(8) {
                    0 if ks.len() >= 2 => format!(" GROUP BY ROLLUP ({})", ks.join(", ")),
                    1 if ks.len() >= 2 => format!(" GROUP BY CUBE ({})", ks.join(", ")),
                    2 => format!(" GROUP BY GROUPING SETS (({}), ())", ks.join(", ")),
                    3 if ks.len() >= 2 => format!(" GROUP BY GROUPING SETS (({}), ({}))", ks[0], ks[1]),
                    _ => format!(" GROUP BY {}", ks.join(", ")),
                };
                if (group.contains("ROLLUP") || group.contains("CUBE") || group.contains("SETS"))
                    && want.is_none()
                    && self.rng.chance(0.4)
                {
                    items.push(format!("GROUPING({})", ks.join(", ")));
                }
            }
            if self.rng.chance(0.25) {
                having = format!(" HAVING {}", self.expr(&cx, Bool, 2));
            }
        } else {
            let cx = Cx {
                atoms: scope.clone(),
                outer: outer.to_vec(),
                agg: None,
                window: top && mode == 9,
                total: total.clone(),
            };
            for t in &types {
                items.push(self.expr(&cx, *t, 3));
            }
            if self.rng.chance(0.12) {
                if types.iter().all(|t| t.comparable()) {
                    distinct = "DISTINCT ".into();
                } else {
                    orderable = false;
                }
            } else if top && self.rng.chance(0.08) && types.iter().all(|t| t.comparable()) {
                distinct_on = Some(vec!["1".to_string()]);
            }
        }
        if want.is_some() && want.unwrap().iter().all(|t| *t == Int) && !named && self.rng.chance(0.2) && distinct.is_empty() && group.is_empty() {
            // EXISTS-style sub-selects: SELECT 1.
            items = (0..types.len()).map(|_| self.rng.range(1, 3).to_string()).collect();
        }
        let cols: Vec<(String, Ty)> = types
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("c{i}"), *t))
            .collect();
        let mut list = Vec::new();
        for (i, e) in items.iter().enumerate() {
            if i < cols.len() && (named || self.rng.chance(0.3)) {
                list.push(format!("{e} AS {}", cols[i].0));
            } else {
                list.push(e.clone());
            }
        }
        if let Some(on) = &distinct_on {
            let _ = write!(sql, "DISTINCT ON ({}) ", on.join(", "));
        }
        let _ = write!(sql, "{distinct}{} FROM {from}", list.join(", "));
        if let Some(w) = where_ {
            let _ = write!(sql, " WHERE {w}");
        }
        sql.push_str(&group);
        sql.push_str(&having);
        let mut ordered = false;
        let all_cmp = types.iter().all(|t| t.comparable()) && items.len() == types.len();
        if (top || self.rng.chance(0.2)) && all_cmp && orderable && (distinct_on.is_some() || self.rng.chance(if top { 0.55 } else { 0.3 })) {
            let ord: Vec<String> = (1..=items.len()).map(|i| format!("{i}{}", self.dir())).collect();
            let _ = write!(sql, " ORDER BY {}", ord.join(", "));
            ordered = top;
            if self.rng.chance(0.3) {
                let _ = write!(sql, " LIMIT {}", self.rng.range(0, 6));
            }
            if self.rng.chance(0.15) {
                let _ = write!(sql, " OFFSET {}", self.rng.range(0, 3));
            }
        } else if distinct_on.is_some() {
            // DISTINCT ON needs its ORDER BY; drop it.
            sql = sql.replacen("DISTINCT ON (1) ", "", 1);
        }
        if top && self.rng.chance(0.03) && group.is_empty() && distinct.is_empty() && !sql.contains("UNION") {
            // Locking: FOR UPDATE / SHARE (errors in places PostgreSQL refuses it).
            let _ = write!(sql, " FOR {}", self.rng.pick(&["UPDATE", "SHARE", "NO KEY UPDATE", "KEY SHARE"]));
        }
        Query { sql, ordered, cols }
    }

    // ------------------------------------------------------------------ DML

    pub fn dml(&mut self) -> Option<Stmt> {
        let ts: Vec<&Table> = self.schema.tables.iter().filter(|t| t.writable).collect();
        if ts.is_empty() {
            return None;
        }
        let mut t = ts[self.rng.below(ts.len())].clone();
        // (Generated columns aren't written.)
        t.cols.retain(|c| !c.generated);
        let a = "x";
        let scope: Vec<SCol> = t
            .cols
            .iter()
            .map(|c| SCol { expr: format!("{a}.{}", c.name), ty: c.ty, coll: c.coll })
            .collect();
        let cx = Cx { atoms: scope.clone(), ..Cx::default() };
        let ret = if self.rng.chance(0.6) {
            let items: Vec<String> = (0..self.rng.range(1, 2))
                .map(|_| {
                    let ty = *self.rng.pick(&[Int, Text, Num, Bool]);
                    self.expr(&cx, ty, 2)
                })
                .collect();
            let all = if self.rng.chance(0.4) { format!("{a}.*") } else { items.join(", ") };
            format!(" RETURNING {all}")
        } else {
            String::new()
        };
        let noncols: Vec<&Col> = t.cols.iter().filter(|c| c.name != "id").collect();
        let sql = match self.rng.below(8) {
            0 | 1 => {
                // INSERT ... VALUES / SELECT.
                let names: Vec<String> = t.cols.iter().map(|c| c.name.clone()).collect();
                let src = if self.rng.chance(0.5) {
                    let rows: Vec<String> = (0..self.rng.range(1, 3))
                        .map(|_| {
                            let vs: Vec<String> = t
                                .cols
                                .iter()
                                .map(|c| {
                                    if c.name == "id" {
                                        self.rng.range(1, 60).to_string()
                                    } else if self.rng.chance(0.2) {
                                        "DEFAULT".into()
                                    } else {
                                        value(&mut self.rng, c)
                                    }
                                })
                                .collect();
                            format!("({})", vs.join(", "))
                        })
                        .collect();
                    format!("VALUES {}", rows.join(", "))
                } else {
                    self.sub_depth += 1;
                    let types: Vec<Ty> = t.cols.iter().map(|c| c.ty).collect();
                    let q = self.select(&[], Some(&types), false);
                    self.sub_depth -= 1;
                    q.sql
                };
                let conflict = if t.key.is_some() && self.rng.chance(0.6) {
                    if self.rng.chance(0.4) || noncols.is_empty() {
                        " ON CONFLICT DO NOTHING".to_string()
                    } else {
                        let c = noncols[self.rng.below(noncols.len())].clone();
                        let ecx = Cx {
                            atoms: scope
                                .iter()
                                .cloned()
                                .chain(t.cols.iter().map(|c| SCol { expr: format!("EXCLUDED.{}", c.name), ty: c.ty, coll: c.coll }))
                                .collect(),
                            ..Cx::default()
                        };
                        let v = self.expr(&ecx, c.ty, 2);
                        let w = if self.rng.chance(0.3) { format!(" WHERE {}", self.expr(&ecx, Bool, 1)) } else { String::new() };
                        format!(" ON CONFLICT (id) DO UPDATE SET {} = {v}{w}", c.name)
                    }
                } else {
                    String::new()
                };
                format!("INSERT INTO {} AS {a} ({}) {src}{conflict}{ret}", t.name, names.join(", "))
            }
            2 | 3 => {
                if noncols.is_empty() {
                    return None;
                }
                let n = self.rng.range(1, 2) as usize;
                let mut sets = Vec::new();
                for _ in 0..n {
                    let c = noncols[self.rng.below(noncols.len())].clone();
                    if sets.iter().any(|s: &String| s.starts_with(&format!("{} =", c.name))) {
                        continue;
                    }
                    let v = if self.rng.chance(0.1) { "DEFAULT".to_string() } else { self.expr(&cx, c.ty, 2) };
                    sets.push(format!("{} = {v}", c.name));
                }
                let w = if self.rng.chance(0.8) { format!(" WHERE {}", self.expr(&cx, Bool, 3)) } else { String::new() };
                format!("UPDATE {} {a} SET {}{w}{ret}", t.name, sets.join(", "))
            }
            4 => match self.rng.below(4) {
                // DELETE ... USING another relation.
                0 => {
                    let other = self.schema.tables[self.rng.below(self.schema.tables.len())].clone();
                    let oscope: Vec<SCol> = other
                        .cols
                        .iter()
                        .map(|c| SCol { expr: format!("u.{}", c.name), ty: c.ty, coll: c.coll })
                        .collect();
                    let cond = self.join_cond(&scope, &oscope, &[]);
                    format!("DELETE FROM {} {a} USING {} u WHERE {cond}{ret}", t.name, other.name)
                }
                // A data-modifying CTE read by the query after it.
                1 => {
                    let w = self.expr(&cx, Bool, 2);
                    let r = if ret.is_empty() { " RETURNING x.*".to_string() } else { ret.clone() };
                    format!(
                        "WITH d AS (DELETE FROM {} {a} WHERE {w}{r}) SELECT count(*) FROM d",
                        t.name
                    )
                }
                _ => {
                    let w = format!(" WHERE {}", self.expr(&cx, Bool, 3));
                    format!("DELETE FROM {} {a}{w}{ret}", t.name)
                }
            },
            5 if t.key.is_some() => {
                // UPDATE ... FROM a sub-select keyed by id: one source row per target row.
                let c = (*noncols.get(self.rng.below(noncols.len().max(1)))?).clone();
                let src = format!(
                    "(SELECT DISTINCT ON (id) id, {} AS v FROM {} ORDER BY id, 2) s",
                    c.name, self.schema.tables[self.rng.below(self.schema.tables.len())].name
                );
                // The other table may lack the column; that's an error both sides report.
                let mut scx = cx.clone();
                scx.atoms.push(SCol { expr: "s.v".into(), ty: c.ty, coll: c.coll });
                let v = self.expr(&scx, c.ty, 2);
                format!("UPDATE {} {a} SET {} = {v} FROM {src} WHERE s.id = {a}.id{ret}", t.name, c.name)
            }
            _ if t.key.is_some() => {
                // MERGE.
                let other = &self.schema.tables[self.rng.below(self.schema.tables.len())];
                let key_ok = other.key.is_some();
                if !key_ok || noncols.is_empty() {
                    return None;
                }
                let shared: Vec<&Col> = noncols
                    .iter()
                    .filter(|c| other.cols.iter().any(|o| o.name == c.name && o.ty == c.ty))
                    .copied()
                    .collect();
                let c = if shared.is_empty() { None } else { Some(shared[self.rng.below(shared.len())].clone()) };
                let mut scx = cx.clone();
                scx.atoms.push(SCol { expr: "s.id".into(), ty: Int, coll: None });
                if let Some(c) = &c {
                    scx.atoms.push(SCol { expr: format!("s.{}", c.name), ty: c.ty, coll: c.coll });
                }
                let tgt = noncols[self.rng.below(noncols.len())].clone();
                let mut whens = Vec::new();
                let cond = if self.rng.chance(0.4) { format!(" AND {}", self.expr(&scx, Bool, 1)) } else { String::new() };
                whens.push(format!(
                    "WHEN MATCHED{cond} THEN UPDATE SET {} = {}",
                    tgt.name,
                    self.expr(&scx, tgt.ty, 2)
                ));
                if self.rng.chance(0.4) {
                    whens.push(format!("WHEN MATCHED AND {} THEN DELETE", self.expr(&scx, Bool, 1)));
                }
                if self.rng.chance(0.7) {
                    let cols: Vec<String> = t.cols.iter().map(|c| c.name.clone()).collect();
                    let vs: Vec<String> = t
                        .cols
                        .iter()
                        .map(|c| if c.name == "id" { "s.id + 100".to_string() } else if self.rng.chance(0.5) { "DEFAULT".into() } else { value(&mut self.rng, c) })
                        .collect();
                    whens.push(format!("WHEN NOT MATCHED THEN INSERT ({}) VALUES ({})", cols.join(", "), vs.join(", ")));
                }
                if self.rng.chance(0.25) {
                    whens.push("WHEN NOT MATCHED BY SOURCE THEN DELETE".to_string());
                }
                let mret = if self.rng.chance(0.4) { format!(" RETURNING merge_action(), {a}.*") } else { String::new() };
                let sel = match &c {
                    Some(c) => format!("id, {}", c.name),
                    None => "id".into(),
                };
                format!(
                    "MERGE INTO {} {a} USING (SELECT {sel} FROM {} WHERE id % {} = 0) s ON s.id = {a}.id {}{mret}",
                    t.name,
                    other.name,
                    self.rng.range(1, 3),
                    whens.join(" ")
                )
            }
            _ => return None,
        };
        // RETURNING rows come in an order the statement's plan decides: compare them sorted.
        Some(Stmt::Dml { sql, table: t.name.clone(), ordered: false })
    }

    /// A random planner-settings combination (a list of `SET` commands).
    pub fn plan_settings(&mut self) -> Vec<String> {
        const GUCS: &[&str] = &[
            "enable_hashjoin",
            "enable_mergejoin",
            "enable_nestloop",
            "enable_seqscan",
            "enable_indexscan",
            "enable_indexonlyscan",
            "enable_bitmapscan",
            "enable_sort",
            "enable_hashagg",
            "enable_memoize",
            "enable_material",
            "enable_incremental_sort",
            "enable_presorted_aggregate",
            "enable_partitionwise_join",
            "enable_partitionwise_aggregate",
            "enable_partition_pruning",
            "enable_group_by_reordering",
            "enable_self_join_elimination",
            "enable_distinct_reordering",
        ];
        let mut out = Vec::new();
        for _ in 0..self.rng.range(1, 4) {
            let g = *self.rng.pick(GUCS);
            let on = g.starts_with("enable_partitionwise");
            out.push(format!("SET {g} = {}", if on { "on" } else { "off" }));
        }
        if self.rng.chance(0.15) {
            out.push("SET join_collapse_limit = 1".into());
        }
        if self.rng.chance(0.1) {
            out.push("SET work_mem = '64kB'".into());
        }
        out.sort();
        out.dedup();
        out
    }

    /// A query for the TLP / NoREC oracles: a FROM clause and a predicate over it.
    pub fn oracle_parts(&mut self) -> (String, Vec<String>, String) {
        let (from, scope, _) = self.from_clause(&[], 2);
        let cx = Cx { atoms: scope.clone(), ..Cx::default() };
        let pred = self.expr(&cx, Bool, 3);
        let cols: Vec<String> = scope
            .iter()
            .filter(|c| c.ty.comparable())
            .map(|c| c.expr.clone())
            .take(4)
            .collect();
        (from, cols, pred)
    }
}

// ---------------------------------------------------------------------- schema

/// A random schema and its data, as the statements that make it.
pub fn schema(seed: u64) -> (Schema, Vec<String>) {
    let mut rng = Rng::new(seed);
    let mut s = Schema::default();
    let mut out = Vec::new();
    let names = ["a", "b", "c", "d", "e", "f"];
    let r3 = seed >= R3_SEEDS;
    if r3 {
        out.push("CREATE COLLATION ci3 (provider = icu, locale = 'und-u-ks-level2', deterministic = false)".into());
        out.push("CREATE DOMAIN d3int AS int CHECK (VALUE IS NULL OR VALUE > -3)".into());
        out.push("CREATE DOMAIN d3text AS text COLLATE \"en-x-icu\" CHECK (VALUE <> 'zz')".into());
    }
    let ntables = rng.range(2, 4);
    for ti in 0..ntables {
        let name = format!("t{ti}");
        let mut cols = Vec::new();
        let keyed = rng.chance(0.75);
        if keyed {
            cols.push(Col { name: "id".into(), ty: Int, coll: None, generated: false, dom: None });
        }
        let ncols = rng.range(2, 5) as usize;
        let mut gen_def = None;
        for c in names.iter().take(ncols) {
            let ty = *rng.pick(COLUMN_TYPES);
            let coll = if ty == Text && rng.chance(0.3) { Some(*rng.pick(if r3 { COLLS3 } else { &COLLS[..4] })) } else { None };
            let mut col = Col { name: c.to_string(), ty, coll, generated: false, dom: None };
            if r3 && ty == Int && rng.chance(0.25) {
                col.dom = Some("d3int");
            } else if r3 && ty == Text && coll.is_none() && rng.chance(0.2) {
                (col.dom, col.coll) = (Some("d3text"), Some("en-x-icu"));
            }
            cols.push(col);
        }
        // Round 3: a generated column over an integer one.
        if r3 && rng.chance(0.4)
            && let Some(src) = cols.iter().find(|c| c.ty == Int).map(|c| c.name.clone())
        {
            let kind = if rng.chance(0.5) { "STORED" } else { "VIRTUAL" };
            gen_def = Some(format!("g int GENERATED ALWAYS AS (coalesce({src}, 0) * 2 + 1) {kind}"));
            cols.push(Col { name: "g".into(), ty: Int, coll: None, generated: true, dom: None });
        }
        let nulls = if r3 { *rng.pick(&[0.15, 0.4, 0.7]) } else { 0.15 };
        let kind = rng.below(10);
        let defs: Vec<String> = cols
            .iter()
            .map(|c| {
                if c.generated {
                    return gen_def.clone().unwrap();
                }
                let mut d = format!("{} {}", c.name, col_type_sql(c));
                if c.name == "id" && kind < 7 {
                    d.push_str(" PRIMARY KEY");
                }
                if c.name != "id" && rng.chance(0.1) {
                    let _ = write!(d, " DEFAULT {}", value(&mut rng, c).replace("NULL", "0"));
                    if c.ty == Bool || c.ty.numeric_like() || c.ty == Text {
                    } else {
                        d = format!("{} {}", c.name, col_type_sql(c));
                    }
                }
                d
            })
            .collect();
        let part_col = cols.iter().find(|c| c.name != "id" && matches!(c.ty, Int | Text | Date));
        let mut writable = true;
        let mut key = if keyed && kind < 7 { Some("id".to_string()) } else { None };
        if kind >= 8
            && let Some(pc) = part_col.cloned()
        {
            // Partitioned: the key must include the partition column.
            let strategy = if pc.ty == Int && rng.chance(0.3) { "HASH" } else if rng.chance(0.5) { "LIST" } else { "RANGE" };
            let defs: Vec<String> = defs.iter().map(|d| d.replace(" PRIMARY KEY", "")).collect();
            out.push(format!("CREATE TABLE {name} ({}) PARTITION BY {strategy} ({})", defs.join(", "), pc.name));
            key = None;
            match (strategy, pc.ty) {
                ("HASH", _) => {
                    for i in 0..3 {
                        out.push(format!("CREATE TABLE {name}_p{i} PARTITION OF {name} FOR VALUES WITH (MODULUS 3, REMAINDER {i})"));
                    }
                }
                ("LIST", Int) => {
                    out.push(format!("CREATE TABLE {name}_p0 PARTITION OF {name} FOR VALUES IN (0, 1, 2)"));
                    out.push(format!("CREATE TABLE {name}_p1 PARTITION OF {name} FOR VALUES IN (3, 4, NULL)"));
                    out.push(format!("CREATE TABLE {name}_pd PARTITION OF {name} DEFAULT"));
                }
                ("LIST", Text) => {
                    out.push(format!("CREATE TABLE {name}_p0 PARTITION OF {name} FOR VALUES IN ('a', 'b', 'A')"));
                    out.push(format!("CREATE TABLE {name}_pd PARTITION OF {name} DEFAULT"));
                }
                ("LIST", _) => {
                    out.push(format!("CREATE TABLE {name}_pd PARTITION OF {name} DEFAULT"));
                }
                (_, Int) => {
                    out.push(format!("CREATE TABLE {name}_p0 PARTITION OF {name} FOR VALUES FROM (MINVALUE) TO (3)"));
                    out.push(format!("CREATE TABLE {name}_p1 PARTITION OF {name} FOR VALUES FROM (3) TO (6)"));
                    if rng.chance(0.5) {
                        out.push(format!("CREATE TABLE {name}_pd PARTITION OF {name} DEFAULT"));
                    } else {
                        out.push(format!("CREATE TABLE {name}_p2 PARTITION OF {name} FOR VALUES FROM (6) TO (MAXVALUE)"));
                    }
                }
                (_, Text) => {
                    out.push(format!("CREATE TABLE {name}_p0 PARTITION OF {name} FOR VALUES FROM (MINVALUE) TO ('b')"));
                    out.push(format!("CREATE TABLE {name}_pd PARTITION OF {name} DEFAULT"));
                }
                _ => {
                    out.push(format!("CREATE TABLE {name}_p0 PARTITION OF {name} FOR VALUES FROM (MINVALUE) TO ('2024-02-01')"));
                    out.push(format!("CREATE TABLE {name}_pd PARTITION OF {name} DEFAULT"));
                }
            }
        } else {
            out.push(format!("CREATE TABLE {name} ({})", defs.join(", ")));
            if kind == 7 {
                // Inheritance: a child with an extra column.
                out.push(format!("CREATE TABLE {name}_c (extra int) INHERITS ({name})"));
                let child_cols: Vec<String> = cols.iter().filter(|c| !c.generated).map(|c| c.name.clone()).collect();
                for _ in 0..rng.range(0, 4) {
                    let vs: Vec<String> = cols.iter().filter(|c| !c.generated).map(|c| if c.name == "id" { (100 + rng.range(0, 1000)).to_string() } else { value_r(&mut rng, c, nulls) }).collect();
                    out.push(format!("INSERT INTO {name}_c ({}, extra) VALUES ({}, {})", child_cols.join(", "), vs.join(", "), rng.range(0, 3)));
                }
                writable = true;
            }
        }
        // Data.
        let nrows = *rng.pick(&[0, 1, 3, 5, 8, 12, 12, 20, 30]);
        let mut used = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for _ in 0..nrows {
            let vs: Vec<String> = cols
                .iter()
                .filter(|c| !c.generated)
                .map(|c| {
                    if c.name == "id" {
                        let mut v = rng.range(1, 40);
                        while !used.insert(v) {
                            v = rng.range(1, 60);
                        }
                        v.to_string()
                    } else {
                        value_r(&mut rng, c, nulls)
                    }
                })
                .collect();
            rows.push(format!("({})", vs.join(", ")));
        }
        if !rows.is_empty() {
            let names: Vec<String> = cols.iter().filter(|c| !c.generated).map(|c| c.name.clone()).collect();
            out.push(format!("INSERT INTO {name} ({}) VALUES {}", names.join(", "), rows.join(", ")));
        }
        // Indexes.
        for _ in 0..rng.range(0, 3) {
            let c = &cols[rng.below(cols.len())];
            if !c.ty.comparable() {
                continue;
            }
            let idx = format!("{name}_i{}", rng.next() % 1000);
            let stmt = match rng.below(7) {
                0 if c.ty == IntArr || c.ty == Jsonb || c.ty == TextArr => format!("CREATE INDEX {idx} ON {name} USING gin ({})", c.name),
                1 => format!("CREATE INDEX {idx} ON {name} USING hash ({})", c.name),
                2 => {
                    let c2 = &cols[rng.below(cols.len())];
                    if c2.name == c.name || !c2.ty.comparable() {
                        format!("CREATE INDEX {idx} ON {name} ({} DESC NULLS LAST)", c.name)
                    } else {
                        format!("CREATE INDEX {idx} ON {name} ({}, {})", c.name, c2.name)
                    }
                }
                3 => format!("CREATE INDEX {idx} ON {name} ({}) WHERE {} IS NOT NULL", c.name, c.name),
                4 if c.ty == Int => format!("CREATE INDEX {idx} ON {name} (({} + 1))", c.name),
                5 if c.ty == Text => format!("CREATE INDEX {idx} ON {name} (lower({}))", c.name),
                6 if key.is_none() && c.ty == Int => format!("CREATE UNIQUE INDEX {idx} ON {name} ({}) NULLS NOT DISTINCT", c.name),
                _ => format!("CREATE INDEX {idx} ON {name} ({})", c.name),
            };
            out.push(stmt);
        }
        // Round 3: triggers on one keyed plain table, writing `audit3`.
        let partitioned = out.iter().any(|o| o.starts_with(&format!("CREATE TABLE {name} (")) && o.contains(" PARTITION BY "));
        let audited = r3
            && key.is_some()
            && !partitioned
            && !s.tables.iter().any(|t| t.audited)
            && rng.chance(0.5);
        if audited {
            out.extend(triggers(&mut rng, &name, &cols));
        }
        s.tables.push(Table { name: name.clone(), cols: cols.clone(), key, is_view: false, writable, audited });
    }
    if r3 && rng.chance(0.6) {
        let coll = *rng.pick(&["en-x-icu", "C", "und-x-icu", "ci3", "default"]);
        let range = rng.chance(0.5);
        out.extend(composite(&mut rng, coll, range));
        s.composite = Some((coll, range));
    }
    if rng.chance(0.7) {
        out.push("ANALYZE".into());
    }
    // Functions.
    let fi = rng.range(0, 2);
    for i in 0..fi {
        let name = format!("f{i}");
        let mut g = Gen::new(seed + 1000 + i as u64, &s);
        let cx = Cx {
            atoms: vec![
                SCol { expr: "p".into(), ty: Int, coll: None },
                SCol { expr: "q".into(), ty: Text, coll: None },
            ],
            ..Cx::default()
        };
        let cond = g.expr(&cx, Bool, 2);
        let e1 = g.expr(&cx, Int, 2);
        let e2 = g.expr(&cx, Int, 2);
        let vol = *rng.pick(&["IMMUTABLE", "STABLE", "VOLATILE", ""]);
        let strict = if rng.chance(0.4) { " STRICT" } else { "" };
        let body = match rng.below(3) {
            0 => format!(
                "CREATE FUNCTION {name}(p int, q text) RETURNS int LANGUAGE plpgsql {vol}{strict} AS $$\nDECLARE r int := 0;\nBEGIN\n  IF {cond} THEN\n    r := {e1};\n  ELSE\n    FOR i IN 1..least(coalesce(p, 0), 4) LOOP\n      r := r + i;\n    END LOOP;\n    r := r + coalesce({e2}, 0);\n  END IF;\n  RETURN r;\nEXCEPTION WHEN division_by_zero OR numeric_value_out_of_range THEN\n  RETURN -1;\nEND\n$$"
            ),
            1 => format!(
                "CREATE FUNCTION {name}(p int, q text) RETURNS int LANGUAGE sql {vol}{strict} AS $$ SELECT CASE WHEN {cond} THEN {e1} ELSE {e2} END $$"
            ),
            _ => format!(
                "CREATE FUNCTION {name}(p int, q text) RETURNS int LANGUAGE plpgsql {vol}{strict} AS $$\nBEGIN\n  CASE WHEN p IS NULL THEN RETURN NULL;\n  WHEN {cond} THEN RETURN {e1};\n  ELSE RETURN {e2};\n  END CASE;\nEND\n$$"
            ),
        };
        out.push(body);
        s.funcs.push(Func { name, args: vec![Int, Text], ret: Int });
    }
    // Set-returning functions over a table.
    if rng.chance(0.5) {
        let t = s.tables[rng.below(s.tables.len())].clone();
        if let Some(c) = t.cols.iter().find(|c| c.ty == Int) {
            let other = t.cols.iter().find(|c| c.ty == Text).map(|c| c.name.clone());
            let (rt, sel, cols) = match &other {
                Some(o) => ("TABLE(x int, y text)".to_string(), format!("{}, {o}", c.name), vec![("x".to_string(), Int), ("y".to_string(), Text)]),
                None => ("TABLE(x int)".to_string(), c.name.clone(), vec![("x".to_string(), Int)]),
            };
            if rng.chance(0.5) {
                out.push(format!(
                    "CREATE FUNCTION srf0(lim int) RETURNS {rt} LANGUAGE sql STABLE AS $$ SELECT {sel} FROM {} WHERE {} <= lim $$",
                    t.name, c.name
                ));
            } else {
                out.push(format!(
                    "CREATE FUNCTION srf0(lim int) RETURNS {rt} LANGUAGE plpgsql AS $$\nBEGIN\n  RETURN QUERY SELECT {sel} FROM {} WHERE {} <= lim;\n  IF lim > 3 THEN\n    RETURN QUERY SELECT {sel} FROM {} WHERE {} IS NULL;\n  END IF;\nEND\n$$",
                    t.name, c.name, t.name, c.name
                ));
            }
            s.srfs.push(("srf0".into(), vec![Int], cols));
        }
    }
    // Views.
    for vi in 0..rng.range(0, 2) {
        let mut g = Gen::new(seed + 2000 + vi as u64, &s);
        g.sub_depth = 1;
        let q = g.select(&[], None, true);
        let name = format!("v{vi}");
        out.push(format!("CREATE VIEW {name} AS {}", q.sql));
        s.tables.push(Table {
            name,
            cols: q.cols.iter().map(|(n, t)| Col { name: n.clone(), ty: *t, coll: None, generated: false, dom: None }).collect(),
            key: None,
            is_view: true,
            writable: false,
            audited: false,
        });
    }
    // (A generator of its own: the seeds before keep their schemas.)
    if seed >= R4_SEEDS {
        out.extend(round4::schema(&mut Rng::new(seed ^ 0x4444_4444)));
        s.round4 = true;
    }
    (s, out)
}


/// Round 3's triggers on table `t`: a BEFORE row trigger that skips some rows and fills an
/// integer column's NULLs, an AFTER row trigger and an AFTER statement trigger (over its
/// transition table) logging into `audit3`.
fn triggers(rng: &mut Rng, t: &str, cols: &[Col]) -> Vec<String> {
    let fill = cols
        .iter()
        .find(|c| c.ty == Int && c.name != "id" && !c.generated)
        .map(|c| format!("NEW.{0} := coalesce(NEW.{0}, -1); ", c.name))
        .unwrap_or_default();
    let skip = rng.range(0, 6);
    let mut out = vec![
        "CREATE TABLE audit3 (op text, k int, v text)".to_string(),
        format!(
            "CREATE FUNCTION trg3b() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = {skip} THEN RETURN NULL; END IF; {fill}RETURN NEW; END $$"
        ),
        format!("CREATE TRIGGER t3b BEFORE INSERT OR UPDATE ON {t} FOR EACH ROW EXECUTE FUNCTION trg3b()"),
        "CREATE FUNCTION trg3a() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO audit3 VALUES (TG_OP, OLD.id, OLD::text); RETURN OLD; END IF; INSERT INTO audit3 VALUES (TG_OP, NEW.id, NEW::text); RETURN NEW; END $$".to_string(),
    ];
    let when = if rng.chance(0.5) { " WHEN (OLD.* IS DISTINCT FROM NEW.*)" } else { "" };
    out.push(format!("CREATE TRIGGER t3a AFTER INSERT OR DELETE ON {t} FOR EACH ROW EXECUTE FUNCTION trg3a()"));
    out.push(format!("CREATE TRIGGER t3u AFTER UPDATE ON {t} FOR EACH ROW{when} EXECUTE FUNCTION trg3a()"));
    out.push("CREATE FUNCTION trg3s() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit3 SELECT 'S' || TG_OP, count(*), string_agg(id::text, ',' ORDER BY id) FROM nt; RETURN NULL; END $$".to_string());
    let ev = *rng.pick(&["INSERT", "UPDATE", "DELETE"]);
    let rel = if ev == "DELETE" { "OLD" } else { "NEW" };
    out.push(format!("CREATE TRIGGER t3s AFTER {ev} ON {t} REFERENCING {rel} TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION trg3s()"));
    out
}

/// A `ct3` literal: a text and an integer, either NULL now and then.
fn ct3_lit(rng: &mut Rng) -> String {
    let a = if rng.chance(0.15) { "NULL".to_string() } else { format!("'{}'", rng.pick(TEXTS)) };
    let b = if rng.chance(0.15) { "NULL".to_string() } else { rng.range(-1, 3).to_string() };
    format!("ROW({a}, {b})::ct3")
}

/// Round 3's composite type `ct3 (a text COLLATE coll, b int)`, the table `tc3` of them
/// (NULL-heavy: NULL values and NULL fields), indexes, and a range type over it.
fn composite(rng: &mut Rng, coll: &str, range: bool) -> Vec<String> {
    let collate = if coll == "default" { String::new() } else { format!(" COLLATE \"{coll}\"") };
    let mut out = vec![
        format!("CREATE TYPE ct3 AS (a text{collate}, b int)"),
        "CREATE TABLE tc3 (id int PRIMARY KEY, c ct3, ca ct3[], n int)".to_string(),
    ];
    let rows: Vec<String> = (0..rng.range(0, 14))
        .map(|i| {
            let c = if rng.chance(0.2) { "NULL".to_string() } else { ct3_lit(rng) };
            let ca = match rng.below(4) {
                0 => "NULL".to_string(),
                1 => "'{}'".to_string(),
                _ => format!("ARRAY[{}, {}]", ct3_lit(rng), ct3_lit(rng)),
            };
            let n = if rng.chance(0.3) { "NULL".to_string() } else { rng.range(0, 4).to_string() };
            format!("({}, {c}, {ca}, {n})", i + 1)
        })
        .collect();
    if !rows.is_empty() {
        out.push(format!("INSERT INTO tc3 VALUES {}", rows.join(", ")));
    }
    if rng.chance(0.6) {
        out.push("CREATE INDEX tc3_c ON tc3 (c)".into());
    }
    if rng.chance(0.3) {
        out.push("CREATE INDEX tc3_cd ON tc3 (c DESC NULLS LAST, n)".into());
    }
    if rng.chance(0.4) {
        out.push("CREATE INDEX tc3_ca ON tc3 (ca)".into());
    }
    if rng.chance(0.2) {
        out.push("CREATE UNIQUE INDEX tc3_u ON tc3 (c)".into());
    }
    if range {
        out.push("CREATE TYPE ctr3 AS RANGE (subtype = ct3)".into());
    }
    out
}

impl Gen<'_> {
    /// One of round 3's statements over `tc3`: comparisons, sorts, grouping, DISTINCT,
    /// aggregates, joins, set operations, window functions and DML over composite values
    /// (their fields under ct3's collation), field accesses, row comparisons, arrays and
    /// ranges of them.
    fn composite_stmt(&mut self, range: bool) -> Stmt {
        let op = *self.rng.pick(&["<", "<=", ">", ">=", "=", "<>"]);
        let lit = ct3_lit(&mut self.rng);
        let lit2 = ct3_lit(&mut self.rng);
        let t = format!("'{}'", self.rng.pick(TEXTS));
        let dir = *self.rng.pick(&["", " DESC", " NULLS FIRST", " DESC NULLS LAST"]);
        let q = |sql: String, cols: &[(&str, Ty)]| Stmt::Query(Query { sql, ordered: true, cols: cols.iter().map(|(n, t)| (n.to_string(), *t)).collect() });
        let id = [("id", Int)];
        let txt = [("c", Text)];
        match self.rng.below(if range { 22 } else { 20 }) {
            0 => q(format!("SELECT c::text, count(*) FROM tc3 GROUP BY c ORDER BY c{dir}"), &[("c", Text), ("count", Big)]),
            1 => q(format!("SELECT s.c::text FROM (SELECT DISTINCT c FROM tc3) s ORDER BY s.c{dir}"), &txt),
            2 => q(format!("SELECT id FROM tc3 WHERE c {op} {lit} ORDER BY id"), &id),
            3 => q("SELECT min(c)::text, max(c)::text, count(DISTINCT c), min(ca)::text, max(ca)::text FROM tc3".into(), &[("a", Text), ("b", Text), ("n", Big), ("c", Text), ("d", Text)]),
            4 => q(format!("SELECT id FROM tc3 ORDER BY c{dir}, id"), &id),
            5 => q(format!("SELECT (c).a, (c).b, id FROM tc3 WHERE (c).a {op} {t} ORDER BY 1{dir}, 3"), &[("a", Text), ("b", Int), ("id", Int)]),
            6 => q(format!("SELECT id, rank() OVER (ORDER BY c{dir}), count(*) OVER (PARTITION BY c) FROM tc3 ORDER BY 1"), &[("id", Int), ("r", Big), ("n", Big)]),
            7 => q(format!("SELECT id FROM tc3 WHERE ca @> ARRAY[{lit}] OR ca {op} ARRAY[{lit2}] ORDER BY id"), &id),
            8 => q(format!("SELECT id FROM tc3 ORDER BY ca{dir}, id"), &id),
            9 => q(format!("SELECT x.id, y.id FROM tc3 x JOIN tc3 y ON x.c {op} y.c ORDER BY 1, 2"), &[("x", Int), ("y", Int)]),
            10 => q(format!("SELECT c::text FROM (SELECT c FROM tc3 UNION SELECT {lit}) s ORDER BY s.c{dir}"), &txt),
            11 => q(format!("SELECT id, ROW((c).a, id) < ROW({t}::text, 3), ((c).a, (c).b) {op} ({t}, 1) FROM tc3 ORDER BY id"), &[("id", Int), ("x", Bool), ("y", Bool)]),
            12 => q(format!("SELECT id FROM tc3 ORDER BY ROW((c).a, n){dir}, id"), &id),
            13 => q(format!("SELECT string_agg(id::text, ',' ORDER BY c{dir}, id), array_agg((c).a ORDER BY (c).a{dir}, id)::text FROM tc3"), &[("s", Text), ("a", Text)]),
            14 => q(format!("SELECT id, greatest(c, {lit})::text, c IN ({lit}, {lit2}) FROM tc3 ORDER BY id"), &[("id", Int), ("g", Text), ("i", Bool)]),
            15 => q(format!("SELECT id FROM tc3 WHERE c {op} ANY (ARRAY[{lit}, {lit2}]) ORDER BY id"), &id),
            16 => q("SELECT n, max(c)::text, count(DISTINCT (c).a) FROM tc3 GROUP BY n ORDER BY n".into(), &[("n", Int), ("m", Text), ("d", Big)]),
            17 => {
                let sql = match self.rng.below(3) {
                    0 => format!("UPDATE tc3 SET c = {lit} WHERE c {op} {lit2} RETURNING id, c::text"),
                    1 => format!("INSERT INTO tc3 VALUES ({}, {lit}, ARRAY[{lit2}], {}) ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c WHERE tc3.c {op} EXCLUDED.c RETURNING id, c::text", self.rng.range(1, 20), self.rng.range(0, 3)),
                    _ => format!("DELETE FROM tc3 WHERE (c).a {op} {t} RETURNING id"),
                };
                Stmt::Dml { sql, table: "tc3".into(), ordered: false }
            }
            18 => q(format!("SELECT x.id FROM tc3 x WHERE x.c {op} (SELECT min(y.c) FROM tc3 y WHERE y.n = x.n) ORDER BY 1"), &id),
            19 => q(format!("SELECT DISTINCT ON (c) c::text, id FROM tc3 ORDER BY c{dir}, id"), &[("c", Text), ("id", Int)]),
            20 => q(format!("SELECT ctr3(least(c, {lit}), greatest(c, {lit}))::text, ctr3(least(c, {lit}), greatest(c, {lit})) @> {lit2} FROM tc3 WHERE c IS NOT NULL ORDER BY id"), &[("r", Text), ("b", Bool)]),
            _ => q(format!("SELECT r::text FROM (SELECT ctr3(least(c, {lit}), greatest(c, {lit})) r, id FROM tc3 WHERE c IS NOT NULL) s ORDER BY r{dir}, id"), &txt),
        }
    }
}

/// The statements of one run after the schema: queries mostly, some DML and settings.
pub fn statements(seed: u64, s: &Schema, n: usize) -> Vec<Stmt> {
    let mut g = Gen::new(seed, s);
    let mut out = Vec::new();
    let mut serial = 0;
    let r3 = seed >= R3_SEEDS;
    let audit = || Stmt::Query(Query {
        sql: "SELECT op, k, v FROM audit3 ORDER BY op, k, v COLLATE \"C\"".into(),
        ordered: true,
        cols: vec![("op".into(), Text), ("k".into(), Int), ("v".into(), Text)],
    });
    while out.len() < n {
        g.alias = 0;
        if s.round4 && g.rng.chance(0.25) {
            out.extend(g.round4_stmt());
            continue;
        }
        if r3
            && let Some((_, range)) = s.composite
            && g.rng.chance(0.12)
        {
            out.push(g.composite_stmt(range));
            continue;
        }
        // (The wider grammar's statement kinds: prepared statements, cursors, savepoints,
        // recursive CTEs with SEARCH / CYCLE.)
        if g.wide && g.rng.chance(0.08) {
            serial += 1;
            let k = serial;
            match g.rng.below(5) {
                0 => {
                    let q = g.select(&[], None, false);
                    out.push(Stmt::Other(format!("PREPARE a45p{k}(int, text) AS SELECT * FROM ({}) s WHERE $1 IS NOT NULL OR $2 = 'x'", q.sql)));
                    let (a, b) = (g.rng.range(-1, 3), *g.rng.pick(TEXTS));
                    out.push(Stmt::Other(format!("EXECUTE a45p{k}({a}, '{b}')")));
                    out.push(Stmt::Other(format!("EXECUTE a45p{k}(NULL, 'x')")));
                    out.push(Stmt::Other(format!("DEALLOCATE a45p{k}")));
                }
                1 => {
                    let q = g.select(&[], None, false);
                    out.push(Stmt::Other("BEGIN".into()));
                    out.push(Stmt::Other(format!("DECLARE a45c{k} CURSOR FOR {}", q.sql)));
                    out.push(Stmt::Other(format!("FETCH ALL FROM a45c{k}")));
                    out.push(Stmt::Other(format!("CLOSE a45c{k}")));
                    out.push(Stmt::Other("COMMIT".into()));
                }
                2 => {
                    let q1 = g.select(&[], None, false);
                    let q2 = g.select(&[], None, false);
                    out.push(Stmt::Other("BEGIN".into()));
                    out.push(Stmt::Other("SAVEPOINT a45s".into()));
                    out.push(Stmt::Other(q1.sql));
                    out.push(Stmt::Other("ROLLBACK TO SAVEPOINT a45s".into()));
                    out.push(Stmt::Other(q2.sql));
                    out.push(Stmt::Other("COMMIT".into()));
                }
                3 => {
                    let step = g.rng.range(1, 5);
                    let m = g.rng.range(3, 9);
                    let (search, cols) = match g.rng.below(3) {
                        0 => ("SEARCH DEPTH FIRST BY n SET ord", "n, x, ord"),
                        1 => ("SEARCH BREADTH FIRST BY n SET ord", "n, x, ord"),
                        _ => ("CYCLE n SET is_cycle USING path", "n, x, is_cycle, path"),
                    };
                    out.push(Stmt::Other(format!(
                        "WITH RECURSIVE r(n, x) AS (VALUES (0, ''::text) UNION ALL SELECT (n * 2 + {step}) % {m}, x || n::text FROM r WHERE length(x) < 4) {search} SELECT {cols} FROM r"
                    )));
                }
                _ => {
                    // A window frame over a wide expression.
                    let q = g.select(&[], None, false);
                    out.push(Stmt::Other(format!(
                        "SELECT count(*) OVER (ORDER BY 1 {}) FROM ({}) s",
                        g.rng.pick(&[
                            "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE TIES",
                            "GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP",
                            "ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW",
                        ]),
                        q.sql
                    )));
                }
            }
            continue;
        }
        let r = g.rng.below(100);
        if r < 82 {
            let q = g.select(&[], None, false);
            out.push(Stmt::Query(q));
        } else if r < 96 {
            if let Some(d) = g.dml() {
                // (Triggers on the table log into audit3: its contents too.)
                let audited = matches!(&d, Stmt::Dml { table, .. } if s.tables.iter().any(|t| t.audited && &t.name == table));
                out.push(d);
                if r3 && audited {
                    out.push(audit());
                }
            }
        } else if r < 98 {
            // A DO block raising notices of computed values.
            let cx = Cx::default();
            let e = g.expr(&cx, Text, 2);
            out.push(Stmt::Other(format!(
                "DO $$ BEGIN RAISE NOTICE 'v=%', {e}; END $$"
            )));
        } else {
            let t = &s.tables[g.rng.below(s.tables.len())];
            if !t.is_view {
                out.push(Stmt::Other(format!("ANALYZE {}", t.name)));
            }
        }
    }
    out
}
