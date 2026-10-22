//! Printing the syntax tree back as CQL.
//!
//! [`Display`] on every node writes valid CQL that parses back to the same tree
//! (identifiers keep their quoting, literals their kind and text). It is a canonical form, not
//! the original text: whitespace and comments are normalised and keywords are upper case.
//!
//! [`cassandra_text`] renders a term the way Cassandra's `Term.Raw.getText()` does, which some
//! of its error messages embed.

use std::fmt::{self, Display, Formatter, Write};

use crate::ast::*;

fn write_list<T: Display>(f: &mut Formatter<'_>, items: &[T], sep: &str) -> fmt::Result {
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(sep)?;
        }
        write!(f, "{it}")?;
    }
    Ok(())
}

/// Writes `s` as a double-quoted identifier.
pub fn quote_identifier(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Cassandra's `ColumnIdentifier.maybeQuote`: names that are not `[a-z][a-z0-9_]*` or are
/// reserved keywords are double-quoted.
pub fn maybe_quote(s: &str) -> String {
    let plain = s.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if plain && !crate::tokens::is_reserved_word(s) { s.to_owned() } else { quote_identifier(s) }
}

/// Writes `s` as a single-quoted string literal.
pub fn quote_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Display for Ident<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.quoted { f.write_str(&quote_identifier(&self.text)) } else { f.write_str(&self.text) }
    }
}

impl Display for QualifiedName<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(ks) = &self.keyspace {
            write!(f, "{ks}.")?;
        }
        write!(f, "{}", self.name)
    }
}

impl Display for FunctionName<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(ks) = &self.keyspace {
            write!(f, "{ks}.")?;
        }
        write!(f, "{}", self.name)
    }
}

impl Display for RoleName<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.form {
            NameForm::Ident => f.write_str(&self.text),
            NameForm::Quoted => f.write_str(&quote_identifier(&self.text)),
            NameForm::String => f.write_str(&quote_string(&self.text)),
        }
    }
}

impl Display for NativeType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Display for RawType<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            RawType::Native(n) => write!(f, "{n}"),
            RawType::List(e) => write!(f, "list<{e}>"),
            RawType::Set(e) => write!(f, "set<{e}>"),
            RawType::Map(kv) => write!(f, "map<{}, {}>", kv.0, kv.1),
            RawType::Tuple(ts) => {
                f.write_str("tuple<")?;
                write_list(f, ts, ", ")?;
                f.write_str(">")
            }
            RawType::Vector(v) => write!(f, "vector<{}, {}>", v.0, v.1),
            RawType::Frozen(e) => write!(f, "frozen<{e}>"),
            RawType::User(n) => write!(f, "{n}"),
            RawType::Custom(c) => f.write_str(&quote_string(c)),
        }
    }
}

/// A type as ScyllaDB names it in messages (no spaces after commas).
pub(crate) fn type_text(t: &RawType<'_>) -> String {
    t.to_string().replace(", ", ",")
}

impl Display for Literal<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.kind {
            LiteralKind::String if self.dollar && !self.text.contains("$$") => write!(f, "$${}$$", self.text),
            LiteralKind::String => f.write_str(&quote_string(&self.text)),
            LiteralKind::List => {
                let items: Vec<String> = self.list_items().iter().map(|i| quote_string(i)).collect();
                write!(f, "[{}]", items.join(", "))
            }
            _ => f.write_str(&self.text),
        }
    }
}

impl Display for BindMarker<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(n) => write!(f, ":{n}"),
            None => f.write_str("?"),
        }
    }
}

impl Display for Term<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Term::Literal(l) => write!(f, "{l}"),
            Term::Null => f.write_str("NULL"),
            Term::Bind(b) => write!(f, "{b}"),
            Term::List(items) => {
                f.write_str("[")?;
                write_list(f, items, ", ")?;
                f.write_str("]")
            }
            Term::Set(items) => {
                f.write_str("{")?;
                write_list(f, items, ", ")?;
                f.write_str("}")
            }
            Term::Map(entries) => {
                f.write_str("{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
            Term::Udt(fields) => {
                f.write_str("{")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
            Term::Tuple(items) => {
                f.write_str("(")?;
                write_list(f, items, ", ")?;
                f.write_str(")")
            }
            Term::Function(c) => write!(f, "{c}"),
            Term::TypeHint(h) => write!(f, "({}) {}", h.ty, h.term),
            Term::Cast(c) => write!(f, "CAST({} AS {})", c.arg, c.ty),
            Term::Binary(b) => write!(f, "{} {} {}", b.left, b.op.as_str(), b.right),
            Term::Negate(t) => write!(f, "- {t}"),
        }
    }
}

impl Display for FunctionCall<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}(", self.name)?;
        write_list(f, &self.args, ", ")?;
        f.write_str(")")
    }
}

