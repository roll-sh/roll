//! Statement dispatch, `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `BATCH`, `USE`, `TRUNCATE`,
//! and ScyllaDB's `PRUNE MATERIALIZED VIEW`. Ports `cqlStatement` through `batchStatement`,
//! the selector rules, `whereClause` / `relation` and `columnOperation` / `columnCondition` of
//! Cassandra's `Parser.g`.

use super::dfa_tables as dfa;
use super::{F_END, F_IDENT, F_TERM, PResult, Parser, is_term_start};
use crate::ast::*;
use crate::tokens::{self as t, Tt};

/// What can follow a selector without modifiers in ScyllaDB's selector decision.
const SCYLLA_SELECTOR_FOLLOW: &[Tt] = &[t::DOT, t::LBRACKET, t::COMMA, t::K_FROM, t::K_AS, t::LPAREN, t::RPAREN];

/// `selectorModifier`'s exit set in Cassandra's generated parser.
const SELECTOR_MODIFIER_EXIT: &[Tt] = &[
    t::EOF,
    t::K_ALLOW,
    t::K_AS,
    t::K_FROM,
    t::K_LIMIT,
    t::K_ORDER,
    t::K_PER,
    t::PERCENT,
    t::RPAREN,
    t::PLUS,
    t::COMMA,
    t::MINUS,
    t::SLASH,
    t::COLON,
    t::STAR,
    t::RBRACKET,
    t::RBRACE,
];

fn is_relation_type(ty: Tt) -> bool {
    matches!(ty, t::EQ | t::LT | t::LTE | t::GT | t::GTE | t::NEQ)
}

