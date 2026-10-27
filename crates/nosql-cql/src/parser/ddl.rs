//! Schema statements and `DESCRIBE`: keyspaces, tables, types, indexes, materialized views,
//! functions, aggregates and triggers, and the `WITH` properties they share. Ports the
//! corresponding rules of Cassandra's `Parser.g`, including the checks its actions make while
//! parsing (duplicate columns and properties, map properties of constants only).

use std::borrow::Cow;

use super::dfa_tables as dfa;
use super::{F_END, F_IDENT, PResult, Parser, is_constant, is_term_start};
use crate::Dialect;
use crate::ast::*;
use crate::tokens::{self as t, Tt};

impl<'a> Parser<'a> {
    /// `IF NOT EXISTS`, if present.
    fn if_not_exists(&mut self) -> PResult<bool> {
        if !self.at(t::K_IF) {
            return Ok(false);
        }
        self.consume();
        self.expect_f(t::K_NOT, &[t::K_EXISTS])?;
        self.expect_f(t::K_EXISTS, &[F_IDENT])?;
        Ok(true)
    }

    /// `IF EXISTS`, if present.
    fn if_exists(&mut self) -> PResult<bool> {
        if !self.at(t::K_IF) {
            return Ok(false);
        }
        self.consume();
        self.expect_f(t::K_EXISTS, &[F_IDENT])?;
        Ok(true)
    }

    /// `IF EXISTS` where only Cassandra 5 has it.
    fn if_exists_c5(&mut self) -> PResult<bool> {
        if self.scylla() { Ok(false) } else { self.if_exists() }
    }

    // -----------------------------------------------------------------------------------------
    // Properties
    // -----------------------------------------------------------------------------------------