/// A term as Cassandra's `Term.Raw.getText()` renders it (strings quoted but not escaped,
/// operators as their native functions).
pub fn cassandra_text(t: &Term<'_>) -> String {
    fn join(items: &[Term<'_>]) -> String {
        items.iter().map(cassandra_text).collect::<Vec<_>>().join(", ")
    }
    match t {
        Term::Literal(l) if l.kind == LiteralKind::String => format!("'{}'", l.text),
        Term::Literal(l) => l.text.to_string(),
        Term::Null => "NULL".into(),
        Term::Bind(_) => "?".into(),
        Term::List(items) => format!("[{}]", join(items)),
        Term::Set(items) => format!("{{{}}}", join(items)),
        Term::Map(entries) => {
            let parts: Vec<String> =
                entries.iter().map(|(k, v)| format!("{}:{}", cassandra_text(k), cassandra_text(v))).collect();
            format!("{{{}}}", parts.join(", "))
        }
        Term::Udt(fields) => {
            let parts: Vec<String> = fields.iter().map(|(k, v)| format!("{}:{}", k.name(), cassandra_text(v))).collect();
            format!("{{{}}}", parts.join(", "))
        }
        Term::Tuple(items) => format!("({})", join(items)),
        Term::Function(c) => {
            let name = match &c.name.keyspace {
                Some(ks) => format!("{}.{}", maybe_quote(&ks.name()), maybe_quote(&c.name.resolved())),
                None => maybe_quote(&c.name.resolved()),
            };
            format!("{name}({})", join(&c.args))
        }
        Term::TypeHint(h) => format!("({}){}", h.ty, cassandra_text(&h.term)),
        Term::Cast(c) => {
            let ty = c.ty.as_str();
            let mut cap = ty[..1].to_uppercase();
            cap.push_str(&ty[1..]);
            format!("system.\"castAs{cap}\"({})", cassandra_text(&c.arg))
        }
        Term::Binary(b) => {
            let name = match b.op {
                ArithOp::Add => "_add",
                ArithOp::Sub => "_substract",
                ArithOp::Mul => "_multiply",
                ArithOp::Div => "_divide",
                ArithOp::Mod => "_modulo",
            };
            format!("system.\"{name}\"({}, {})", cassandra_text(&b.left), cassandra_text(&b.right))
        }
        Term::Negate(t) => format!("system.\"_negate\"({})", cassandra_text(t)),
    }
}

impl Display for Selectable<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Selectable::Column(c) => write!(f, "{c}"),
            Selectable::Term(t) => write!(f, "{t}"),
            Selectable::CountRows => f.write_str("COUNT(*)"),
            Selectable::Metadata(k, arg) => {
                let kw = match k {
                    MetadataKind::WriteTime => "WRITETIME",
                    MetadataKind::MaxWriteTime => "MAXWRITETIME",
                    MetadataKind::Ttl => "TTL",
                };
                write!(f, "{kw}({arg})")
            }
            Selectable::Cast(c) => write!(f, "CAST({} AS {})", c.arg, c.ty),
            Selectable::Function(n, args) => {
                write!(f, "{n}(")?;
                write_list(f, args, ", ")?;
                f.write_str(")")
            }
            Selectable::Field(r, field) => write!(f, "{r}.{field}"),
            Selectable::Element(r, k) => write!(f, "{r}[{k}]"),
            Selectable::Slice(s) => {
                write!(f, "{}[", s.receiver)?;
                if let Some(from) = &s.from {
                    write!(f, "{from}")?;
                }
                f.write_str("..")?;
                if let Some(to) = &s.to {
                    write!(f, "{to}")?;
                }
                f.write_str("]")
            }
            Selectable::TypeHint(t, s) => write!(f, "({t}) {s}"),
            Selectable::List(items) => {
                f.write_str("[")?;
                write_list(f, items, ", ")?;
                f.write_str("]")
            }
            Selectable::Set(items) => {
                f.write_str("{")?;
                write_list(f, items, ", ")?;
                f.write_str("}")
            }
            Selectable::MapOrUdt(entries) => {
                f.write_str("{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
            Selectable::Tuple(items) => {
                f.write_str("(")?;
                write_list(f, items, ", ")?;
                f.write_str(")")
            }
            Selectable::Binary(b) => write!(f, "{} {} {}", b.left, b.op.as_str(), b.right),
            Selectable::Negate(s) => write!(f, "- {s}"),
        }
    }
}

impl Display for Selector<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if let Some(a) = &self.alias {
            write!(f, " AS {a}")?;
        }
        Ok(())
    }
}

impl Display for Value<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Value::Term(t) => write!(f, "{t}"),
            Value::List(items) => {
                f.write_str("(")?;
                write_list(f, items, ", ")?;
                f.write_str(")")
            }
            Value::ListMarker(m) => write!(f, "{m}"),
        }
    }
}

fn write_op_value(f: &mut Formatter<'_>, op: Operator, value: &Value<'_>) -> fmt::Result {
    match op {
        Operator::IsNot => f.write_str(" IS NOT NULL"),
        _ => write!(f, " {} {value}", op.as_str()),
    }
}