impl<'a> Parser<'a> {
    /// `cqlStatement`.
    pub(crate) fn cql_statement(&mut self) -> PResult<StatementKind<'a>> {
        if self.scylla() {
            return self.scylla_statement();
        }
        let alt = self.predict(&dfa::DFA1)?;
        Ok(match alt {
            1 => StatementKind::Select(Box::new(self.select_statement()?)),
            2 => StatementKind::Insert(Box::new(self.insert_statement()?)),
            3 => StatementKind::Update(Box::new(self.update_statement()?)),
            4 => StatementKind::Batch(Box::new(self.batch_statement()?)),
            5 => StatementKind::Delete(Box::new(self.delete_statement()?)),
            6 => self.use_statement()?,
            7 => self.truncate_statement()?,
            8 => self.create_keyspace()?,
            9 => self.create_table()?,
            10 => self.create_index()?,
            11 => self.drop_keyspace()?,
            12 => self.drop_table()?,
            13 => self.drop_index()?,
            14 => self.alter_table()?,
            15 => self.alter_keyspace()?,
            16 => self.grant_permissions()?,
            17 => self.revoke_permissions()?,
            18 => self.list_permissions()?,
            19 => self.create_user()?,
            20 => self.alter_user()?,
            21 => self.drop_user()?,
            22 => self.list_users()?,
            23 => self.create_trigger()?,
            24 => self.drop_trigger()?,
            25 => self.create_type()?,
            26 => self.alter_type()?,
            27 => self.drop_type()?,
            28 => self.create_function()?,
            29 => self.drop_function()?,
            30 => self.create_aggregate()?,
            31 => self.drop_aggregate()?,
            32 => self.create_role()?,
            33 => self.alter_role()?,
            34 => self.drop_role()?,
            35 => self.list_roles()?,
            36 => self.grant_role()?,
            37 => self.revoke_role()?,
            38 => self.create_view()?,
            39 => self.drop_view()?,
            40 => self.alter_view()?,
            41 => self.describe()?,
            42 => self.add_identity()?,
            _ => self.drop_identity()?,
        })
    }

    /// ScyllaDB's statement dispatch. Its parser reports a failed dispatch at the first token.
    fn scylla_statement(&mut self) -> PResult<StatementKind<'a>> {
        let a = self.la(1);
        Ok(match a {
            t::K_SELECT => StatementKind::Select(Box::new(self.select_statement()?)),
            t::K_INSERT => StatementKind::Insert(Box::new(self.insert_statement()?)),
            t::K_UPDATE => StatementKind::Update(Box::new(self.update_statement()?)),
            t::K_BEGIN => StatementKind::Batch(Box::new(self.batch_statement()?)),
            t::K_DELETE => StatementKind::Delete(Box::new(self.delete_statement()?)),
            t::K_USE => self.use_statement()?,
            t::K_TRUNCATE => self.truncate_statement()?,
            t::K_DESCRIBE | t::K_DESC => self.describe()?,
            t::K_PRUNE => self.prune_view()?,
            t::K_ATTACH => self.attach_service_level()?,
            t::K_DETACH => self.detach_service_level()?,
            t::K_CREATE => {
                let b = self.la(2);
                if b == t::K_OR && self.la(3) != t::K_REPLACE {
                    return self.no_viable(0);
                }
                match b {
                    t::K_KEYSPACE => self.create_keyspace()?,
                    t::K_COLUMNFAMILY => self.create_table()?,
                    t::K_INDEX | t::K_CUSTOM => self.create_index()?,
                    t::K_TYPE => self.create_type()?,
                    t::K_FUNCTION => self.create_function()?,
                    t::K_AGGREGATE => self.create_aggregate()?,
                    t::K_OR => match self.la(4) {
                        t::K_FUNCTION => self.create_function()?,
                        t::K_AGGREGATE => self.create_aggregate()?,
                        _ => return self.no_viable(0),
                    },
                    t::K_ROLE => self.create_role()?,
                    t::K_USER => self.create_user()?,
                    t::K_MATERIALIZED => self.create_view()?,
                    t::K_SERVICE | t::K_SERVICE_LEVEL => self.create_service_level()?,
                    _ => return self.no_viable(0),
                }
            }
            t::K_DROP => match self.la(2) {
                t::K_KEYSPACE => self.drop_keyspace()?,
                t::K_COLUMNFAMILY => self.drop_table()?,
                t::K_INDEX => self.drop_index()?,
                t::K_TYPE => self.drop_type()?,
                t::K_FUNCTION => self.drop_function()?,
                t::K_AGGREGATE => self.drop_aggregate()?,
                t::K_ROLE => self.drop_role()?,
                t::K_USER => self.drop_user()?,
                t::K_MATERIALIZED => self.drop_view()?,
                t::K_SERVICE | t::K_SERVICE_LEVEL => self.drop_service_level()?,
                _ => return self.no_viable(0),
            },
            t::K_ALTER => match self.la(2) {
                t::K_KEYSPACE => self.alter_keyspace()?,
                t::K_COLUMNFAMILY => self.alter_table()?,
                t::K_TYPE => self.alter_type()?,
                t::K_ROLE => self.alter_role()?,
                t::K_USER => self.alter_user()?,
                t::K_MATERIALIZED => self.alter_view()?,
                t::K_SERVICE | t::K_SERVICE_LEVEL => self.alter_service_level()?,
                _ => return self.no_viable(0),
            },
            t::K_GRANT | t::K_REVOKE => {
                let grant = a == t::K_GRANT;
                match self.scylla_permission_or_role(grant)? {
                    true if grant => self.grant_permissions()?,
                    true => self.revoke_permissions()?,
                    false if grant => self.grant_role()?,
                    false => self.revoke_role()?,
                }
            }
            t::K_LIST => {
                let b = self.la(2);
                let c = self.la(3);
                let perm_follow =
                    |ty: Tt| matches!(ty, t::K_ON | t::K_OF | t::K_NORECURSIVE | t::K_PERMISSION | t::K_PERMISSIONS | t::EOF | t::SEMI);
                match b {
                    t::K_ROLES => self.list_roles()?,
                    t::K_USERS => self.list_users()?,
                    t::K_SERVICE | t::K_SERVICE_LEVEL | t::K_ATTACHED | t::K_EFFECTIVE => self.list_service_levels()?,
                    t::K_ALL if matches!(c, t::K_SERVICE | t::K_SERVICE_LEVELS | t::K_ATTACHED) => self.list_service_levels()?,
                    t::K_ALL if perm_follow(c) => self.list_permissions()?,
                    t::K_CREATE | t::K_ALTER | t::K_DROP | t::K_SELECT | t::K_MODIFY | t::K_AUTHORIZE => {
                        self.list_permissions()?
                    }
                    t::K_DESCRIBE | t::K_EXECUTE if perm_follow(c) => self.list_permissions()?,
                    _ => return self.no_viable(0),
                }
            }
            _ => return self.no_viable(0),
        })
    }

    /// ScyllaDB: `GRANT perm ON ...` (true) versus `GRANT role TO ...` (false). Reserved
    /// permission names decide at once; `ALL`, `DESCRIBE` and `EXECUTE` can also be role names.
    fn scylla_permission_or_role(&mut self, grant: bool) -> PResult<bool> {
        let b = self.la(2);
        let to = if grant { t::K_TO } else { t::K_FROM };
        match b {
            t::K_CREATE | t::K_ALTER | t::K_DROP | t::K_SELECT | t::K_MODIFY | t::K_AUTHORIZE => Ok(true),
            t::K_ALL | t::K_DESCRIBE | t::K_EXECUTE => match self.la(3) {
                t::K_ON | t::K_PERMISSION | t::K_PERMISSIONS => Ok(true),
                c if c == to => Ok(false),
                _ => self.no_viable(0),
            },
            t::STRING_LITERAL | t::QUOTED_NAME | t::QMARK => Ok(false),
            _ if t::is_ident(b, self.d) => Ok(false),
            _ => self.no_viable(0),
        }
    }

    // -----------------------------------------------------------------------------------------
    // SELECT
    // -----------------------------------------------------------------------------------------

    /// `selectStatement`.
    pub(crate) fn select_statement(&mut self) -> PResult<Select<'a>> {
        self.expect(t::K_SELECT)?;
        let json = if self.cassandra() {
            self.predict(&dfa::DFA2)? == 1
        } else {
            self.at(t::K_JSON)
                && (self.la(2) == t::K_DISTINCT
                    || self.speculate(|p| {
                        p.consume();
                        p.select_clause()?;
                        p.expect(t::K_FROM)
                    }))
        };
        if json {
            self.expect(t::K_JSON)?;
        }
        let (distinct, selectors) = self.select_clause()?;
        self.expect_f(t::K_FROM, &[F_IDENT, t::QMARK])?;
        // ScyllaDB's `MUTATION_FRAGMENTS(cf)` (an unreserved word: a table may be named so).
        let mutation_fragments = self.scylla() && t::is_ident(self.la(1), self.d) && self.la(2) == t::LPAREN && {
            let tok = self.lt(1);
            self.text(&tok).eq_ignore_ascii_case("mutation_fragments")
        };
        if mutation_fragments {
            self.consume();
            self.expect(t::LPAREN)?;
        }
        let table = self.column_family_name()?;
        if mutation_fragments {
            self.expect(t::RPAREN)?;
        }
        let mut sel = Select {
            json,
            distinct,
            selectors,
            table,
            where_clause: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            per_partition_limit: None,
            limit: None,
            allow_filtering: false,
            bypass_cache: false,
            using: UsingClause::default(),
            mutation_fragments,
        };
        if self.eat(t::K_WHERE) {
            sel.where_clause = self.where_clause()?;
        }
        if self.at(t::K_GROUP) {
            self.consume();
            self.expect_f(t::K_BY, &[F_IDENT])?;
            sel.group_by.push(self.group_by_clause()?);
            while self.eat(t::COMMA) {
                sel.group_by.push(self.group_by_clause()?);
            }
        }
        if self.at(t::K_ORDER) {
            self.consume();
            self.expect_f(t::K_BY, &[F_IDENT])?;
            sel.order_by.push(self.order_by_clause()?);
            while self.eat(t::COMMA) {
                sel.order_by.push(self.order_by_clause()?);
            }
        }
        if self.at(t::K_PER) {
            self.consume();
            self.expect_f(t::K_PARTITION, &[t::K_LIMIT])?;
            self.expect_f(t::K_LIMIT, &[t::INTEGER, t::COLON, t::QMARK])?;
            sel.per_partition_limit = Some(self.int_value()?);
        }
        if self.eat(t::K_LIMIT) {
            sel.limit = Some(self.int_value()?);
        }
        if self.at(t::K_ALLOW) {
            self.consume();
            self.expect_f(t::K_FILTERING, &[F_END, t::K_BYPASS, t::K_USING])?;
            sel.allow_filtering = true;
        }
        if self.scylla() {
            if self.at(t::K_BYPASS) {
                self.consume();
                self.expect_f(t::K_CACHE, &[F_END, t::K_USING])?;
                sel.bypass_cache = true;
            }
            if self.at(t::K_USING) {
                sel.using = self.using_clause(&[t::K_TIMEOUT, t::K_SERVICE, t::K_SERVICE_LEVEL])?;
            }
        }
        Ok(sel)
    }

    /// `selectClause`: `[DISTINCT] selectors`.
    pub(crate) fn select_clause(&mut self) -> PResult<(bool, Vec<Selector<'a>>)> {
        let distinct = if self.cassandra() {
            self.predict(&dfa::DFA11)? == 1
        } else {
            self.at(t::K_DISTINCT)
                && self.speculate(|p| {
                    p.consume();
                    p.selectors()
                })
        };
        if distinct {
            self.expect(t::K_DISTINCT)?;
        }
        Ok((distinct, self.selectors()?))
    }

    /// `selectors`: `selector (',' selector)* | '*'`. `*` is an empty list.
    pub(crate) fn selectors(&mut self) -> PResult<Vec<Selector<'a>>> {
        let ty = self.la(1);
        if ty == t::STAR {
            self.consume();
            return Ok(Vec::new());
        }
        if !(is_term_start(ty, self.d) || ty == t::MINUS || (ty == t::K_CAST && self.scylla())) {
            return self.no_viable(0);
        }
        let mut out = vec![self.selector()?];
        while self.eat(t::COMMA) {
            out.push(self.selector()?);
        }
        Ok(out)
    }

    fn selector(&mut self) -> PResult<Selector<'a>> {
        let expr = self.unaliased_selector()?;
        let alias = if self.eat(t::K_AS) { Some(self.ident()?) } else { None };
        Ok(Selector { expr, alias })
    }

    /// `unaliasedSelector` / `selectionAddition`.
    pub(crate) fn unaliased_selector(&mut self) -> PResult<Selectable<'a>> {
        self.nested(|p| p.unaliased_selector_unguarded())
    }

    fn unaliased_selector_unguarded(&mut self) -> PResult<Selectable<'a>> {
        if self.scylla() {
            return self.scylla_selector();
        }
        let mut left = self.selection_multiplication()?;
        loop {
            let op = match self.la(1) {
                t::PLUS => ArithOp::Add,
                t::MINUS => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.consume();
            let right = self.selection_multiplication()?;
            left = Selectable::Binary(Box::new(BinaryOp { op, left, right }));
        }
    }

    fn selection_multiplication(&mut self) -> PResult<Selectable<'a>> {
        let mut left = self.selection_group()?;
        loop {
            let op = match self.la(1) {
                t::STAR => ArithOp::Mul,
                t::SLASH => ArithOp::Div,
                t::PERCENT => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.consume();
            let right = self.selection_group()?;
            left = Selectable::Binary(Box::new(BinaryOp { op, left, right }));
        }
    }

    fn selection_group(&mut self) -> PResult<Selectable<'a>> {
        self.nested(|p| p.selection_group_unguarded())
    }

    fn selection_group_unguarded(&mut self) -> PResult<Selectable<'a>> {
        match self.predict(&dfa::DFA17)? {
            1 => self.selection_group_with_field(),
            2 => self.selection_group_without_field(),
            _ => {
                self.expect(t::MINUS)?;
                let inner = self.selection_group()?;
                Ok(Selectable::Negate(Box::new(inner)))
            }
        }
    }

    pub(crate) fn selection_group_with_field(&mut self) -> PResult<Selectable<'a>> {
        let g = self.selection_group_without_field()?;
        self.selector_modifier(g)
    }

    /// `selectorModifier`: `.field` and `[element]` / `[from..to]` selections.
    fn selector_modifier(&mut self, mut receiver: Selectable<'a>) -> PResult<Selectable<'a>> {
        loop {
            let ty = self.la(1);
            match ty {
                t::DOT => {
                    self.consume();
                    let f = self.ident()?;
                    receiver = Selectable::Field(Box::new(receiver), f);
                }
                t::LBRACKET => {
                    self.consume();
                    receiver = self.collection_sub_selection(receiver)?;
                    self.expect(t::RBRACKET)?;
                }
                _ if SELECTOR_MODIFIER_EXIT.contains(&ty) => return Ok(receiver),
                _ => return self.no_viable(0),
            }
        }
    }

    fn collection_sub_selection(&mut self, receiver: Selectable<'a>) -> PResult<Selectable<'a>> {
        let ty = self.la(1);
        if is_term_start(ty, self.d) || ty == t::MINUS {
            let from = self.term()?;
            if self.eat(t::RANGE) {
                let ty = self.la(1);
                let to = if is_term_start(ty, self.d) || ty == t::MINUS { Some(self.term()?) } else { None };
                return Ok(Selectable::Slice(Box::new(SliceSelection { receiver, from: Some(from), to })));
            }
            return Ok(Selectable::Element(Box::new(receiver), from));
        }
        if ty == t::RANGE {
            self.consume();
            let to = self.term()?;
            return Ok(Selectable::Slice(Box::new(SliceSelection { receiver, from: None, to: Some(to) })));
        }
        self.no_viable(0)
    }

    fn selection_group_without_field(&mut self) -> PResult<Selectable<'a>> {
        match self.predict(&dfa::DFA22)? {
            1 => self.simple_unaliased_selector(),
            2 => self.selection_type_hint(),
            3 => {
                self.expect(t::LPAREN)?;
                let mut items = vec![self.unaliased_selector()?];
                while self.eat(t::COMMA) {
                    items.push(self.unaliased_selector()?);
                }
                self.expect(t::RPAREN)?;
                Ok(Selectable::Tuple(items))
            }
            4 => {
                self.expect(t::LBRACKET)?;
                let mut items = Vec::new();
                let ty = self.la(1);
                if is_term_start(ty, self.d) || ty == t::MINUS {
                    items.push(self.unaliased_selector()?);
                    while self.eat(t::COMMA) {
                        items.push(self.unaliased_selector()?);
                    }
                }
                self.expect(t::RBRACKET)?;
                Ok(Selectable::List(items))
            }
            _ => self.selection_map_or_set(),
        }
    }

    pub(crate) fn selection_type_hint(&mut self) -> PResult<Selectable<'a>> {
        self.expect(t::LPAREN)?;
        let ty = self.comparator_type()?;
        self.expect(t::RPAREN)?;
        let arg = self.selection_group_without_field()?;
        Ok(Selectable::TypeHint(ty, Box::new(arg)))
    }

    fn selection_map_or_set(&mut self) -> PResult<Selectable<'a>> {
        if self.la(1) != t::LBRACE {
            return self.no_viable(0);
        }
        let next = self.la(2);
        if next == t::RBRACE {
            self.consume();
            self.consume();
            return Ok(Selectable::Set(Vec::new()));
        }
        if !(is_term_start(next, self.d) || next == t::MINUS) {
            return self.no_viable(1);
        }
        self.consume();
        let first = self.unaliased_selector()?;
        let out = match self.la(1) {
            t::COLON => {
                self.consume();
                let v = self.unaliased_selector()?;
                let mut entries = vec![(first, v)];
                while self.eat(t::COMMA) {
                    let k = self.unaliased_selector()?;
                    self.expect(t::COLON)?;
                    let v = self.unaliased_selector()?;
                    entries.push((k, v));
                }
                Selectable::MapOrUdt(entries)
            }
            t::COMMA | t::RBRACE => {
                let mut items = vec![first];
                while self.eat(t::COMMA) {
                    items.push(self.unaliased_selector()?);
                }
                Selectable::Set(items)
            }
            _ => return self.no_viable(0),
        };
        self.expect(t::RBRACE)?;
        Ok(out)
    }

    /// `simpleUnaliasedSelector: sident | selectionLiteral | selectionFunction`.
    fn simple_unaliased_selector(&mut self) -> PResult<Selectable<'a>> {
        match self.predict(&dfa::DFA30)? {
            1 => Ok(Selectable::Column(self.ident()?)),
            2 => Ok(Selectable::Term(self.selection_literal()?)),
            _ => self.selection_function(),
        }
    }

    /// `selectionLiteral: constant | NULL | ':' noncol_ident | QMARK`.
    fn selection_literal(&mut self) -> PResult<Term<'a>> {
        match self.la(1) {
            t::K_NULL => {
                self.consume();
                Ok(Term::Null)
            }
            t::COLON | t::QMARK => Ok(Term::Bind(self.bind_marker()?)),
            _ => Ok(Term::Literal(self.constant()?)),
        }
    }

    fn selection_function(&mut self) -> PResult<Selectable<'a>> {
        let alt = self.predict(&dfa::DFA31)?;
        match alt {
            1 => {
                self.expect(t::K_COUNT)?;
                self.expect(t::LPAREN)?;
                self.expect(t::STAR)?;
                self.expect(t::RPAREN)?;
                Ok(Selectable::CountRows)
            }
            2..=4 => {
                let kind = match alt {
                    2 => MetadataKind::MaxWriteTime,
                    3 => MetadataKind::WriteTime,
                    _ => MetadataKind::Ttl,
                };
                self.consume();
                self.expect(t::LPAREN)?;
                let c = Selectable::Column(self.ident()?);
                let arg = self.selector_modifier(c)?;
                self.expect(t::RPAREN)?;
                Ok(Selectable::Metadata(kind, Box::new(arg)))
            }
            5 => {
                self.expect(t::K_CAST)?;
                self.expect(t::LPAREN)?;
                let arg = self.unaliased_selector()?;
                self.expect(t::K_AS)?;
                let ty = self.native_type()?;
                self.expect(t::RPAREN)?;
                Ok(Selectable::Cast(Box::new(Cast::new(arg, ty))))
            }
            _ => {
                let name = self.function_name()?;
                let args = self.selection_function_args()?;
                Ok(Selectable::Function(name, args))
            }
        }
    }

    /// `selectionFunctionArgs: '(' (unaliasedSelector (',' unaliasedSelector)*)? ')'`.
    pub(crate) fn selection_function_args(&mut self) -> PResult<Vec<Selectable<'a>>> {
        self.expect(t::LPAREN)?;
        let mut args = Vec::new();
        let ty = self.la(1);
        if is_term_start(ty, self.d) || (ty == t::MINUS && self.cassandra()) {
            args.push(self.unaliased_selector()?);
            while self.eat(t::COMMA) {
                args.push(self.unaliased_selector()?);
            }
        }
        self.expect_f(t::RPAREN, &[t::COMMA, t::K_FROM, t::K_AS, F_END])?;
        Ok(args)
    }

    /// ScyllaDB's selector: no arithmetic, slices or type hints; collection and tuple literals of
    /// selectors; field and element selection.
    fn scylla_selector(&mut self) -> PResult<Selectable<'a>> {
        self.nested(|p| p.scylla_selector_unguarded())
    }

    fn scylla_selector_unguarded(&mut self) -> PResult<Selectable<'a>> {
        let ty = self.la(1);
        let base = match ty {
            t::MINUS => Selectable::Term(self.scylla_minus()?),
            t::K_COUNT
                if self.la(2) == t::LPAREN
                    && !(matches!(self.la(3), t::STAR | t::RPAREN | t::MINUS) || super::is_term_start(self.la(3), self.d)) =>
            {
                return self.no_viable(0);
            }
            t::K_COUNT if self.la(2) == t::LPAREN && self.la(3) == t::STAR => {
                self.consume();
                self.consume();
                self.consume();
                self.expect_f(t::RPAREN, &[F_END])?;
                Selectable::CountRows
            }
            t::K_WRITETIME | t::K_TTL if self.la(2) == t::LPAREN => {
                let kind = if ty == t::K_TTL { MetadataKind::Ttl } else { MetadataKind::WriteTime };
                self.consume();
                self.consume();
                let mut arg = Selectable::Column(self.ident()?);
                loop {
                    if self.eat(t::DOT) {
                        arg = Selectable::Field(Box::new(arg), self.ident()?);
                    } else if self.eat(t::LBRACKET) {
                        let k = self.term()?;
                        self.expect_f(t::RBRACKET, &[F_END])?;
                        arg = Selectable::Element(Box::new(arg), k);
                    } else {
                        break;
                    }
                }
                self.expect_f(t::RPAREN, &[F_END])?;
                Selectable::Metadata(kind, Box::new(arg))
            }
            t::K_CAST => {
                self.consume();
                self.expect_f(t::LPAREN, &[F_IDENT, F_TERM])?;
                let arg = self.scylla_selector()?;
                self.expect_f(t::K_AS, &[F_IDENT])?;
                let ty = self.native_type()?;
                self.expect_f(t::RPAREN, &[F_END])?;
                Selectable::Cast(Box::new(Cast::new(arg, ty)))
            }
            // Collection and tuple selectors are literal terms in ScyllaDB.
            t::LBRACKET | t::LBRACE => Selectable::Term(self.value()?),
            t::LPAREN => {
                self.consume();
                let mut items = vec![self.term()?];
                while self.eat(t::COMMA) {
                    items.push(self.term()?);
                }
                self.expect_f(t::RPAREN, &[F_END])?;
                Selectable::Term(Term::Tuple(items))
            }
            t::QMARK => {
                // A `?` selector is decided with the token after it, like a column.
                let next = self.la(2);
                if next == t::LPAREN || !SCYLLA_SELECTOR_FOLLOW.contains(&next) {
                    return self.no_viable(0);
                }
                Selectable::Term(self.selection_literal()?)
            }
            t::COLON => {
                if !t::is_ident(self.la(2), self.d) {
                    return self.no_viable_deep(1);
                }
                Selectable::Term(self.selection_literal()?)
            }
            t::K_NULL => Selectable::Term(self.selection_literal()?),
            _ if super::is_constant(ty) => Selectable::Term(Term::Literal(self.constant()?)),
            t::K_TOKEN if self.la(2) != t::LPAREN => return self.no_viable_deep(1),
            t::K_KEY if self.la(2) == t::LPAREN => return self.no_viable(0),
            _ if t::is_ident(ty, self.d) || ty == t::K_TOKEN || ty == t::K_COUNT => {
                let next = self.la(2);
                if next == t::DOT {
                    let third = self.la(3);
                    // `ks.token(…)` / `ks.count(…)` are (user) function calls.
                    let keyword_fn = matches!(third, t::K_TOKEN | t::K_COUNT) && self.la(4) == t::LPAREN;
                    if !t::is_ident(third, self.d) && !keyword_fn {
                        return self.no_viable(0);
                    }
                    let fourth = self.la(4);
                    if fourth != t::LPAREN && !SCYLLA_SELECTOR_FOLLOW.contains(&fourth) {
                        return self.no_viable(0);
                    }
                }
                if next == t::LPAREN || (next == t::DOT && self.la(4) == t::LPAREN) {
                    let name = self.function_name()?;
                    let args = self.scylla_function_args()?;
                    Selectable::Function(name, args)
                } else if (ty == t::K_TOKEN) || !SCYLLA_SELECTOR_FOLLOW.contains(&next) {
                    return self.no_viable(0);
                } else {
                    Selectable::Column(self.ident()?)
                }
            }
            _ => return self.no_viable(0),
        };
        let mut out = base;
        loop {
            match self.la(1) {
                t::DOT => {
                    self.consume();
                    out = Selectable::Field(Box::new(out), self.ident()?);
                }
                t::LBRACKET => {
                    self.consume();
                    let k = self.term()?;
                    self.expect_f(t::RBRACKET, &[F_END])?;
                    out = Selectable::Element(Box::new(out), k);
                }
                _ => return Ok(out),
            }
        }
    }

    fn scylla_function_args(&mut self) -> PResult<Vec<Selectable<'a>>> {
        if self.la(1) != t::LPAREN {
            return self.no_viable(0);
        }
        let next = self.la(2);
        if next == t::RPAREN {
            self.consume();
            self.consume();
            return Ok(Vec::new());
        }
        if !(is_term_start(next, self.d) || next == t::MINUS) {
            return self.no_viable(0);
        }
        self.consume();
        let mut args = vec![self.scylla_selector()?];
        while self.eat(t::COMMA) {
            args.push(self.scylla_selector()?);
        }
        self.expect_f(t::RPAREN, &[t::COMMA, t::K_FROM, t::K_AS, F_END])?;
        Ok(args)
    }

    /// ScyllaDB's report of a token that cannot follow a lone `-`.
    pub(crate) fn scylla_unexpected(&mut self, tok: &crate::lexer::Token) -> super::Fail {
        let (hdr, pos) = self.header(tok);
        let msg = format!("{hdr} : Unexpected {}", self.display(tok));
        self.record(crate::SyntaxError { code: crate::ErrorCode::Syntax, message: msg, position: Some(pos) })
    }

    /// `groupByClause`: a selector (Cassandra) or a column (ScyllaDB).
    fn group_by_clause(&mut self) -> PResult<Selectable<'a>> {
        if self.scylla() {
            return Ok(Selectable::Column(self.cident()?));
        }
        self.unaliased_selector()
    }

    /// `orderByClause: cident (ANN OF term)? (ASC | DESC)?`.
    fn order_by_clause(&mut self) -> PResult<OrderBy<'a>> {
        let function = self.scylla()
            && t::is_ident(self.la(1), self.d)
            && (self.la(2) == t::LPAREN
                || (self.la(2) == t::DOT && t::is_ident(self.la(3), self.d) && self.la(4) == t::LPAREN));
        let expr = if function {
            let name = self.function_name()?;
            let args = self.scylla_function_args()?;
            // A scoring function ordering takes no direction.
            return Ok(OrderBy { expr: OrderByExpr::Function(name, args), direction: None });
        } else {
            if self.scylla() {
                let next = self.la(2);
                let ok = matches!(
                    next,
                    t::K_ASC | t::K_DESC | t::COMMA | t::K_LIMIT | t::K_ANN | t::K_ALLOW | t::K_BYPASS | t::K_USING | t::K_PER
                        | t::EOF | t::SEMI
                );
                if !ok {
                    return self.no_viable(0);
                }
            }
            let c = self.cident()?;
            if self.at(t::K_ANN) {
                self.consume();
                self.expect_f(t::K_OF, &[F_TERM])?;
                let v = self.term()?;
                OrderByExpr::Ann(c, v)
            } else {
                OrderByExpr::Column(c)
            }
        };
        let direction = match self.la(1) {
            t::K_ASC => {
                self.consume();
                Some(Direction::Asc)
            }
            t::K_DESC => {
                self.consume();
                Some(Direction::Desc)
            }
            _ => None,
        };
        Ok(OrderBy { expr, direction })
    }

    // -----------------------------------------------------------------------------------------
    // WHERE
    // -----------------------------------------------------------------------------------------

    /// `whereClause: relationOrExpression (AND relationOrExpression)*`.
    pub(crate) fn where_clause(&mut self) -> PResult<Vec<Relation<'a>>> {
        let mut out = Vec::new();
        self.relation_or_expression(&mut out)?;
        while self.eat(t::K_AND) {
            self.relation_or_expression(&mut out)?;
        }
        Ok(out)
    }

    fn relation_or_expression(&mut self, out: &mut Vec<Relation<'a>>) -> PResult<()> {
        let ty = self.la(1);
        if ty == t::EXPR_LPAREN {
            self.consume();
            let name = self.object_name_pub("index")?;
            self.expect(t::COMMA)?;
            let value = self.term()?;
            self.expect(t::RPAREN)?;
            out.push(Relation::Expression(CustomExpression { index: QualifiedName { keyspace: None, name }, value }));
            return Ok(());
        }
        if self.scylla() {
            return self.scylla_relation(out);
        }
        if ty == t::EMPTY_QUOTED_NAME || ty == t::LPAREN || ty == t::K_TOKEN || t::is_ident(ty, self.d) {
            return self.relation(out);
        }
        self.no_viable(0)
    }

    /// `relation`.
    fn relation(&mut self, out: &mut Vec<Relation<'a>>) -> PResult<()> {
        self.nested(|p| p.relation_unguarded(out))
    }

    fn relation_unguarded(&mut self, out: &mut Vec<Relation<'a>>) -> PResult<()> {
        let alt = self.predict(&dfa::DFA252)?;
        match alt {
            1 => {
                let column = self.cident()?;
                let op = self.relation_type()?;
                let v = self.term()?;
                out.push(single(column, None, op, Value::Term(v)));
            }
            2 => {
                let column = self.cident()?;
                self.expect(t::K_LIKE)?;
                let v = self.term()?;
                out.push(single(column, None, Operator::Like, Value::Term(v)));
            }
            3 => {
                let column = self.cident()?;
                self.expect(t::K_IS)?;
                self.expect(t::K_NOT)?;
                self.expect(t::K_NULL)?;
                out.push(single(column, None, Operator::IsNot, Value::Term(Term::Null)));
            }
            4 => {
                self.expect(t::K_TOKEN)?;
                let columns = self.tuple_of_identifiers()?;
                let op = self.relation_type()?;
                let value = self.term()?;
                out.push(Relation::Token(TokenRelation { columns, op, value }));
            }
            5 => {
                let column = self.cident()?;
                self.expect(t::K_IN)?;
                let m = self.bind_marker()?;
                out.push(single(column, None, Operator::In, Value::ListMarker(m)));
            }
            6 => {
                let column = self.cident()?;
                self.expect(t::K_IN)?;
                let values = self.single_column_in_values()?;
                out.push(single(column, None, Operator::In, Value::List(values)));
            }
            7 => {
                let column = self.cident()?;
                let op = self.contains_operator()?;
                let v = self.term()?;
                out.push(single(column, None, op, Value::Term(v)));
            }
            8 => {
                let column = self.cident()?;
                self.expect(t::LBRACKET)?;
                let key = self.term()?;
                self.expect(t::RBRACKET)?;
                let op = self.relation_type()?;
                let v = self.term()?;
                out.push(single(column, Some(key), op, Value::Term(v)));
            }
            9 => {
                let columns = self.tuple_of_identifiers()?;
                let r = self.multi_column_rest(columns)?;
                out.push(r);
            }
            _ => {
                self.expect(t::LPAREN)?;
                self.relation(out)?;
                self.expect(t::RPAREN)?;
            }
        }
        Ok(())
    }

    /// After `(a, b)`: `IN (...)`, `IN ?`, `op (tuple)`, `op ?`.
    fn multi_column_rest(&mut self, columns: Vec<Ident<'a>>) -> PResult<Relation<'a>> {
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA251)?
        } else {
            match self.la(1) {
                t::K_IN => 1,
                ty if is_relation_type(ty) => match self.la(2) {
                    t::LPAREN => 2,
                    t::QMARK | t::COLON => 3,
                    _ => return self.no_viable(0),
                },
                _ => return self.no_viable(0),
            }
        };
        match alt {
            1 => {
                self.expect(t::K_IN)?;
                let value = match self.la(1) {
                    t::LPAREN => match self.la(2) {
                        t::RPAREN => {
                            self.consume();
                            self.consume();
                            Value::List(Vec::new())
                        }
                        t::LPAREN => {
                            self.consume();
                            let mut items = vec![self.tuple_literal()?];
                            while self.eat(t::COMMA) {
                                items.push(self.tuple_literal()?);
                            }
                            self.expect_f(t::RPAREN, &[F_END])?;
                            Value::List(items)
                        }
                        t::QMARK | t::COLON => {
                            self.consume();
                            let mut items = vec![Term::Bind(self.bind_marker()?)];
                            while self.eat(t::COMMA) {
                                items.push(Term::Bind(self.bind_marker()?));
                            }
                            self.expect_f(t::RPAREN, &[F_END])?;
                            Value::List(items)
                        }
                        _ => return self.no_viable(1),
                    },
                    t::QMARK | t::COLON => Value::ListMarker(self.bind_marker()?),
                    _ => return self.no_viable(0),
                };
                Ok(Relation::MultiColumn(MultiColumnRelation { columns, op: Operator::In, value }))
            }
            2 => {
                let op = self.relation_type()?;
                let v = self.tuple_literal()?;
                Ok(Relation::MultiColumn(MultiColumnRelation { columns, op, value: Value::Term(v) }))
            }
            _ => {
                let op = self.relation_type()?;
                let m = self.bind_marker()?;
                Ok(Relation::MultiColumn(MultiColumnRelation { columns, op, value: Value::Term(Term::Bind(m)) }))
            }
        }
    }

    /// ScyllaDB's relation: the operator is a decision of its own (reported at the operator),
    /// with `NOT IN`, function calls on the left, and parenthesised relations.
    fn scylla_relation(&mut self, out: &mut Vec<Relation<'a>>) -> PResult<()> {
        self.nested(|p| p.scylla_relation_unguarded(out))
    }

    fn scylla_relation_unguarded(&mut self, out: &mut Vec<Relation<'a>>) -> PResult<()> {
        let ty = self.la(1);
        if ty == t::K_TOKEN {
            self.consume();
            let columns = self.tuple_of_identifiers()?;
            if !is_relation_type(self.la(1)) {
                return self.no_viable(0);
            }
            let op = self.relation_type()?;
            let value = self.term()?;
            out.push(Relation::Token(TokenRelation { columns, op, value }));
            return Ok(());
        }
        if ty == t::LPAREN {
            let second = self.la(2);
            let third = self.la(3);
            if (t::is_ident(second, self.d) || second == t::EMPTY_QUOTED_NAME) && matches!(third, t::COMMA | t::RPAREN) {
                let columns = self.tuple_of_identifiers()?;
                out.push(self.multi_column_rest(columns)?);
                return Ok(());
            }
            let relation_follows = second == t::LPAREN
                || second == t::K_TOKEN
                || (t::is_ident(second, self.d)
                    && (is_relation_type(third)
                        || matches!(third, t::K_IN | t::K_NOT | t::K_CONTAINS | t::K_LIKE | t::K_IS | t::LBRACKET | t::LPAREN)));
            if !relation_follows {
                return self.no_viable(0);
            }
            self.consume();
            self.scylla_relation(out)?;
            self.expect_f(t::RPAREN, &[F_END])?;
            return Ok(());
        }
        if t::is_ident(ty, self.d) && self.la(2) == t::DOT {
            // Only a function call `ks.f(...)` can start like this.
            if !t::is_ident(self.la(3), self.d) {
                return self.no_viable_deep(2);
            }
            if self.la(4) != t::LPAREN {
                return self.no_viable_deep(3);
            }
        }
        if t::is_ident(ty, self.d) && (self.la(2) == t::LPAREN || self.la(2) == t::DOT) {
            self.relation_function = true;
            let function = self.function_name()?;
            let args = self.scylla_function_args()?;
            // `f(...) LIKE 'pattern'` parses (and is refused while preparing).
            if self.la(1) == t::K_LIKE {
                self.consume();
                let value = self.term()?;
                out.push(Relation::Function(FunctionRelation { function, args, op: Operator::Like, value }));
                return Ok(());
            }
            if !is_relation_type(self.la(1)) {
                return self.no_viable(0);
            }
            let op = self.relation_type()?;
            let value = self.term()?;
            out.push(Relation::Function(FunctionRelation { function, args, op, value }));
            return Ok(());
        }
        let column = self.cident()?;
        let op_ty = self.la(1);
        match op_ty {
            _ if is_relation_type(op_ty) => {
                let op = self.relation_type()?;
                let v = self.term()?;
                out.push(single(column, None, op, Value::Term(v)));
            }
            t::K_LIKE => {
                self.consume();
                let v = self.term()?;
                out.push(single(column, None, Operator::Like, Value::Term(v)));
            }
            t::K_IS => {
                self.consume();
                self.expect_f(t::K_NOT, &[t::K_NULL])?;
                self.expect_f(t::K_NULL, &[F_END])?;
                out.push(single(column, None, Operator::IsNot, Value::Term(Term::Null)));
            }
            t::K_IN | t::K_NOT => {
                let op = if op_ty == t::K_NOT {
                    if self.la(2) != t::K_IN || !matches!(self.la(3), t::LPAREN | t::QMARK | t::COLON) {
                        return self.no_viable(0);
                    }
                    self.consume();
                    Operator::NotIn
                } else {
                    Operator::In
                };
                let value = match self.la(2) {
                    t::LPAREN => {
                        self.consume();
                        Value::List(self.single_column_in_values()?)
                    }
                    t::QMARK | t::COLON => {
                        self.consume();
                        Value::ListMarker(self.bind_marker()?)
                    }
                    _ => return self.no_viable(0),
                };
                out.push(single(column, None, op, value));
            }
            t::K_CONTAINS => {
                let op = self.contains_operator()?;
                let v = self.term()?;
                out.push(single(column, None, op, Value::Term(v)));
            }
            t::LBRACKET => {
                self.consume();
                let key = self.term()?;
                self.expect_f(t::RBRACKET, &[t::EQ, t::LT, t::LTE, t::GT, t::GTE, t::NEQ])?;
                if !is_relation_type(self.la(1)) {
                    return self.no_viable(0);
                }
                let op = self.relation_type()?;
                let v = self.term()?;
                out.push(single(column, Some(key), op, Value::Term(v)));
            }
            _ => return self.no_viable(0),
        }
        Ok(())
    }

    pub(crate) fn relation_type(&mut self) -> PResult<Operator> {
        let op = match self.la(1) {
            t::EQ => Operator::Eq,
            t::LT => Operator::Lt,
            t::LTE => Operator::Lte,
            t::GT => Operator::Gt,
            t::GTE => Operator::Gte,
            t::NEQ => Operator::Neq,
            _ => return self.no_viable(0),
        };
        self.consume();
        Ok(op)
    }

    /// `containsOperator: CONTAINS (KEY)?`. `KEY` is unreserved, so it is the keyword only when a
    /// term follows it.
    fn contains_operator(&mut self) -> PResult<Operator> {
        self.expect(t::K_CONTAINS)?;
        if self.at(t::K_KEY) {
            let next = self.la(2);
            if is_term_start(next, self.d) || next == t::MINUS {
                self.consume();
                return Ok(Operator::ContainsKey);
            }
        }
        Ok(Operator::Contains)
    }

    fn tuple_of_identifiers(&mut self) -> PResult<Vec<Ident<'a>>> {
        self.expect_f(t::LPAREN, &[F_IDENT])?;
        let mut ids = vec![self.cident()?];
        while self.eat(t::COMMA) {
            ids.push(self.cident()?);
        }
        self.expect_f(t::RPAREN, &[F_END])?;
        Ok(ids)
    }

    /// `singleColumnInValues: '(' (term (',' term)*)? ')'`.
    fn single_column_in_values(&mut self) -> PResult<Vec<Term<'a>>> {
        self.expect_f(t::LPAREN, &[F_TERM, t::RPAREN])?;
        let mut out = Vec::new();
        let ty = self.la(1);
        if is_term_start(ty, self.d) || ty == t::MINUS {
            out.push(self.term()?);
            while self.eat(t::COMMA) {
                out.push(self.term()?);
            }
        }
        self.expect_f(t::RPAREN, &[F_END])?;
        Ok(out)
    }

    pub(crate) fn object_name_pub(&mut self, what: &str) -> PResult<Ident<'a>> {
        if self.at(t::QMARK) {
            self.consume();
            self.custom_error(format!("Bind variables cannot be used for {what} names"))?;
            return Ok(Ident::unquoted("?"));
        }
        self.ident()
    }

    // -----------------------------------------------------------------------------------------
    // INSERT / UPDATE / DELETE / BATCH
    // -----------------------------------------------------------------------------------------

    /// `insertStatement`.
    pub(crate) fn insert_statement(&mut self) -> PResult<Insert<'a>> {
        self.expect(t::K_INSERT)?;
        self.expect_f(t::K_INTO, &[F_IDENT])?;
        let table = self.column_family_name()?;
        let values = match self.la(1) {
            t::LPAREN => {
                self.consume();
                let mut columns = vec![self.cident()?];
                while self.eat(t::COMMA) {
                    columns.push(self.cident()?);
                }
                self.expect_f(t::RPAREN, &[t::K_VALUES])?;
                self.expect_f(t::K_VALUES, &[t::LPAREN])?;
                self.expect_f(t::LPAREN, &[F_TERM])?;
                let mut values = vec![self.term_or_scylla_minus()?];
                while self.eat(t::COMMA) {
                    values.push(self.term_or_scylla_minus()?);
                }
                self.expect_f(t::RPAREN, &[F_END, t::K_IF, t::K_USING])?;
                InsertValues::Values { columns, values }
            }
            t::K_JSON => {
                self.consume();
                let value = match self.la(1) {
                    t::STRING_LITERAL => {
                        let tok = self.consume();
                        Term::Literal(self.literal_from(&tok))
                    }
                    t::COLON | t::QMARK => Term::Bind(self.bind_marker()?),
                    _ => return self.no_viable(0),
                };
                let default = if self.at(t::K_DEFAULT) && (self.cassandra() || matches!(self.la(2), t::K_NULL | t::K_UNSET)) {
                    self.consume();
                    match self.la(1) {
                        t::K_NULL => {
                            self.consume();
                            Some(JsonDefault::Null)
                        }
                        t::K_UNSET => {
                            self.consume();
                            Some(JsonDefault::Unset)
                        }
                        _ => return self.no_viable(0),
                    }
                } else {
                    None
                };
                InsertValues::Json { value, default }
            }
            _ => return self.no_viable(0),
        };
        let mut if_not_exists = false;
        if self.at(t::K_IF) {
            self.consume();
            self.expect_f(t::K_NOT, &[t::K_EXISTS])?;
            self.expect_f(t::K_EXISTS, &[F_END])?;
            if_not_exists = true;
        }
        let using = if self.at(t::K_USING) {
            self.using_clause(&[t::K_TIMESTAMP, t::K_TTL, t::K_TIMEOUT])?
        } else {
            UsingClause::default()
        };
        Ok(Insert { table, values, if_not_exists, using })
    }

    /// A term; in ScyllaDB, a lone `-` reports the next token as unexpected.
    fn term_or_scylla_minus(&mut self) -> PResult<Term<'a>> {
        if self.scylla() && self.at(t::MINUS) {
            // `- NaN` and `- Infinity` only.
            return self.scylla_minus();
        }
        self.term()
    }

    /// `usingClause: USING objective (AND objective)*` with the objectives a statement allows:
    /// `TIMESTAMP` and `TTL` (Cassandra), plus `TIMEOUT` and `CONCURRENCY` (ScyllaDB).
    pub(crate) fn using_clause(&mut self, allowed: &[Tt]) -> PResult<UsingClause<'a>> {
        self.expect(t::K_USING)?;
        let mut u = UsingClause::default();
        loop {
            let ty = self.la(1);
            let scylla_only = matches!(ty, t::K_TIMEOUT | t::K_CONCURRENCY | t::K_SERVICE | t::K_SERVICE_LEVEL);
            if !allowed.contains(&ty) || (scylla_only && self.cassandra()) {
                return self.no_viable(0);
            }
            if matches!(ty, t::K_SERVICE | t::K_SERVICE_LEVEL) {
                self.consume();
                if ty == t::K_SERVICE {
                    self.expect(t::K_LEVEL)?;
                }
                u.service_level = Some(self.role_or_service_level_name(true)?);
                u.order.push(UsingKind::ServiceLevel);
                if !self.eat(t::K_AND) {
                    return Ok(u);
                }
                continue;
            }
            self.consume();
            match ty {
                t::K_TIMESTAMP => {
                    u.timestamp = Some(self.int_value()?);
                    u.order.push(UsingKind::Timestamp);
                }
                t::K_TTL => {
                    u.ttl = Some(self.int_value()?);
                    u.order.push(UsingKind::Ttl);
                }
                t::K_TIMEOUT => {
                    u.timeout = Some(self.term()?);
                    u.order.push(UsingKind::Timeout);
                }
                _ => {
                    u.concurrency = Some(self.term()?);
                    u.order.push(UsingKind::Concurrency);
                }
            }
            if !self.eat(t::K_AND) {
                return Ok(u);
            }
        }
    }

    /// `updateStatement`.
    pub(crate) fn update_statement(&mut self) -> PResult<Update<'a>> {
        self.expect(t::K_UPDATE)?;
        let table = self.column_family_name()?;
        let using = if self.at(t::K_USING) {
            self.using_clause(&[t::K_TIMESTAMP, t::K_TTL, t::K_TIMEOUT])?
        } else {
            UsingClause::default()
        };
        self.expect_f(t::K_SET, &[F_IDENT])?;
        let mut assignments: Vec<Assignment<'a>> = Vec::new();
        let mut null_key = false;
        self.column_operation(&mut assignments, &mut null_key)?;
        while self.eat(t::COMMA) {
            self.column_operation(&mut assignments, &mut null_key)?;
        }
        self.expect_f(t::K_WHERE, &[F_IDENT, t::LPAREN, t::K_TOKEN])?;
        let where_clause = self.where_clause()?;
        let conditions = self.if_clause(false)?;
        Ok(Update { table, using, assignments, where_clause, conditions })
    }

    /// `IF EXISTS | IF conditions` of `UPDATE` / `DELETE`.
    fn if_clause(&mut self, delete: bool) -> PResult<Conditions<'a>> {
        if !self.at(t::K_IF) {
            return Ok(Conditions::None);
        }
        self.consume();
        let ty = self.la(1);
        if ty == t::K_EXISTS {
            let next = self.la(2);
            let ends = matches!(next, t::EOF | t::K_APPLY | t::K_DELETE | t::K_INSERT | t::K_UPDATE | t::SEMI)
                || (delete && self.scylla() && next == t::K_BYPASS);
            if ends {
                self.consume();
                return Ok(Conditions::Exists);
            }
            let cond = matches!(next, t::K_CONTAINS | t::K_IN | t::NEQ | t::DOT | t::LBRACKET)
                || matches!(next, t::LT | t::LTE | t::EQ | t::GT | t::GTE)
                || (self.scylla() && matches!(next, t::K_NOT | t::K_LIKE));
            if !cond {
                return self.no_viable(1);
            }
        } else if self.scylla() && ty == t::K_NOT {
            // ScyllaDB has no `IF NOT EXISTS` on updates: `NOT` is reported as a missing name.
            return self.no_viable(0);
        } else if !(ty == t::EMPTY_QUOTED_NAME || t::is_ident(ty, self.d)) {
            return self.no_viable(0);
        }
        let mut out = vec![self.column_condition()?];
        while self.eat(t::K_AND) {
            out.push(self.column_condition()?);
        }
        Ok(Conditions::List(out))
    }

    /// `columnCondition`.
    fn column_condition(&mut self) -> PResult<Condition<'a>> {
        let column = self.cident()?;
        let ty = self.la(1);
        let (target, op_ty) = match ty {
            t::LBRACKET => {
                self.consume();
                let e = self.term()?;
                self.expect_f(t::RBRACKET, &[t::EQ, t::LT, t::LTE, t::GT, t::GTE, t::NEQ, t::K_IN])?;
                (ConditionTarget::Element(e), self.la(1))
            }
            t::DOT => {
                self.consume();
                let f = self.ident()?;
                (ConditionTarget::Field(f), self.la(1))
            }
            _ => (ConditionTarget::Column, ty),
        };
        let column_level = matches!(target, ConditionTarget::Column);
        let (op, value) = match op_ty {
            _ if is_relation_type(op_ty) => {
                let op = self.relation_type()?;
                (op, Value::Term(self.term()?))
            }
            t::K_CONTAINS if column_level && self.cassandra() => {
                let op = self.contains_operator()?;
                (op, Value::Term(self.term()?))
            }
            t::K_LIKE if self.scylla() => {
                self.consume();
                (Operator::Like, Value::Term(self.term()?))
            }
            t::K_IN => {
                self.consume();
                (Operator::In, self.in_value()?)
            }
            t::K_NOT if self.scylla() => {
                self.consume();
                self.expect_f(t::K_IN, &[t::LPAREN, t::QMARK, t::COLON])?;
                (Operator::NotIn, self.in_value()?)
            }
            _ => return self.no_viable(0),
        };
        Ok(Condition { column, target, op, value })
    }

    /// After `IN`: `(terms)` or a marker.
    fn in_value(&mut self) -> PResult<Value<'a>> {
        match self.la(1) {
            t::LPAREN => Ok(Value::List(self.single_column_in_values()?)),
            t::QMARK | t::COLON => Ok(Value::ListMarker(self.bind_marker()?)),
            _ => self.no_viable(0),
        }
    }

    /// `columnOperation: cident columnOperationDifferentiator`.
    ///
    /// Cassandra's parser carries on after a missing column name (a `null` key) or a bad
    /// operator; comparing a `null` key later throws, which this reproduces.
    fn column_operation(&mut self, ops: &mut Vec<Assignment<'a>>, null_key: &mut bool) -> PResult<()> {
        let recover = self.cassandra() && self.spec == 0;
        let at = self.pos;
        let column = match self.cident() {
            Ok(c) => Some(c),
            Err(_) if recover && self.pos == at => None,
            Err(f) => return Err(f),
        };
        let op = match self.la(1) {
            t::EQ => {
                self.consume();
                self.normal_column_operation(column.as_ref())?
            }
            t::PLUS_EQ | t::MINUS_EQ => {
                let sig = self.consume();
                let v = self.term()?;
                if sig.ty == t::PLUS_EQ {
                    Operation::Add(v, OpForm::Shorthand)
                } else {
                    Operation::Subtract(v, OpForm::Shorthand)
                }
            }
            t::LBRACKET => {
                if self.scylla() && !(super::is_term_start(self.la(2), self.d) || self.la(2) == t::MINUS) {
                    return self.no_viable(0);
                }
                self.consume();
                let k = self.term()?;
                self.expect_f(t::RBRACKET, &[t::EQ])?;
                self.expect_f(t::EQ, &[F_TERM])?;
                let v = self.term_or_scylla_minus()?;
                Operation::SetElement(k, v)
            }
            t::DOT => {
                self.consume();
                let f = self.ident()?;
                self.expect_f(t::EQ, &[F_TERM])?;
                let v = self.term_or_scylla_minus()?;
                Operation::SetField(f, v)
            }
            _ if recover => {
                let _ = self.no_viable::<()>(0);
                return Ok(());
            }
            _ => return self.no_viable(0),
        };
        if *null_key {
            return Err(self.null_pointer(
                "Cannot invoke \"org.apache.cassandra.cql3.ColumnIdentifier.equals(Object)\" because \"p.left\" is null",
            ));
        }
        match column {
            Some(column) => self.add_raw_update(ops, Assignment { column, op }),
            None => {
                *null_key = true;
                Ok(())
            }
        }
    }

    /// `addRawUpdate`: rejects incompatible operations on the same column.
    fn add_raw_update(&mut self, ops: &mut Vec<Assignment<'a>>, new: Assignment<'a>) -> PResult<()> {
        let key = new.column.name().into_owned();
        let incompatible = ops.iter().any(|p| {
            p.column.name() == key
                && match (&p.op, &new.op) {
                    (Operation::Set(_), _) => true,
                    (Operation::SetField(f1, _), Operation::SetField(f2, _)) => f1.name() == f2.name(),
                    (_, Operation::Set(_)) => true,
                    _ => false,
                }
        });
        // ScyllaDB checks this when preparing (an invalid request); Cassandra while parsing.
        if incompatible && self.cassandra() {
            self.custom_error(format!("Multiple incompatible setting of column {key}"))?;
        }
        ops.push(new);
        Ok(())
    }

    /// `normalColumnOperation`: `t`, `t + c`, `c + t`, `c - t`, `c -3`.
    fn normal_column_operation(&mut self, key: Option<&Ident<'a>>) -> PResult<Operation<'a>> {
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA239)?
        } else {
            let ty = self.la(1);
            let next = self.la(2);
            if (t::is_ident(ty, self.d) || ty == t::EMPTY_QUOTED_NAME) && matches!(next, t::PLUS | t::MINUS) {
                2
            } else if (t::is_ident(ty, self.d) || ty == t::EMPTY_QUOTED_NAME) && next == t::INTEGER {
                3
            } else if t::is_ident(ty, self.d) && !matches!(next, t::LPAREN | t::DOT) {
                return self.no_viable(0);
            } else if ty == t::MINUS {
                // `- NaN` / `- Infinity`; anything else after `-` is unexpected.
                return self.scylla_minus().map(Operation::Set);
            } else {
                1
            }
        };
        let key_name = key.map(|k| k.name().into_owned());
        let key_npe = "Cannot invoke \"org.apache.cassandra.cql3.ColumnIdentifier.equals(Object)\" because \"key\" is null";
        match alt {
            1 => {
                let v = self.term()?;
                if self.at(t::PLUS) {
                    self.consume();
                    let c = self.cident()?;
                    let Some(key_name) = &key_name else { return Err(self.null_pointer(key_npe)) };
                    if c.name() != *key_name {
                        self.custom_error("Only expressions of the form X = <value> + X are supported.")?;
                    }
                    return Ok(Operation::Prepend(v));
                }
                Ok(Operation::Set(v))
            }
            2 => {
                let c = self.cident()?;
                let sig = self.la(1);
                if sig != t::PLUS && sig != t::MINUS {
                    return self.mismatched_set();
                }
                self.consume();
                let v = self.term_or_scylla_minus()?;
                let Some(key_name) = &key_name else { return Err(self.null_pointer(key_npe)) };
                if c.name() != *key_name {
                    let s = if sig == t::PLUS { "+" } else { "-" };
                    self.custom_error(format!("Only expressions of the form X = X {s}<value> are supported."))?;
                }
                Ok(if sig == t::PLUS { Operation::Add(v, OpForm::Long) } else { Operation::Subtract(v, OpForm::Long) })
            }
            _ => {
                let c = self.cident()?;
                let tok = self.expect(t::INTEGER)?;
                let text = self.text(&tok);
                let Some(key_name) = &key_name else { return Err(self.null_pointer(key_npe)) };
                if c.name() != *key_name {
                    let s = if text.starts_with('-') { '-' } else { '+' };
                    self.custom_error(format!("Only expressions of the form X = X {s} <value> are supported."))?;
                }
                Ok(Operation::Add(Term::Literal(self.literal_from(&tok)), OpForm::Juxtaposed))
            }
        }
    }

    /// `deleteStatement`.
    pub(crate) fn delete_statement(&mut self) -> PResult<Delete<'a>> {
        self.expect(t::K_DELETE)?;
        let mut columns = Vec::new();
        let ty = self.la(1);
        if ty == t::EMPTY_QUOTED_NAME || t::is_ident(ty, self.d) {
            columns.push(self.delete_op()?);
            while self.eat(t::COMMA) {
                columns.push(self.delete_op()?);
            }
        }
        self.expect_f(t::K_FROM, &[F_IDENT, t::QMARK])?;
        let table = self.column_family_name()?;
        let using = if self.at(t::K_USING) {
            if self.cassandra() {
                self.expect(t::K_USING)?;
                self.expect(t::K_TIMESTAMP)?;
                let ts = self.int_value()?;
                UsingClause { timestamp: Some(ts), order: vec![UsingKind::Timestamp], ..Default::default() }
            } else {
                self.using_clause(&[t::K_TIMESTAMP, t::K_TIMEOUT])?
            }
        } else {
            UsingClause::default()
        };
        self.expect_f(t::K_WHERE, &[F_IDENT, t::LPAREN, t::K_TOKEN])?;
        let where_clause = self.where_clause()?;
        let conditions = self.if_clause(true)?;
        let mut bypass_guardrails = false;
        if self.scylla() && self.at(t::K_BYPASS) {
            self.consume();
            self.expect_f(t::K_LARGE_DATA_GUARDRAILS, &[F_END])?;
            bypass_guardrails = true;
        }
        Ok(Delete { columns, table, using, where_clause, conditions, bypass_guardrails })
    }

    /// `deleteOp: cident | cident '[' term ']' | cident '.' fident`.
    fn delete_op(&mut self) -> PResult<Deletion<'a>> {
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA62)?
        } else {
            match self.la(2) {
                t::LBRACKET => 2,
                t::DOT => 3,
                t::COMMA | t::K_FROM => 1,
                _ => return self.no_viable(0),
            }
        };
        let c = self.cident()?;
        match alt {
            1 => Ok(Deletion::Column(c)),
            2 => {
                self.expect(t::LBRACKET)?;
                let k = self.term()?;
                self.expect_f(t::RBRACKET, &[F_END])?;
                Ok(Deletion::Element(c, k))
            }
            _ => {
                self.expect_f(t::DOT, &[F_IDENT])?;
                let f = self.ident()?;
                Ok(Deletion::Field(c, f))
            }
        }
    }

    /// `batchStatement`.
    pub(crate) fn batch_statement(&mut self) -> PResult<Batch<'a>> {
        self.expect(t::K_BEGIN)?;
        let kind = match self.la(1) {
            t::K_UNLOGGED => {
                self.consume();
                BatchKind::Unlogged
            }
            t::K_COUNTER => {
                self.consume();
                BatchKind::Counter
            }
            _ => BatchKind::Logged,
        };
        self.expect_f(t::K_BATCH, &[t::K_INSERT, t::K_UPDATE, t::K_DELETE, t::K_APPLY, t::K_USING])?;
        let using = if self.at(t::K_USING) {
            self.using_clause(&[t::K_TIMESTAMP, t::K_TTL, t::K_TIMEOUT])?
        } else {
            UsingClause::default()
        };
        let mut statements = Vec::new();
        loop {
            match self.la(1) {
                t::K_INSERT => statements.push(BatchStatement::Insert(self.insert_statement()?)),
                t::K_UPDATE => statements.push(BatchStatement::Update(self.update_statement()?)),
                t::K_DELETE => statements.push(BatchStatement::Delete(self.delete_statement()?)),
                _ => break,
            }
            self.eat(t::SEMI);
        }
        self.expect_f(t::K_APPLY, &[t::K_BATCH])?;
        self.expect_f(t::K_BATCH, &[F_END])?;
        Ok(Batch { kind, using, statements })
    }

    /// `USE keyspace`.
    pub(crate) fn use_statement(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_USE)?;
        Ok(StatementKind::Use(self.keyspace_name()?))
    }

    /// `TRUNCATE [TABLE] name` (+ ScyllaDB's `USING TIMEOUT`).
    pub(crate) fn truncate_statement(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_TRUNCATE)?;
        self.eat(t::K_COLUMNFAMILY);
        let table = self.column_family_name()?;
        let using = if self.scylla() && self.at(t::K_USING) {
            // `TRUNCATE ... USING` matches `TIMEOUT` rather than deciding on it.
            if self.la(2) != t::K_TIMEOUT {
                self.consume();
                self.expect_f(t::K_TIMEOUT, &[F_TERM])?;
            }
            self.using_clause(&[t::K_TIMEOUT])?
        } else {
            UsingClause::default()
        };
        Ok(StatementKind::Truncate(Box::new(Truncate { table, using })))
    }

    /// ScyllaDB: `PRUNE MATERIALIZED VIEW name [WHERE ...] [USING CONCURRENCY n AND TIMEOUT d]`.
    fn prune_view(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_PRUNE)?;
        self.expect_f(t::K_MATERIALIZED, &[t::K_VIEW])?;
        self.expect_f(t::K_VIEW, &[F_IDENT])?;
        let view = self.column_family_name()?;
        let where_clause = if self.eat(t::K_WHERE) { self.where_clause()? } else { Vec::new() };
        let using =
            if self.at(t::K_USING) { self.using_clause(&[t::K_CONCURRENCY, t::K_TIMEOUT])? } else { UsingClause::default() };
        Ok(StatementKind::PruneView(Box::new(PruneView { view, where_clause, using })))
    }
}

fn single<'a>(column: Ident<'a>, key: Option<Term<'a>>, op: Operator, value: Value<'a>) -> Relation<'a> {
    Relation::Single(SingleRelation { column, key, op, value })
}