    /// `properties: property (AND property)*`.
    pub(crate) fn properties(&mut self) -> PResult<Vec<Property<'a>>> {
        let mut out = Vec::new();
        self.property(&mut out)?;
        while self.eat(t::K_AND) {
            self.property(&mut out)?;
        }
        Ok(out)
    }

    /// `property: noncol_ident '=' (propertyValue | fullMapLiteral)`.
    pub(crate) fn property(&mut self, out: &mut Vec<Property<'a>>) -> PResult<()> {
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA247)?
        } else {
            // ScyllaDB decides on `name = value-start` and fails at the name.
            let third = self.la(3);
            let value = is_constant(third)
                || matches!(third, t::LBRACE | t::K_NULL | t::MINUS)
                || t::is_unreserved(third, self.d);
            if !t::is_ident(self.la(1), self.d) || self.la(2) != t::EQ || !value {
                return self.no_viable(0);
            }
            if third == t::LBRACE { 2 } else { 1 }
        };
        let name = self.ident()?;
        self.expect_f(t::EQ, &[t::LBRACE, super::F_TERM, F_IDENT])?;
        let value = if alt == 1 {
            let ty = self.la(1);
            if is_constant(ty) {
                PropertyValue::Simple(self.constant()?)
            } else if ty == t::MINUS && self.scylla() {
                return Err(match self.scylla_minus() {
                    Ok(_) => super::Fail,
                    Err(f) => f,
                });
            } else if ty == t::K_NULL && self.scylla() {
                self.consume();
                PropertyValue::Simple(Literal { kind: LiteralKind::String, text: Cow::Borrowed("null"), dollar: false })
            } else if t::is_unreserved(ty, self.d) {
                let tok = self.consume();
                let raw = &self.src[tok.start as usize..tok.end as usize];
                PropertyValue::Simple(Literal { kind: LiteralKind::String, text: Cow::Borrowed(raw), dollar: false })
            } else {
                return self.no_viable(0);
            }
        } else {
            let map = self.full_map_literal()?;
            PropertyValue::Map(self.convert_property_map(map)?)
        };
        let key = name.name().into_owned();
        if out.iter().any(|p| p.name.name() == key) {
            self.duplicate_property(&key)?;
        }
        out.push(Property { name, value });
        Ok(())
    }

    /// `PropertyDefinitions.addProperty` on a name already defined.
    fn duplicate_property(&mut self, key: &str) -> PResult<()> {
        let msg = match self.d {
            Dialect::Cassandra => format!("Multiple definitions for property '{key}'"),
            Dialect::Scylla => format!("Multiple definition for property '{key}'"),
        };
        self.custom_error(msg)
    }

    /// `fullMapLiteral: '{' (term ':' term (',' term ':' term)*)? '}'`.
    fn full_map_literal(&mut self) -> PResult<Vec<(Term<'a>, Term<'a>)>> {
        self.expect(t::LBRACE)?;
        let mut out = Vec::new();
        let ty = self.la(1);
        if is_term_start(ty, self.d) || ty == t::MINUS {
            let k = self.term()?;
            self.expect_f(t::COLON, &[super::F_TERM])?;
            let v = self.term()?;
            out.push((k, v));
            while self.eat(t::COMMA) {
                let k = self.term()?;
                self.expect_f(t::COLON, &[super::F_TERM])?;
                let v = self.term()?;
                out.push((k, v));
            }
        }
        self.expect_f(t::RBRACE, &[F_END, t::K_AND])?;
        Ok(out)
    }

    /// `convertPropertyMap`: keys and values must be constants, keys unique.
    fn convert_property_map(&mut self, map: Vec<(Term<'a>, Term<'a>)>) -> PResult<Vec<(Literal<'a>, Literal<'a>)>> {
        let mut out: Vec<(Literal<'a>, Literal<'a>)> = Vec::with_capacity(map.len());
        for (k, v) in map {
            let Term::Literal(k) = k else {
                let mut msg = format!("Invalid property name: {}", crate::display::cassandra_text(&k));
                if matches!(k, Term::Bind(_)) {
                    msg.push_str(" (bind variables are not supported in DDL queries)");
                }
                self.custom_error(msg)?;
                break;
            };
            // ScyllaDB also takes a list of constants (rack lists).
            let v = match v {
                Term::List(items) if self.scylla() && items.iter().all(|i| matches!(i, Term::Literal(_))) => {
                    let mut text = String::new();
                    for i in &items {
                        if let Term::Literal(l) = i {
                            text.push_str(&l.text);
                            text.push('\0');
                        }
                    }
                    Term::Literal(Literal { kind: LiteralKind::List, text: Cow::Owned(text), dollar: false })
                }
                // ScyllaDB keeps `null` as the constant's text.
                Term::Null if self.scylla() => {
                    Term::Literal(Literal { kind: LiteralKind::String, text: Cow::Borrowed("null"), dollar: false })
                }
                v => v,
            };
            let Term::Literal(v) = v else {
                let mut msg = format!(
                    "Invalid property value: {} for property: {}",
                    crate::display::cassandra_text(&v),
                    crate::display::cassandra_text(&Term::Literal(k.clone()))
                );
                if matches!(v, Term::Bind(_)) {
                    msg.push_str(" (bind variables are not supported in DDL queries)");
                }
                self.custom_error(msg)?;
                break;
            };
            if out.iter().any(|(ok, _)| ok.text == k.text) {
                self.custom_error(format!("Multiple definition for property {}", k.text))?;
            }
            out.push((k, v));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------------------------
    // Keyspaces
    // -----------------------------------------------------------------------------------------

    pub(crate) fn create_keyspace(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_KEYSPACE)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.keyspace_name()?;
        let properties = if self.scylla() {
            if self.eat(t::K_WITH) { self.properties()? } else { Vec::new() }
        } else {
            self.expect(t::K_WITH)?;
            self.properties()?
        };
        Ok(StatementKind::CreateKeyspace(Box::new(CreateKeyspace { if_not_exists, name, properties })))
    }

    pub(crate) fn alter_keyspace(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.expect(t::K_KEYSPACE)?;
        let if_exists = self.if_exists_c5()?;
        let name = self.keyspace_name()?;
        self.expect_f(t::K_WITH, &[F_IDENT])?;
        let properties = self.properties()?;
        Ok(StatementKind::AlterKeyspace(Box::new(AlterKeyspace { if_exists, name, properties })))
    }

    pub(crate) fn drop_keyspace(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_KEYSPACE)?;
        let if_exists = self.if_exists()?;
        let ks = self.keyspace_name()?;
        Ok(StatementKind::DropKeyspace(Box::new(DropObject { if_exists, name: QualifiedName { keyspace: None, name: ks } })))
    }

    // -----------------------------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------------------------

    pub(crate) fn create_table(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_COLUMNFAMILY)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.column_family_name()?;
        let mut stmt = CreateTable { if_not_exists, name, columns: Vec::new(), primary_keys: Vec::new(), options: Vec::new() };
        self.expect_f(t::LPAREN, &[F_IDENT, t::K_PRIMARY])?;
        self.table_columns(&mut stmt)?;
        while self.eat(t::COMMA) {
            let ty = self.la(1);
            if t::is_ident(ty, self.d) || ty == t::K_PRIMARY {
                self.table_columns(&mut stmt)?;
            }
        }
        self.expect_f(t::RPAREN, &[F_END, t::K_WITH])?;
        if self.eat(t::K_WITH) {
            let what = "table";
            let o = self.table_property(&stmt.name, what, &stmt.options)?;
            stmt.options.push(o);
            while self.eat(t::K_AND) {
                let o = self.table_property(&stmt.name, what, &stmt.options)?;
                stmt.options.push(o);
            }
        }
        Ok(StatementKind::CreateTable(Box::new(stmt)))
    }

    /// `tableColumns`: a column definition or a `PRIMARY KEY (...)` element.
    fn table_columns(&mut self, stmt: &mut CreateTable<'a>) -> PResult<()> {
        let ty = self.la(1);
        if t::is_ident(ty, self.d) {
            let name = self.ident()?;
            let col_ty = self.comparator_type()?;
            // ScyllaDB: `type TTL? STATIC?`.
            let mut ttl = false;
            if self.scylla() && self.at(t::K_TTL) {
                self.consume();
                ttl = true;
            }
            let is_static = self.eat(t::K_STATIC);
            let mask = if self.cassandra() && self.at(t::K_MASKED) { Some(self.column_mask()?) } else { None };
            let key = name.name().into_owned();
            if stmt.columns.iter().any(|c| c.name.name() == key) {
                let table = qualified_display(&stmt.name);
                self.raise_invalid(format!("Duplicate column '{key}' declaration for table '{table}'"))?;
            }
            let mut def = ColumnDef { name, ty: col_ty, is_static, mask, ttl, primary_key: false };
            if self.at(t::K_PRIMARY) {
                self.consume();
                self.expect_f(t::K_KEY, &[F_END, t::COMMA, t::RPAREN])?;
                def.primary_key = true;
                let pk = PrimaryKey { partition: vec![def.name.clone()], composite: false, clustering: Vec::new() };
                self.set_primary_key(stmt, pk)?;
            }
            stmt.columns.push(def);
            return Ok(());
        }
        if ty == t::K_PRIMARY {
            self.consume();
            self.expect_f(t::K_KEY, &[t::LPAREN])?;
            self.expect_f(t::LPAREN, &[F_IDENT, t::LPAREN])?;
            let (partition, composite) = self.partition_key(false)?;
            let mut clustering = Vec::new();
            while self.eat(t::COMMA) {
                clustering.push(self.ident()?);
            }
            self.expect_f(t::RPAREN, &[F_END])?;
            self.set_primary_key(stmt, PrimaryKey { partition, composite, clustering })?;
            return Ok(());
        }
        self.no_viable(0)
    }

    fn set_primary_key(&mut self, stmt: &mut CreateTable<'a>, pk: PrimaryKey<'a>) -> PResult<()> {
        if !stmt.primary_keys.is_empty() {
            let table = qualified_display(&stmt.name);
            self.raise_invalid(format!("Multiple PRIMARY KEY specified for table '{table}' (exactly one required)"))?;
        }
        stmt.primary_keys.push(pk);
        Ok(())
    }

    /// Cassandra throws `InvalidRequestException` from some parser actions; the parse stops
    /// there and the client gets an invalid-request error. ScyllaDB checks later.
    fn raise_invalid(&mut self, msg: String) -> PResult<()> {
        if self.cassandra() && self.spec == 0 { Err(self.throw(crate::SyntaxError::invalid(msg))) } else { Ok(()) }
    }

    /// The `(` of `PRIMARY KEY (`; ScyllaDB decides the partition key form at it.
    fn primary_key_open(&mut self) -> PResult<()> {
        if self.scylla() && self.at(t::LPAREN) {
            let next = self.la(2);
            if !(t::is_ident(next, self.d) || next == t::LPAREN) {
                return self.no_viable(0);
            }
        }
        self.expect_f(t::LPAREN, &[F_IDENT, t::LPAREN])?;
        Ok(())
    }

    /// `tablePartitionKey` / `viewPartitionKey`: `a` or `(a, b)`.
    fn partition_key(&mut self, view: bool) -> PResult<(Vec<Ident<'a>>, bool)> {
        let ty = self.la(1);
        if t::is_ident(ty, self.d) {
            return Ok((vec![self.ident()?], false));
        }
        if ty == t::LPAREN {
            self.consume();
            let mut cols = vec![self.ident()?];
            while self.eat(t::COMMA) {
                cols.push(self.ident()?);
            }
            // ScyllaDB reports a view's as missing only before what can follow it.
            let follow: &[Tt] = if view { &[t::COMMA, t::RPAREN] } else { &[F_END] };
            self.expect_f(t::RPAREN, follow)?;
            return Ok((cols, true));
        }
        self.no_viable(0)
    }

    /// `columnMask: MASKED WITH functionName(args) | MASKED WITH DEFAULT` (Cassandra 5).
    fn column_mask(&mut self) -> PResult<Masking<'a>> {
        // Decision 98: `DEFAULT` is an unreserved keyword, so it can also name a function.
        if self.la(1) != t::K_MASKED {
            return self.no_viable(0);
        }
        if self.la(2) != t::K_WITH {
            return self.no_viable(1);
        }
        let third = self.la(3);
        let is_default = if third == t::K_DEFAULT {
            match self.la(4) {
                t::EOF | t::K_PRIMARY | t::RPAREN | t::COMMA | t::SEMI => true,
                t::LPAREN | t::DOT => false,
                _ => return self.no_viable(3),
            }
        } else if t::is_ident(third, self.d) || matches!(third, t::QMARK | t::K_TOKEN | t::K_COUNT) {
            false
        } else {
            return self.no_viable(2);
        };
        self.consume();
        self.consume();
        if is_default {
            self.consume();
            return Ok(Masking { function: None, args: Vec::new() });
        }
        let function = self.function_name()?;
        if self.la(1) != t::LPAREN {
            return self.no_viable(0);
        }
        let mut args = Vec::new();
        match self.la(2) {
            t::RPAREN => {
                self.consume();
                self.consume();
            }
            ty if is_term_start(ty, self.d) || ty == t::MINUS => {
                self.consume();
                args.push(self.term()?);
                while self.eat(t::COMMA) {
                    args.push(self.term()?);
                }
                self.expect(t::RPAREN)?;
            }
            _ => return self.no_viable(1),
        }
        Ok(Masking { function: Some(function), args })
    }

    /// `tableProperty` / `viewProperty`.
    fn table_property(
        &mut self,
        name: &QualifiedName<'a>,
        what: &str,
        existing: &[TableOption<'a>],
    ) -> PResult<TableOption<'a>> {
        let ty = self.la(1);
        let alt = match ty {
            t::K_COMPACT => match self.la(2) {
                t::K_STORAGE => 2,
                t::EQ => 1,
                _ => return self.no_viable(1),
            },
            t::K_CLUSTERING => match self.la(2) {
                t::K_ORDER => 3,
                t::EQ => 1,
                _ => return self.no_viable(1),
            },
            _ if t::is_ident(ty, self.d) => 1,
            _ => return self.no_viable(0),
        };
        match alt {
            1 => {
                let mut one = Vec::new();
                self.property(&mut one)?;
                let Some(p) = one.pop() else { return Err(super::Fail) };
                let key = p.name.name().into_owned();
                let dup = existing.iter().any(|o| matches!(o, TableOption::Property(q) if q.name.name() == key));
                if dup {
                    self.duplicate_property(&key)?;
                }
                Ok(TableOption::Property(p))
            }
            2 => {
                self.consume();
                self.consume();
                if what == "view" && self.cassandra() {
                    let msg = "COMPACT STORAGE tables are not allowed starting with version 4.0";
                    return Err(self.throw(crate::SyntaxError::custom(msg)));
                }
                Ok(TableOption::CompactStorage)
            }
            _ => {
                self.consume();
                self.consume();
                self.expect_f(t::K_BY, &[t::LPAREN])?;
                self.expect_f(t::LPAREN, &[F_IDENT])?;
                let mut order: Vec<(Ident<'a>, Direction)> = Vec::new();
                loop {
                    let c = self.ident()?;
                    let dir = match self.la(1) {
                        t::K_ASC => Direction::Asc,
                        t::K_DESC => Direction::Desc,
                        _ => return self.no_viable(0),
                    };
                    self.consume();
                    let key = c.name().into_owned();
                    if order.iter().any(|(o, _)| o.name() == key) {
                        let n = qualified_display(name);
                        self.raise_invalid(format!("Duplicate column '{key}' in CLUSTERING ORDER BY clause for {what} '{n}'"))?;
                    }
                    order.push((c, dir));
                    if !self.eat(t::COMMA) {
                        break;
                    }
                }
                self.expect_f(t::RPAREN, &[F_END, t::K_AND])?;
                Ok(TableOption::ClusteringOrder(order))
            }
        }
    }

    pub(crate) fn alter_table(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.expect(t::K_COLUMNFAMILY)?;
        let if_exists = self.if_exists_c5()?;
        let name = self.column_family_name()?;
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA145)?
        } else {
            match self.la(1) {
                t::K_ALTER => 1,
                t::K_ADD => 3,
                t::K_DROP => 4,
                t::K_RENAME => 5,
                t::K_WITH => 7,
                t::K_TTL => 8,
                _ => return self.no_viable(0),
            }
        };
        let action = match alt {
            1 => {
                self.expect(t::K_ALTER)?;
                let column = self.cident()?;
                self.expect_f(t::K_TYPE, &[F_IDENT])?;
                let ty = self.comparator_type()?;
                AlterTableAction::AlterType { column, ty }
            }
            2 => {
                self.expect(t::K_ALTER)?;
                let if_exists = self.if_exists()?;
                let column = self.cident()?;
                let mask = match self.la(1) {
                    t::K_MASKED => Some(self.column_mask()?),
                    t::K_DROP => {
                        self.consume();
                        self.expect(t::K_MASKED)?;
                        None
                    }
                    _ => return self.no_viable(0),
                };
                AlterTableAction::Mask { if_exists, column, mask }
            }
            3 => {
                self.expect(t::K_ADD)?;
                let if_not_exists = if self.scylla() { false } else { self.if_not_exists()? };
                let ty = self.la(1);
                let (columns, parenthesized) = if t::is_ident(ty, self.d) {
                    (vec![self.add_column()?], false)
                } else if ty == t::LPAREN {
                    self.consume();
                    let mut cols = vec![self.add_column()?];
                    while self.eat(t::COMMA) {
                        cols.push(self.add_column()?);
                    }
                    self.expect_f(t::RPAREN, &[F_END])?;
                    (cols, true)
                } else {
                    return self.no_viable(0);
                };
                AlterTableAction::Add { if_not_exists, columns, parenthesized }
            }
            4 => {
                self.expect(t::K_DROP)?;
                let if_exists = if self.scylla() { false } else { self.if_exists()? };
                let ty = self.la(1);
                let (columns, parenthesized) = if t::is_ident(ty, self.d) {
                    (vec![self.ident()?], false)
                } else if ty == t::LPAREN {
                    self.consume();
                    let mut cols = vec![self.ident()?];
                    while self.eat(t::COMMA) {
                        cols.push(self.ident()?);
                    }
                    self.expect_f(t::RPAREN, &[F_END])?;
                    (cols, true)
                } else {
                    return self.no_viable(0);
                };
                let mut timestamp = None;
                if self.at(t::K_USING) {
                    self.consume();
                    self.expect_f(t::K_TIMESTAMP, &[t::INTEGER])?;
                    if self.scylla() && matches!(self.la(1), t::QMARK | t::COLON) {
                        timestamp = Some(Term::Bind(self.bind_marker()?));
                    } else {
                        if self.scylla() && !self.at(t::INTEGER) {
                            return self.no_viable(0);
                        }
                        let tok = self.expect(t::INTEGER)?;
                        let text = self.text(&tok);
                        if text.parse::<i64>().is_err() && self.cassandra() {
                            return Err(self.number_format_error(&text));
                        }
                        timestamp = Some(Term::Literal(Literal { kind: LiteralKind::Integer, text, dollar: false }));
                    }
                }
                AlterTableAction::DropObject { if_exists, columns, parenthesized, timestamp }
            }
            5 => {
                self.expect(t::K_RENAME)?;
                let if_exists = if self.scylla() { false } else { self.if_exists()? };
                let mut renames = Vec::new();
                loop {
                    let from = self.ident()?;
                    self.expect_f(t::K_TO, &[F_IDENT])?;
                    let to = self.ident()?;
                    renames.push((from, to));
                    if !self.eat(t::K_AND) {
                        break;
                    }
                }
                AlterTableAction::Rename { if_exists, renames }
            }
            6 => {
                self.expect(t::K_DROP)?;
                self.expect(t::K_COMPACT)?;
                self.expect(t::K_STORAGE)?;
                AlterTableAction::DropCompactStorage
            }
            7 => {
                self.expect(t::K_WITH)?;
                AlterTableAction::With(self.properties()?)
            }
            _ => {
                self.expect(t::K_TTL)?;
                if self.eat(t::K_NULL) {
                    AlterTableAction::Ttl(None)
                } else {
                    AlterTableAction::Ttl(Some(self.ident()?))
                }
            }
        };
        Ok(StatementKind::AlterTable(Box::new(AlterTable { if_exists, name, action })))
    }

    /// `ident comparatorType isStaticColumn columnMask?` in `ALTER TABLE ... ADD`.
    fn add_column(&mut self) -> PResult<AddColumn<'a>> {
        let name = self.ident()?;
        let ty = self.comparator_type()?;
        let is_static = self.eat(t::K_STATIC);
        let mask = if self.cassandra() && self.at(t::K_MASKED) { Some(self.column_mask()?) } else { None };
        Ok(AddColumn { name, ty, is_static, mask })
    }

    pub(crate) fn drop_table(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_COLUMNFAMILY)?;
        let if_exists = self.if_exists()?;
        let name = self.column_family_name()?;
        Ok(StatementKind::DropTable(Box::new(DropObject { if_exists, name })))
    }

    // -----------------------------------------------------------------------------------------
    // Types
    // -----------------------------------------------------------------------------------------

    pub(crate) fn create_type(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_TYPE)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.user_type_name()?;
        self.expect_f(t::LPAREN, &[F_IDENT])?;
        let mut fields = Vec::new();
        let f = self.ident()?;
        let ty = self.comparator_type()?;
        fields.push((f, ty));
        while self.eat(t::COMMA) {
            if t::is_ident(self.la(1), self.d) {
                let f = self.ident()?;
                let ty = self.comparator_type()?;
                fields.push((f, ty));
            }
        }
        self.expect_f(t::RPAREN, &[F_END])?;
        Ok(StatementKind::CreateType(Box::new(CreateType { if_not_exists, name, fields })))
    }

    pub(crate) fn alter_type(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.expect(t::K_TYPE)?;
        let if_exists = self.if_exists_c5()?;
        let name = self.user_type_name()?;
        let action = match self.la(1) {
            t::K_ALTER => {
                self.consume();
                let field = self.ident()?;
                self.expect_f(t::K_TYPE, &[F_IDENT])?;
                let ty = self.comparator_type()?;
                AlterTypeAction::AlterField { field, ty }
            }
            t::K_ADD => {
                self.consume();
                let if_not_exists = if self.scylla() { false } else { self.if_not_exists()? };
                let field = self.ident()?;
                let ty = self.comparator_type()?;
                AlterTypeAction::Add { if_not_exists, field, ty }
            }
            t::K_RENAME => {
                self.consume();
                let if_exists = if self.scylla() { false } else { self.if_exists()? };
                let mut renames = Vec::new();
                loop {
                    let from = self.ident()?;
                    self.expect_f(t::K_TO, &[F_IDENT])?;
                    let to = self.ident()?;
                    renames.push((from, to));
                    if !self.eat(t::K_AND) {
                        break;
                    }
                }
                AlterTypeAction::Rename { if_exists, renames }
            }
            _ => return self.no_viable(0),
        };
        Ok(StatementKind::AlterType(Box::new(AlterType { if_exists, name, action })))
    }

    pub(crate) fn drop_type(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_TYPE)?;
        let if_exists = self.if_exists()?;
        let name = self.user_type_name()?;
        Ok(StatementKind::DropType(Box::new(DropObject { if_exists, name })))
    }

    // -----------------------------------------------------------------------------------------
    // Indexes
    // -----------------------------------------------------------------------------------------

    pub(crate) fn create_index(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        let custom = self.eat(t::K_CUSTOM);
        self.expect_f(t::K_INDEX, &[F_IDENT, t::K_IF, t::K_ON])?;
        let if_not_exists = self.if_not_exists()?;
        let ty = self.la(1);
        let name = if t::is_ident(ty, self.d) || ty == t::QMARK { Some(self.object_name_pub("index")?) } else { None };
        self.expect_f(t::K_ON, &[F_IDENT])?;
        let table = self.column_family_name()?;
        self.expect_f(t::LPAREN, &[F_IDENT, t::RPAREN, t::LPAREN])?;
        let mut targets = Vec::new();
        let ty = self.la(1);
        let starts = ty == t::EMPTY_QUOTED_NAME
            || t::is_ident(ty, self.d)
            || matches!(ty, t::K_ENTRIES | t::K_FULL)
            || (self.scylla() && ty == t::LPAREN);
        if starts {
            targets.push(self.index_ident()?);
            while self.eat(t::COMMA) {
                targets.push(self.index_ident()?);
            }
        }
        self.expect_f(t::RPAREN, &[F_END, t::K_USING, t::K_WITH])?;
        let using = if self.eat(t::K_USING) {
            let tok = self.expect_f(t::STRING_LITERAL, &[F_END])?;
            Some(self.text(&tok))
        } else {
            None
        };
        let mut properties = Vec::new();
        let mut view_options = Vec::new();
        if self.eat(t::K_WITH) {
            if self.scylla() {
                // ScyllaDB takes any view property (`cfamProperty`) on an index.
                let mut all: Vec<TableOption<'a>> = Vec::new();
                loop {
                    let o = self.table_property(&table, "index", &all)?;
                    all.push(o);
                    if !self.eat(t::K_AND) {
                        break;
                    }
                }
                for o in all {
                    match o {
                        TableOption::Property(p) => properties.push(p),
                        o => view_options.push(o),
                    }
                }
            } else {
                properties = self.properties()?;
            }
        }
        Ok(StatementKind::CreateIndex(Box::new(CreateIndex {
            custom,
            if_not_exists,
            name,
            table,
            targets,
            using,
            properties,
            view_options,
        })))
    }

    /// `indexIdent`: `col`, `VALUES(col)`, `KEYS(col)`, `ENTRIES(col)`, `FULL(col)`, and
    /// ScyllaDB's local-index partition key `(a, b)`.
    fn index_ident(&mut self) -> PResult<IndexTarget<'a>> {
        let ty = self.la(1);
        let kind = match ty {
            t::K_VALUES | t::K_KEYS => match self.la(2) {
                t::LPAREN => Some(if ty == t::K_VALUES { IndexTargetKind::Values } else { IndexTargetKind::Keys }),
                t::RPAREN | t::COMMA => None,
                _ => return self.no_viable(1),
            },
            t::K_ENTRIES => Some(IndexTargetKind::Entries),
            t::K_FULL => Some(IndexTargetKind::Full),
            t::LPAREN if self.scylla() => {
                self.consume();
                let mut cols = vec![self.cident()?];
                while self.eat(t::COMMA) {
                    cols.push(self.cident()?);
                }
                self.expect_f(t::RPAREN, &[F_END])?;
                return Ok(IndexTarget::PartitionKey(cols));
            }
            _ if ty == t::EMPTY_QUOTED_NAME || t::is_ident(ty, self.d) => None,
            _ => return self.no_viable(0),
        };
        match kind {
            None => Ok(IndexTarget::Column(IndexTargetKind::Simple, self.cident()?)),
            Some(k) => {
                self.consume();
                self.expect_f(t::LPAREN, &[F_IDENT])?;
                let c = self.cident()?;
                self.expect_f(t::RPAREN, &[F_END])?;
                Ok(IndexTarget::Column(k, c))
            }
        }
    }

    pub(crate) fn drop_index(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_INDEX)?;
        let if_exists = self.if_exists()?;
        let name = self.index_name()?;
        Ok(StatementKind::DropIndex(Box::new(DropObject { if_exists, name })))
    }

    // -----------------------------------------------------------------------------------------
    // Materialized views
    // -----------------------------------------------------------------------------------------

    pub(crate) fn create_view(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_MATERIALIZED)?;
        self.expect_f(t::K_VIEW, &[F_IDENT, t::K_IF])?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.column_family_name()?;
        self.expect_f(t::K_AS, &[t::K_SELECT])?;
        self.expect_f(t::K_SELECT, &[F_IDENT, t::STAR])?;
        let selectors = self.selectors()?;
        self.expect_f(t::K_FROM, &[F_IDENT])?;
        let base = self.column_family_name()?;
        let where_clause = if self.eat(t::K_WHERE) { self.where_clause()? } else { Vec::new() };
        self.expect_f(t::K_PRIMARY, &[t::K_KEY])?;
        self.expect_f(t::K_KEY, &[t::LPAREN])?;
        self.primary_key_open()?;
        let (partition, composite) = self.partition_key(true)?;
        let mut clustering = Vec::new();
        while self.eat(t::COMMA) {
            clustering.push(self.ident()?);
        }
        self.expect_f(t::RPAREN, &[F_END, t::K_WITH])?;
        let primary_key = PrimaryKey { partition, composite, clustering };
        let mut options = Vec::new();
        if self.eat(t::K_WITH) {
            let o = self.table_property(&name, "view", &options)?;
            options.push(o);
            while self.eat(t::K_AND) {
                let o = self.table_property(&name, "view", &options)?;
                options.push(o);
            }
        }
        Ok(StatementKind::CreateView(Box::new(CreateView {
            if_not_exists,
            name,
            selectors,
            base,
            where_clause,
            primary_key,
            options,
        })))
    }

    pub(crate) fn alter_view(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.expect(t::K_MATERIALIZED)?;
        self.expect_f(t::K_VIEW, &[F_IDENT, t::K_IF])?;
        let if_exists = self.if_exists_c5()?;
        let name = self.column_family_name()?;
        self.expect_f(t::K_WITH, &[F_IDENT])?;
        let properties = self.properties()?;
        Ok(StatementKind::AlterView(Box::new(AlterView { if_exists, name, properties })))
    }

    pub(crate) fn drop_view(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_MATERIALIZED)?;
        self.expect_f(t::K_VIEW, &[F_IDENT, t::K_IF])?;
        let if_exists = self.if_exists()?;
        let name = self.column_family_name()?;
        Ok(StatementKind::DropView(Box::new(DropObject { if_exists, name })))
    }

    // -----------------------------------------------------------------------------------------
    // Functions, aggregates, triggers
    // -----------------------------------------------------------------------------------------

    /// `CREATE [OR REPLACE]` prefix.
    fn create_or_replace(&mut self) -> PResult<bool> {
        self.expect(t::K_CREATE)?;
        if self.at(t::K_OR) {
            self.consume();
            self.expect(t::K_REPLACE)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn type_list(&mut self) -> PResult<Vec<RawType<'a>>> {
        let mut out = Vec::new();
        if is_type_start(self.la(1), self.d) {
            out.push(self.comparator_type()?);
            while self.eat(t::COMMA) {
                out.push(self.comparator_type()?);
            }
        }
        Ok(out)
    }

    pub(crate) fn create_function(&mut self) -> PResult<StatementKind<'a>> {
        let or_replace = self.create_or_replace()?;
        if self.scylla() && !(self.la(2) == t::K_IF || t::is_ident(self.la(2), self.d) || self.la(2) == t::QMARK) {
            return if or_replace { self.no_viable_deep(1) } else { self.no_viable(0) };
        }
        // ScyllaDB's grammar has `IF NOT EXISTS` only without `OR REPLACE`.
        if self.scylla() && or_replace && self.la(2) == t::K_IF {
            return self.no_viable_deep(1);
        }
        self.expect(t::K_FUNCTION)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.function_name()?;
        self.expect_f(t::LPAREN, &[F_IDENT, t::RPAREN])?;
        let mut args = Vec::new();
        if t::is_ident(self.la(1), self.d) {
            let n = self.ident()?;
            let ty = self.comparator_type()?;
            args.push((n, ty));
            while self.eat(t::COMMA) {
                let n = self.ident()?;
                let ty = self.comparator_type()?;
                args.push((n, ty));
            }
        }
        self.expect_f(t::RPAREN, &[t::K_RETURNS, t::K_CALLED])?;
        let called_on_null_input = match self.la(1) {
            t::K_RETURNS => {
                self.consume();
                self.expect_f(t::K_NULL, &[t::K_ON])?;
                false
            }
            t::K_CALLED => {
                self.consume();
                true
            }
            _ => return self.no_viable(0),
        };
        self.expect_f(t::K_ON, &[t::K_NULL])?;
        self.expect_f(t::K_NULL, &[t::K_INPUT])?;
        self.expect_f(t::K_INPUT, &[t::K_RETURNS])?;
        self.expect_f(t::K_RETURNS, &[F_IDENT])?;
        let return_type = self.comparator_type()?;
        self.expect_f(t::K_LANGUAGE, &[t::IDENT])?;
        let lang = self.expect_f(t::IDENT, &[t::K_AS])?;
        let language = Cow::Owned(self.src[lang.start as usize..lang.end as usize].to_lowercase());
        self.expect_f(t::K_AS, &[t::STRING_LITERAL])?;
        let body_tok = self.expect_f(t::STRING_LITERAL, &[F_END])?;
        let body = self.literal_from(&body_tok);
        Ok(StatementKind::CreateFunction(Box::new(CreateFunction {
            or_replace,
            if_not_exists,
            name,
            args,
            called_on_null_input,
            return_type,
            language,
            body,
        })))
    }

    fn drop_function_like(&mut self, kw: Tt) -> PResult<DropFunction<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(kw)?;
        let if_exists = self.if_exists()?;
        let name = self.function_name()?;
        let arg_types = if self.eat(t::LPAREN) {
            let types = self.type_list()?;
            self.expect_f(t::RPAREN, &[F_END])?;
            Some(types)
        } else {
            None
        };
        Ok(DropFunction { if_exists, name, arg_types })
    }

    pub(crate) fn drop_function(&mut self) -> PResult<StatementKind<'a>> {
        Ok(StatementKind::DropFunction(Box::new(self.drop_function_like(t::K_FUNCTION)?)))
    }

    pub(crate) fn drop_aggregate(&mut self) -> PResult<StatementKind<'a>> {
        Ok(StatementKind::DropAggregate(Box::new(self.drop_function_like(t::K_AGGREGATE)?)))
    }

    pub(crate) fn create_aggregate(&mut self) -> PResult<StatementKind<'a>> {
        let or_replace = self.create_or_replace()?;
        self.expect(t::K_AGGREGATE)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.function_name()?;
        self.expect_f(t::LPAREN, &[F_IDENT, t::RPAREN])?;
        let arg_types = self.type_list()?;
        self.expect_f(t::RPAREN, &[t::K_SFUNC])?;
        self.expect_f(t::K_SFUNC, &[F_IDENT])?;
        let state_function = self.bare_function_name()?;
        self.expect_f(t::K_STYPE, &[F_IDENT])?;
        let state_type = self.comparator_type()?;
        let mut reduce_function = None;
        if self.scylla() && self.at(t::K_REDUCEFUNC) {
            self.consume();
            reduce_function = Some(self.bare_function_name()?);
        }
        let final_function = if self.eat(t::K_FINALFUNC) { Some(self.bare_function_name()?) } else { None };
        let initial_condition = if self.eat(t::K_INITCOND) { Some(self.term()?) } else { None };
        Ok(StatementKind::CreateAggregate(Box::new(CreateAggregate {
            or_replace,
            if_not_exists,
            name,
            arg_types,
            state_function,
            state_type,
            reduce_function,
            final_function,
            initial_condition,
        })))
    }

    /// `allowedFunctionName` as a [`FunctionName`] without keyspace.
    fn bare_function_name(&mut self) -> PResult<FunctionName<'a>> {
        let (name, keyword) = self.allowed_function_name()?;
        Ok(FunctionName { keyspace: None, name, keyword })
    }

    pub(crate) fn create_trigger(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_TRIGGER)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.trigger_name()?;
        self.expect(t::K_ON)?;
        let table = self.column_family_name()?;
        self.expect(t::K_USING)?;
        let tok = self.expect(t::STRING_LITERAL)?;
        let class = self.text(&tok);
        let Some(name) = name else { return Err(self.null_trigger_name()) };
        Ok(StatementKind::CreateTrigger(Box::new(CreateTrigger { if_not_exists, name, table, class })))
    }

    pub(crate) fn drop_trigger(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_TRIGGER)?;
        let if_exists = self.if_exists()?;
        let name = self.trigger_name()?;
        self.expect(t::K_ON)?;
        let table = self.column_family_name()?;
        let Some(name) = name else { return Err(self.null_trigger_name()) };
        Ok(StatementKind::DropTrigger(Box::new(DropTrigger { if_exists, name, table })))
    }

    /// A trigger name. When it is missing Cassandra carries on, then fails dereferencing it.
    fn trigger_name(&mut self) -> PResult<Option<Ident<'a>>> {
        let at = self.pos;
        match self.ident() {
            Ok(n) => Ok(Some(n)),
            Err(_) if self.pos == at && self.spec == 0 => Ok(None),
            Err(f) => Err(f),
        }
    }

    fn null_trigger_name(&mut self) -> super::Fail {
        self.null_pointer("Cannot invoke \"org.apache.cassandra.cql3.ColumnIdentifier.toString()\" because \"name\" is null")
    }

    // -----------------------------------------------------------------------------------------
    // DESCRIBE
    // -----------------------------------------------------------------------------------------

    /// `describeStatement`.
    pub(crate) fn describe(&mut self) -> PResult<StatementKind<'a>> {
        let first = self.la(1);
        if first != t::K_DESCRIBE && first != t::K_DESC {
            return self.mismatched_set();
        }
        self.consume();
        let short = first == t::K_DESC;
        let alt = if self.cassandra() { self.predict(&dfa::DFA202)? } else { self.scylla_describe_alt()? };
        let target = match alt {
            1 => {
                self.expect(t::K_CLUSTER)?;
                DescribeTarget::Cluster
            }
            2 => {
                let full = self.eat(t::K_FULL);
                self.expect_f(t::K_SCHEMA, &[F_END])?;
                DescribeTarget::Schema { full }
            }
            3 => {
                self.expect(t::K_KEYSPACES)?;
                DescribeTarget::Keyspaces
            }
            4 => {
                let only = self.eat(t::K_ONLY);
                self.expect(t::K_KEYSPACE)?;
                let ty = self.la(1);
                let name = if t::is_ident(ty, self.d) || ty == t::QMARK { Some(self.keyspace_name()?) } else { None };
                DescribeTarget::Keyspace { only, name }
            }
            5 => {
                self.expect(t::K_TABLES)?;
                DescribeTarget::Tables
            }
            6 => {
                self.expect(t::K_COLUMNFAMILY)?;
                DescribeTarget::Table(self.column_family_name()?)
            }
            7 => {
                self.expect(t::K_INDEX)?;
                DescribeTarget::Index(self.column_family_name()?)
            }
            8 => {
                self.expect(t::K_MATERIALIZED)?;
                self.expect_f(t::K_VIEW, &[F_IDENT])?;
                DescribeTarget::MaterializedView(self.column_family_name()?)
            }
            9 => {
                self.expect(t::K_TYPES)?;
                DescribeTarget::Types
            }
            10 => {
                self.expect(t::K_TYPE)?;
                let mut in_name = false;
                match self.user_type_name_staged(&mut in_name) {
                    Ok(n) => DescribeTarget::Type(n),
                    Err(_) if in_name => {
                        return Err(self.null_pointer(
                            "Cannot invoke \"org.apache.cassandra.cql3.ColumnIdentifier.toString()\" because \"this.utName\" is null",
                        ));
                    }
                    Err(_) => {
                        return Err(self.null_pointer(
                            "Cannot invoke \"org.apache.cassandra.cql3.UTName.getKeyspace()\" because \"tn\" is null",
                        ));
                    }
                }
            }
            11 => {
                self.expect(t::K_FUNCTIONS)?;
                DescribeTarget::Functions
            }
            12 => {
                self.expect(t::K_FUNCTION)?;
                match self.function_name() {
                    Ok(n) => DescribeTarget::Function(n),
                    Err(_) => return Err(self.null_pointer("Cannot read field \"keyspace\" because \"fn\" is null")),
                }
            }
            13 => {
                self.expect(t::K_AGGREGATES)?;
                DescribeTarget::Aggregates
            }
            14 => {
                self.expect(t::K_AGGREGATE)?;
                match self.function_name() {
                    Ok(n) => DescribeTarget::Aggregate(n),
                    Err(_) => return Err(self.null_pointer("Cannot read field \"keyspace\" because \"ag\" is null")),
                }
            }
            _ => {
                let has_ks = if self.cassandra() {
                    self.predict(&dfa::DFA200)? == 1
                } else {
                    t::is_ident(self.la(1), self.d) && self.la(2) == t::DOT
                };
                let keyspace = if has_ks {
                    let ks = self.ident()?;
                    self.expect(t::DOT)?;
                    Some(ks)
                } else {
                    None
                };
                let name = self.ident()?;
                DescribeTarget::Generic(QualifiedName { keyspace, name })
            }
        };
        let mut with_internals = false;
        let mut with_passwords = false;
        if self.at(t::K_WITH) {
            self.consume();
            self.expect_f(t::K_INTERNALS, &[F_END, t::K_AND])?;
            with_internals = true;
            if self.scylla() && self.at(t::K_AND) {
                self.consume();
                self.expect_f(t::K_PASSWORDS, &[F_END])?;
                with_passwords = true;
            }
        }
        Ok(StatementKind::Describe(Box::new(Describe { short, target, with_internals, with_passwords })))
    }

    /// ScyllaDB's `DESCRIBE` alternatives: a keyword form when the keyword is followed by what
    /// the form needs, the generic `[ks.]name` otherwise.
    fn scylla_describe_alt(&mut self) -> PResult<u32> {
        let a = self.la(1);
        let b = self.la(2);
        // The generic form `[ks.]name` applies when the keyword stands alone or is qualified.
        let generic = matches!(b, t::EOF | t::SEMI | t::K_WITH | t::DOT);
        let name_follows = t::is_ident(b, self.d) || b == t::QMARK;
        Ok(match a {
            t::K_CLUSTER => 1,
            t::K_KEYSPACES => 3,
            t::K_TABLES => 5,
            t::K_TYPES => 9,
            t::K_FUNCTIONS => 11,
            t::K_AGGREGATES => 13,
            t::K_FULL => 2,
            t::K_SCHEMA => 2,
            t::K_ONLY if b == t::K_KEYSPACE => 4,
            t::K_KEYSPACE => 4,
            t::K_COLUMNFAMILY => 6,
            t::K_INDEX => 7,
            t::K_MATERIALIZED => 8,
            t::K_TYPE | t::K_FUNCTION | t::K_AGGREGATE if name_follows => match a {
                t::K_TYPE => 10,
                t::K_FUNCTION => 12,
                _ => 14,
            },
            t::K_TYPE | t::K_FUNCTION | t::K_AGGREGATE | t::K_ONLY if !generic => return self.no_viable(0),
            _ if t::is_ident(a, self.d) => 15,
            _ => return self.no_viable(0),
        })
    }
}

/// Whether `ty` can start a `comparatorType`.
pub(crate) fn is_type_start(ty: Tt, d: Dialect) -> bool {
    t::is_ident(ty, d) || (ty == t::STRING_LITERAL && d == Dialect::Cassandra)
}

/// `QualifiedName.toString()`: `ks.name` with case-folded parts.
pub(crate) fn qualified_display(q: &QualifiedName<'_>) -> String {
    match &q.keyspace {
        Some(ks) => format!("{}.{}", ks.name(), q.name.name()),
        None => q.name.name().into_owned(),
    }
}