impl Display for Relation<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Relation::Single(r) => {
                write!(f, "{}", r.column)?;
                if let Some(k) = &r.key {
                    write!(f, "[{k}]")?;
                }
                write_op_value(f, r.op, &r.value)
            }
            Relation::MultiColumn(r) => {
                f.write_str("(")?;
                write_list(f, &r.columns, ", ")?;
                f.write_str(")")?;
                write_op_value(f, r.op, &r.value)
            }
            Relation::Token(r) => {
                f.write_str("TOKEN(")?;
                write_list(f, &r.columns, ", ")?;
                write!(f, ") {} {}", r.op.as_str(), r.value)
            }
            Relation::Expression(e) => write!(f, "expr({}, {})", e.index, e.value),
            Relation::Function(r) => {
                write!(f, "{}(", r.function)?;
                write_list(f, &r.args, ", ")?;
                write!(f, ") {} {}", r.op.as_str(), r.value)
            }
        }
    }
}

fn write_where(f: &mut Formatter<'_>, w: &[Relation<'_>]) -> fmt::Result {
    if !w.is_empty() {
        f.write_str(" WHERE ")?;
        write_list(f, w, " AND ")?;
    }
    Ok(())
}

impl Display for Condition<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.column)?;
        match &self.target {
            ConditionTarget::Column => {}
            ConditionTarget::Element(e) => write!(f, "[{e}]")?,
            ConditionTarget::Field(fl) => write!(f, ".{fl}")?,
        }
        write_op_value(f, self.op, &self.value)
    }
}

impl Display for Conditions<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Conditions::None => Ok(()),
            Conditions::Exists => f.write_str(" IF EXISTS"),
            Conditions::List(c) => {
                f.write_str(" IF ")?;
                write_list(f, c, " AND ")
            }
        }
    }
}

impl Display for UsingClause<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.order.is_empty() {
            return Ok(());
        }
        f.write_str(" USING ")?;
        for (i, k) in self.order.iter().enumerate() {
            if i > 0 {
                f.write_str(" AND ")?;
            }
            if *k == UsingKind::ServiceLevel {
                if let Some(n) = &self.service_level {
                    write!(f, "SERVICE LEVEL {n}")?;
                }
                continue;
            }
            let (kw, v) = match k {
                UsingKind::ServiceLevel => continue,
                UsingKind::Timestamp => ("TIMESTAMP", &self.timestamp),
                UsingKind::Ttl => ("TTL", &self.ttl),
                UsingKind::Timeout => ("TIMEOUT", &self.timeout),
                UsingKind::Concurrency => ("CONCURRENCY", &self.concurrency),
            };
            write!(f, "{kw}")?;
            if let Some(v) = v {
                write!(f, " {v}")?;
            }
        }
        Ok(())
    }
}

impl Display for Direction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Asc => "ASC",
            Direction::Desc => "DESC",
        })
    }
}

impl Display for OrderBy<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.expr {
            OrderByExpr::Column(c) => write!(f, "{c}")?,
            OrderByExpr::Ann(c, v) => write!(f, "{c} ANN OF {v}")?,
            OrderByExpr::Function(n, args) => {
                write!(f, "{n}(")?;
                write_list(f, args, ", ")?;
                f.write_str(")")?;
            }
        }
        if let Some(d) = self.direction {
            write!(f, " {d}")?;
        }
        Ok(())
    }
}

impl Display for Select<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        if self.json {
            f.write_str("JSON ")?;
        }
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        if self.selectors.is_empty() {
            f.write_str("*")?;
        } else {
            write_list(f, &self.selectors, ", ")?;
        }
        if self.mutation_fragments {
            write!(f, " FROM MUTATION_FRAGMENTS({})", self.table)?;
        } else {
            write!(f, " FROM {}", self.table)?;
        }
        write_where(f, &self.where_clause)?;
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            write_list(f, &self.group_by, ", ")?;
        }
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            write_list(f, &self.order_by, ", ")?;
        }
        if let Some(l) = &self.per_partition_limit {
            write!(f, " PER PARTITION LIMIT {l}")?;
        }
        if let Some(l) = &self.limit {
            write!(f, " LIMIT {l}")?;
        }
        if self.allow_filtering {
            f.write_str(" ALLOW FILTERING")?;
        }
        if self.bypass_cache {
            f.write_str(" BYPASS CACHE")?;
        }
        write!(f, "{}", self.using)
    }
}

impl Display for Insert<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {} ", self.table)?;
        match &self.values {
            InsertValues::Values { columns, values } => {
                f.write_str("(")?;
                write_list(f, columns, ", ")?;
                f.write_str(") VALUES (")?;
                write_list(f, values, ", ")?;
                f.write_str(")")?;
            }
            InsertValues::Json { value, default } => {
                write!(f, "JSON {value}")?;
                match default {
                    Some(JsonDefault::Null) => f.write_str(" DEFAULT NULL")?,
                    Some(JsonDefault::Unset) => f.write_str(" DEFAULT UNSET")?,
                    None => {}
                }
            }
        }
        if self.if_not_exists {
            f.write_str(" IF NOT EXISTS")?;
        }
        write!(f, "{}", self.using)
    }
}

impl Display for Assignment<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let c = &self.column;
        match &self.op {
            Operation::Set(t) => write!(f, "{c} = {t}"),
            Operation::SetElement(k, t) => write!(f, "{c}[{k}] = {t}"),
            Operation::SetField(fl, t) => write!(f, "{c}.{fl} = {t}"),
            Operation::Add(t, OpForm::Long) => write!(f, "{c} = {c} + {t}"),
            Operation::Add(t, OpForm::Shorthand) => write!(f, "{c} += {t}"),
            Operation::Add(t, OpForm::Juxtaposed) => write!(f, "{c} = {c} {t}"),
            Operation::Subtract(t, OpForm::Shorthand) => write!(f, "{c} -= {t}"),
            Operation::Subtract(t, _) => write!(f, "{c} = {c} - {t}"),
            Operation::Prepend(t) => write!(f, "{c} = {t} + {c}"),
        }
    }
}

impl Display for Update<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "UPDATE {}{} SET ", self.table, self.using)?;
        write_list(f, &self.assignments, ", ")?;
        write_where(f, &self.where_clause)?;
        write!(f, "{}", self.conditions)
    }
}

impl Display for Deletion<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Deletion::Column(c) => write!(f, "{c}"),
            Deletion::Element(c, k) => write!(f, "{c}[{k}]"),
            Deletion::Field(c, fl) => write!(f, "{c}.{fl}"),
        }
    }
}

impl Display for Delete<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("DELETE ")?;
        if !self.columns.is_empty() {
            write_list(f, &self.columns, ", ")?;
            f.write_str(" ")?;
        }
        write!(f, "FROM {}{}", self.table, self.using)?;
        write_where(f, &self.where_clause)?;
        write!(f, "{}", self.conditions)?;
        if self.bypass_guardrails {
            f.write_str(" BYPASS LARGE_DATA_GUARDRAILS")?;
        }
        Ok(())
    }
}

impl Display for BatchStatement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            BatchStatement::Insert(s) => write!(f, "{s}"),
            BatchStatement::Update(s) => write!(f, "{s}"),
            BatchStatement::Delete(s) => write!(f, "{s}"),
        }
    }
}

impl Display for Batch<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("BEGIN ")?;
        match self.kind {
            BatchKind::Logged => {}
            BatchKind::Unlogged => f.write_str("UNLOGGED ")?,
            BatchKind::Counter => f.write_str("COUNTER ")?,
        }
        write!(f, "BATCH{}", self.using)?;
        for s in &self.statements {
            write!(f, " {s};")?;
        }
        f.write_str(" APPLY BATCH")
    }
}

impl Display for PropertyValue<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            PropertyValue::Simple(l) => write!(f, "{l}"),
            PropertyValue::Map(entries) => {
                f.write_str("{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
        }
    }
}

impl Display for Property<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.name, self.value)
    }
}

fn write_properties(f: &mut Formatter<'_>, props: &[Property<'_>]) -> fmt::Result {
    write_list(f, props, " AND ")
}

impl Display for Masking<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.function {
            None => f.write_str("MASKED WITH DEFAULT"),
            Some(n) => {
                write!(f, "MASKED WITH {n}(")?;
                write_list(f, &self.args, ", ")?;
                f.write_str(")")
            }
        }
    }
}

impl Display for PrimaryKey<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("PRIMARY KEY (")?;
        if self.composite {
            f.write_str("(")?;
            write_list(f, &self.partition, ", ")?;
            f.write_str(")")?;
        } else {
            write_list(f, &self.partition, ", ")?;
        }
        for c in &self.clustering {
            write!(f, ", {c}")?;
        }
        f.write_str(")")
    }
}

impl Display for TableOption<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            TableOption::Property(p) => write!(f, "{p}"),
            TableOption::CompactStorage => f.write_str("COMPACT STORAGE"),
            TableOption::ClusteringOrder(order) => {
                f.write_str("CLUSTERING ORDER BY (")?;
                for (i, (c, d)) in order.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{c} {d}")?;
                }
                f.write_str(")")
            }
        }
    }
}

fn write_table_options(f: &mut Formatter<'_>, options: &[TableOption<'_>]) -> fmt::Result {
    if !options.is_empty() {
        f.write_str(" WITH ")?;
        write_list(f, options, " AND ")?;
    }
    Ok(())
}

impl Display for ColumnDef<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.ty)?;
        if self.ttl {
            f.write_str(" TTL")?;
        }
        if self.is_static {
            f.write_str(" STATIC")?;
        }
        if let Some(m) = &self.mask {
            write!(f, " {m}")?;
        }
        if self.primary_key {
            f.write_str(" PRIMARY KEY")?;
        }
        Ok(())
    }
}

impl Display for CreateTable<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE TABLE ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{} (", self.name)?;
        write_list(f, &self.columns, ", ")?;
        for pk in &self.primary_keys {
            // Keys written inline are part of their column definition.
            let inline = !pk.composite
                && pk.clustering.is_empty()
                && pk.partition.len() == 1
                && self.columns.iter().any(|c| c.primary_key && c.name == pk.partition[0]);
            if !inline {
                write!(f, ", {pk}")?;
            }
        }
        f.write_str(")")?;
        write_table_options(f, &self.options)
    }
}

impl Display for AddColumn<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.ty)?;
        if self.is_static {
            f.write_str(" STATIC")?;
        }
        if let Some(m) = &self.mask {
            write!(f, " {m}")?;
        }
        Ok(())
    }
}

impl Display for AlterTable<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("ALTER TABLE ")?;
        if self.if_exists {
            f.write_str("IF EXISTS ")?;
        }
        write!(f, "{} ", self.name)?;
        match &self.action {
            AlterTableAction::AlterType { column, ty } => write!(f, "ALTER {column} TYPE {ty}"),
            AlterTableAction::Mask { if_exists, column, mask } => {
                f.write_str("ALTER ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                match mask {
                    Some(m) => write!(f, "{column} {m}"),
                    None => write!(f, "{column} DROP MASKED"),
                }
            }
            AlterTableAction::Add { if_not_exists, columns, parenthesized } => {
                f.write_str("ADD ")?;
                if *if_not_exists {
                    f.write_str("IF NOT EXISTS ")?;
                }
                if *parenthesized {
                    f.write_str("(")?;
                    write_list(f, columns, ", ")?;
                    f.write_str(")")
                } else {
                    write_list(f, columns, ", ")
                }
            }
            AlterTableAction::DropObject { if_exists, columns, parenthesized, timestamp } => {
                f.write_str("DROP ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                if *parenthesized {
                    f.write_str("(")?;
                    write_list(f, columns, ", ")?;
                    f.write_str(")")?;
                } else {
                    write_list(f, columns, ", ")?;
                }
                if let Some(ts) = timestamp {
                    write!(f, " USING TIMESTAMP {ts}")?;
                }
                Ok(())
            }
            AlterTableAction::Rename { if_exists, renames } => {
                f.write_str("RENAME ")?;
                if *if_exists {
                    f.write_str("IF EXISTS ")?;
                }
                for (i, (a, b)) in renames.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" AND ")?;
                    }
                    write!(f, "{a} TO {b}")?;
                }
                Ok(())
            }
            AlterTableAction::DropCompactStorage => f.write_str("DROP COMPACT STORAGE"),
            AlterTableAction::With(p) => {
                f.write_str("WITH ")?;
                write_properties(f, p)
            }
            AlterTableAction::Ttl(None) => f.write_str("TTL NULL"),
            AlterTableAction::Ttl(Some(c)) => write!(f, "TTL {c}"),
        }
    }
}

impl Display for IndexTarget<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            IndexTarget::Column(IndexTargetKind::Simple, c) => write!(f, "{c}"),
            IndexTarget::Column(k, c) => {
                let kw = match k {
                    IndexTargetKind::Values => "VALUES",
                    IndexTargetKind::Keys => "KEYS",
                    IndexTargetKind::Entries => "ENTRIES",
                    IndexTargetKind::Full => "FULL",
                    IndexTargetKind::Simple => "",
                };
                write!(f, "{kw}({c})")
            }
            IndexTarget::PartitionKey(cols) => {
                f.write_str("(")?;
                write_list(f, cols, ", ")?;
                f.write_str(")")
            }
        }
    }
}

fn if_not_exists(f: &mut Formatter<'_>, b: bool) -> fmt::Result {
    if b { f.write_str("IF NOT EXISTS ") } else { Ok(()) }
}

fn if_exists(f: &mut Formatter<'_>, b: bool) -> fmt::Result {
    if b { f.write_str("IF EXISTS ") } else { Ok(()) }
}

fn type_list(f: &mut Formatter<'_>, types: &[RawType<'_>]) -> fmt::Result {
    f.write_str("(")?;
    write_list(f, types, ", ")?;
    f.write_str(")")
}

impl Display for Permissions {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Permissions::All { keyword: true } => f.write_str("ALL PERMISSIONS"),
            Permissions::All { keyword: false } => f.write_str("ALL"),
            Permissions::List(l) => {
                for (i, (p, kw)) in l.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(p.as_str())?;
                    if *kw {
                        f.write_str(" PERMISSION")?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl Display for Resource<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Resource::AllKeyspaces => f.write_str("ALL KEYSPACES"),
            Resource::Keyspace(k) => write!(f, "KEYSPACE {k}"),
            Resource::Table { name, keyword: true } => write!(f, "TABLE {name}"),
            Resource::Table { name, keyword: false } => write!(f, "{name}"),
            Resource::AllTablesInKeyspace(k) => write!(f, "ALL TABLES IN KEYSPACE {k}"),
            Resource::AllRoles => f.write_str("ALL ROLES"),
            Resource::Role(r) => write!(f, "ROLE {r}"),
            Resource::AllFunctions => f.write_str("ALL FUNCTIONS"),
            Resource::AllFunctionsInKeyspace(k) => write!(f, "ALL FUNCTIONS IN KEYSPACE {k}"),
            Resource::Function { name, arg_types } => {
                write!(f, "FUNCTION {name}")?;
                type_list(f, arg_types)
            }
            Resource::AllMbeans => f.write_str("ALL MBEANS"),
            Resource::Mbean { name, plural } => {
                write!(f, "{} {}", if *plural { "MBEANS" } else { "MBEAN" }, quote_string(name))
            }
        }
    }
}

impl Display for RoleOption<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let strings = |f: &mut Formatter<'_>, items: &[std::borrow::Cow<'_, str>]| -> fmt::Result {
            f.write_str("{")?;
            for (i, s) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(&quote_string(s))?;
            }
            f.write_str("}")
        };
        match self {
            RoleOption::Password(p) => write!(f, "PASSWORD = {}", quote_string(p)),
            RoleOption::HashedPassword(p) => write!(f, "HASHED PASSWORD = {}", quote_string(p)),
            RoleOption::Options(m) => write!(f, "OPTIONS = {}", PropertyValue::Map(m.clone())),
            RoleOption::Superuser(b) => write!(f, "SUPERUSER = {b}"),
            RoleOption::Login(b) => write!(f, "LOGIN = {b}"),
            RoleOption::AccessToAllDatacenters => f.write_str("ACCESS TO ALL DATACENTERS"),
            RoleOption::AccessToDatacenters(d) => {
                f.write_str("ACCESS TO DATACENTERS ")?;
                strings(f, d)
            }
            RoleOption::AccessFromAllCidrs => f.write_str("ACCESS FROM ALL CIDRS"),
            RoleOption::AccessFromCidrs(c) => {
                f.write_str("ACCESS FROM CIDRS ")?;
                strings(f, c)
            }
        }
    }
}

fn write_role_statement(f: &mut Formatter<'_>, verb: &str, s: &RoleStatement<'_>, create: bool) -> fmt::Result {
    write!(f, "{verb} ROLE ")?;
    if s.if_exists {
        f.write_str(if create { "IF NOT EXISTS " } else { "IF EXISTS " })?;
    }
    write!(f, "{}", s.name)?;
    if s.with {
        f.write_str(" WITH ")?;
        write_list(f, &s.options, " AND ")?;
    }
    Ok(())
}

fn write_user_statement(f: &mut Formatter<'_>, verb: &str, s: &UserStatement<'_>, create: bool) -> fmt::Result {
    write!(f, "{verb} USER ")?;
    if s.if_exists {
        f.write_str(if create { "IF NOT EXISTS " } else { "IF EXISTS " })?;
    }
    write!(f, "{}", s.name)?;
    match &s.password {
        Some(UserPassword::Plain(p)) => write!(f, " WITH PASSWORD {}", quote_string(p))?,
        Some(UserPassword::Hashed(p)) => write!(f, " WITH HASHED PASSWORD {}", quote_string(p))?,
        None => {}
    }
    match s.superuser {
        Some(true) => f.write_str(" SUPERUSER"),
        Some(false) => f.write_str(" NOSUPERUSER"),
        None => Ok(()),
    }
}

impl Display for Describe<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(if self.short { "DESC " } else { "DESCRIBE " })?;
        match &self.target {
            DescribeTarget::Cluster => f.write_str("CLUSTER")?,
            DescribeTarget::Schema { full: true } => f.write_str("FULL SCHEMA")?,
            DescribeTarget::Schema { full: false } => f.write_str("SCHEMA")?,
            DescribeTarget::Keyspaces => f.write_str("KEYSPACES")?,
            DescribeTarget::Keyspace { only, name } => {
                if *only {
                    f.write_str("ONLY ")?;
                }
                f.write_str("KEYSPACE")?;
                if let Some(n) = name {
                    write!(f, " {n}")?;
                }
            }
            DescribeTarget::Tables => f.write_str("TABLES")?,
            DescribeTarget::Table(n) => write!(f, "TABLE {n}")?,
            DescribeTarget::Index(n) => write!(f, "INDEX {n}")?,
            DescribeTarget::MaterializedView(n) => write!(f, "MATERIALIZED VIEW {n}")?,
            DescribeTarget::Types => f.write_str("TYPES")?,
            DescribeTarget::Type(n) => write!(f, "TYPE {n}")?,
            DescribeTarget::Functions => f.write_str("FUNCTIONS")?,
            DescribeTarget::Function(n) => write!(f, "FUNCTION {n}")?,
            DescribeTarget::Aggregates => f.write_str("AGGREGATES")?,
            DescribeTarget::Aggregate(n) => write!(f, "AGGREGATE {n}")?,
            DescribeTarget::Generic(n) => write!(f, "{n}")?,
        }
        if self.with_internals {
            f.write_str(" WITH INTERNALS")?;
        }
        if self.with_passwords {
            f.write_str(" AND PASSWORDS")?;
        }
        Ok(())
    }
}

impl Display for ListServiceLevels<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ListServiceLevels::One(n) => write!(f, "LIST SERVICE LEVEL {n}"),
            ListServiceLevels::All => f.write_str("LIST ALL SERVICE LEVELS"),
            ListServiceLevels::AttachedOf(r) => write!(f, "LIST ATTACHED SERVICE LEVEL OF {r}"),
            ListServiceLevels::AllAttached => f.write_str("LIST ALL ATTACHED SERVICE LEVELS"),
            ListServiceLevels::EffectiveOf(r) => write!(f, "LIST EFFECTIVE SERVICE LEVEL OF {r}"),
        }
    }
}

impl Display for StatementKind<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use StatementKind as S;
        match self {
            S::Select(s) => write!(f, "{s}"),
            S::Insert(s) => write!(f, "{s}"),
            S::Update(s) => write!(f, "{s}"),
            S::Delete(s) => write!(f, "{s}"),
            S::Batch(s) => write!(f, "{s}"),
            S::Use(k) => write!(f, "USE {k}"),
            S::Truncate(t) => write!(f, "TRUNCATE TABLE {}{}", t.table, t.using),
            S::CreateKeyspace(s) => {
                f.write_str("CREATE KEYSPACE ")?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{}", s.name)?;
                if !s.properties.is_empty() {
                    f.write_str(" WITH ")?;
                    write_properties(f, &s.properties)?;
                }
                Ok(())
            }
            S::AlterKeyspace(s) => {
                f.write_str("ALTER KEYSPACE ")?;
                if_exists(f, s.if_exists)?;
                write!(f, "{} WITH ", s.name)?;
                write_properties(f, &s.properties)
            }
            S::DropKeyspace(d) => drop(f, "KEYSPACE", d),
            S::CreateTable(s) => write!(f, "{s}"),
            S::AlterTable(s) => write!(f, "{s}"),
            S::DropTable(d) => drop(f, "TABLE", d),
            S::CreateIndex(s) => {
                f.write_str(if s.custom { "CREATE CUSTOM INDEX " } else { "CREATE INDEX " })?;
                if_not_exists(f, s.if_not_exists)?;
                if let Some(n) = &s.name {
                    write!(f, "{n} ")?;
                }
                write!(f, "ON {} (", s.table)?;
                write_list(f, &s.targets, ", ")?;
                f.write_str(")")?;
                if let Some(u) = &s.using {
                    write!(f, " USING {}", quote_string(u))?;
                }
                if !s.properties.is_empty() {
                    f.write_str(" WITH ")?;
                    write_properties(f, &s.properties)?;
                }
                for (i, o) in s.view_options.iter().enumerate() {
                    f.write_str(if i == 0 && s.properties.is_empty() { " WITH " } else { " AND " })?;
                    write!(f, "{o}")?;
                }
                Ok(())
            }
            S::DropIndex(d) => drop(f, "INDEX", d),
            S::CreateType(s) => {
                f.write_str("CREATE TYPE ")?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{} (", s.name)?;
                for (i, (n, t)) in s.fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{n} {t}")?;
                }
                f.write_str(")")
            }
            S::AlterType(s) => {
                f.write_str("ALTER TYPE ")?;
                if_exists(f, s.if_exists)?;
                write!(f, "{} ", s.name)?;
                match &s.action {
                    AlterTypeAction::AlterField { field, ty } => write!(f, "ALTER {field} TYPE {ty}"),
                    AlterTypeAction::Add { if_not_exists: ine, field, ty } => {
                        f.write_str("ADD ")?;
                        if_not_exists(f, *ine)?;
                        write!(f, "{field} {ty}")
                    }
                    AlterTypeAction::Rename { if_exists: ie, renames } => {
                        f.write_str("RENAME ")?;
                        if_exists(f, *ie)?;
                        for (i, (a, b)) in renames.iter().enumerate() {
                            if i > 0 {
                                f.write_str(" AND ")?;
                            }
                            write!(f, "{a} TO {b}")?;
                        }
                        Ok(())
                    }
                }
            }
            S::DropType(d) => drop(f, "TYPE", d),
            S::CreateView(s) => {
                f.write_str("CREATE MATERIALIZED VIEW ")?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{} AS SELECT ", s.name)?;
                if s.selectors.is_empty() {
                    f.write_str("*")?;
                } else {
                    write_list(f, &s.selectors, ", ")?;
                }
                write!(f, " FROM {}", s.base)?;
                write_where(f, &s.where_clause)?;
                write!(f, " {}", s.primary_key)?;
                write_table_options(f, &s.options)
            }
            S::AlterView(s) => {
                f.write_str("ALTER MATERIALIZED VIEW ")?;
                if_exists(f, s.if_exists)?;
                write!(f, "{} WITH ", s.name)?;
                write_properties(f, &s.properties)
            }
            S::DropView(d) => drop(f, "MATERIALIZED VIEW", d),
            S::CreateFunction(s) => {
                f.write_str(if s.or_replace { "CREATE OR REPLACE FUNCTION " } else { "CREATE FUNCTION " })?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{}(", s.name)?;
                for (i, (n, t)) in s.args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{n} {t}")?;
                }
                f.write_str(") ")?;
                f.write_str(if s.called_on_null_input { "CALLED ON NULL INPUT" } else { "RETURNS NULL ON NULL INPUT" })?;
                write!(f, " RETURNS {} LANGUAGE {} AS {}", s.return_type, s.language, s.body)
            }
            S::DropFunction(d) => drop_function(f, "FUNCTION", d),
            S::CreateAggregate(s) => {
                f.write_str(if s.or_replace { "CREATE OR REPLACE AGGREGATE " } else { "CREATE AGGREGATE " })?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{}", s.name)?;
                type_list(f, &s.arg_types)?;
                write!(f, " SFUNC {} STYPE {}", s.state_function, s.state_type)?;
                if let Some(r) = &s.reduce_function {
                    write!(f, " REDUCEFUNC {r}")?;
                }
                if let Some(ff) = &s.final_function {
                    write!(f, " FINALFUNC {ff}")?;
                }
                if let Some(i) = &s.initial_condition {
                    write!(f, " INITCOND {i}")?;
                }
                Ok(())
            }
            S::DropAggregate(d) => drop_function(f, "AGGREGATE", d),
            S::CreateTrigger(s) => {
                f.write_str("CREATE TRIGGER ")?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{} ON {} USING {}", s.name, s.table, quote_string(&s.class))
            }
            S::DropTrigger(s) => {
                f.write_str("DROP TRIGGER ")?;
                if_exists(f, s.if_exists)?;
                write!(f, "{} ON {}", s.name, s.table)
            }
            S::Grant(s) => write!(f, "GRANT {} ON {} TO {}", s.permissions, s.resource, s.role),
            S::Revoke(s) => write!(f, "REVOKE {} ON {} FROM {}", s.permissions, s.resource, s.role),
            S::ListPermissions(s) => {
                write!(f, "LIST {}", s.permissions)?;
                if let Some(r) = &s.resource {
                    write!(f, " ON {r}")?;
                }
                if let Some(o) = &s.of {
                    write!(f, " OF {o}")?;
                }
                if !s.recursive {
                    f.write_str(" NORECURSIVE")?;
                }
                Ok(())
            }
            S::CreateRole(s) => write_role_statement(f, "CREATE", s, true),
            S::AlterRole(s) => write_role_statement(f, "ALTER", s, false),
            S::DropRole(d) => {
                f.write_str("DROP ROLE ")?;
                if_exists(f, d.if_exists)?;
                write!(f, "{}", d.name)
            }
            S::ListRoles(s) => {
                f.write_str("LIST ROLES")?;
                if let Some(o) = &s.of {
                    write!(f, " OF {o}")?;
                }
                if !s.recursive {
                    f.write_str(" NORECURSIVE")?;
                }
                Ok(())
            }
            S::GrantRole(g) => write!(f, "GRANT {} TO {}", g.role, g.grantee),
            S::RevokeRole(g) => write!(f, "REVOKE {} FROM {}", g.role, g.grantee),
            S::CreateUser(s) => write_user_statement(f, "CREATE", s, true),
            S::AlterUser(s) => write_user_statement(f, "ALTER", s, false),
            S::DropUser(d) => {
                f.write_str("DROP USER ")?;
                if_exists(f, d.if_exists)?;
                write!(f, "{}", d.name)
            }
            S::ListUsers => f.write_str("LIST USERS"),
            S::AddIdentity(s) => {
                f.write_str("ADD IDENTITY ")?;
                if_not_exists(f, s.if_not_exists)?;
                write!(f, "{} TO ROLE {}", s.identity, s.role)
            }
            S::DropIdentity(s) => {
                f.write_str("DROP IDENTITY ")?;
                if_exists(f, s.if_exists)?;
                write!(f, "{}", s.identity)
            }
            S::Describe(d) => write!(f, "{d}"),
            S::PruneView(p) => {
                write!(f, "PRUNE MATERIALIZED VIEW {}", p.view)?;
                write_where(f, &p.where_clause)?;
                write!(f, "{}", p.using)
            }
            S::CreateServiceLevel(s) => {
                f.write_str("CREATE SERVICE LEVEL ")?;
                if_not_exists(f, s.if_exists)?;
                write!(f, "{}", s.name)?;
                if !s.properties.is_empty() {
                    f.write_str(" WITH ")?;
                    write_properties(f, &s.properties)?;
                }
                Ok(())
            }
            S::AlterServiceLevel(s) => {
                f.write_str("ALTER SERVICE LEVEL ")?;
                if_exists(f, s.if_exists)?;
                write!(f, "{} WITH ", s.name)?;
                write_properties(f, &s.properties)
            }
            S::DropServiceLevel(d) => {
                f.write_str("DROP SERVICE LEVEL ")?;
                if_exists(f, d.if_exists)?;
                write!(f, "{}", d.name)
            }
            S::AttachServiceLevel(a) => write!(f, "ATTACH SERVICE LEVEL {} TO {}", a.role, a.grantee),
            S::DetachServiceLevel(r) => write!(f, "DETACH SERVICE LEVEL FROM {r}"),
            S::ListServiceLevels(l) => write!(f, "{l}"),
        }
    }
}

fn drop(f: &mut Formatter<'_>, what: &str, d: &DropObject<'_>) -> fmt::Result {
    write!(f, "DROP {what} ")?;
    if_exists(f, d.if_exists)?;
    write!(f, "{}", d.name)
}

fn drop_function(f: &mut Formatter<'_>, what: &str, d: &DropFunction<'_>) -> fmt::Result {
    write!(f, "DROP {what} ")?;
    if_exists(f, d.if_exists)?;
    write!(f, "{}", d.name)?;
    if let Some(types) = &d.arg_types {
        type_list(f, types)?;
    }
    Ok(())
}

impl Display for Statement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

/// Writes `items` as CQL statements separated by `;\n`.
pub fn script(items: &[Statement<'_>]) -> String {
    let mut out = String::new();
    for s in items {
        let _ = writeln!(out, "{s};");
    }
    out
}
