//! A text builder with indentation: port of Cassandra's `org.apache.cassandra.cql3.CqlBuilder`
//! (an indent is applied lazily at the first write after a newline, four spaces per level).

use std::collections::BTreeMap;

use nosql_types::Dialect;

pub struct CqlBuilder {
    out: String,
    indent: usize,
    new_line: bool,
    dialect: Dialect,
}

impl CqlBuilder {
    pub fn new(dialect: Dialect) -> Self {
        CqlBuilder {
            out: String::with_capacity(256),
            indent: 0,
            new_line: false,
            dialect,
        }
    }

    fn indent_if_needed(&mut self) {
        if self.new_line {
            for _ in 0..self.indent {
                self.out.push_str("    ");
            }
            self.new_line = false;
        }
    }

    pub fn append(&mut self, s: impl AsRef<str>) -> &mut Self {
        self.indent_if_needed();
        self.out.push_str(s.as_ref());
        self
    }

    pub fn append_display(&mut self, v: impl std::fmt::Display) -> &mut Self {
        self.indent_if_needed();
        use std::fmt::Write;
        let _ = write!(self.out, "{v}");
        self
    }

    /// `appendQuotingIfNeeded`: an identifier with `ColumnIdentifier.maybeQuote`.
    pub fn ident(&mut self, name: &str) -> &mut Self {
        let q = quote(name, self.dialect);
        self.append(q)
    }

    /// `appendWithSingleQuotes`: a string literal with `'` doubled.
    pub fn quoted(&mut self, s: &str) -> &mut Self {
        self.indent_if_needed();
        self.out.push('\'');
        self.out.push_str(&s.replace('\'', "''"));
        self.out.push('\'');
        self
    }

    /// `append(Map)`: `{'k': 'v', ...}` in key order, values quoted unless `quote_values` is
    /// false.
    pub fn map(&mut self, m: &BTreeMap<String, String>, quote_values: bool) -> &mut Self {
        self.indent_if_needed();
        self.out.push('{');
        for (i, (k, v)) in m.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.quoted(k);
            self.out.push_str(": ");
            if quote_values {
                self.quoted(v);
            } else {
                self.out.push_str(v);
            }
        }
        self.out.push('}');
        self
    }

    pub fn new_line(&mut self) -> &mut Self {
        self.out.push('\n');
        self.new_line = true;
        self
    }

    pub fn increase_indent(&mut self) -> &mut Self {
        self.indent += 1;
        self
    }

    pub fn decrease_indent(&mut self) -> &mut Self {
        self.indent = self.indent.saturating_sub(1);
        self
    }

    /// `appendWithSeparators`.
    pub fn join<T>(
        &mut self,
        items: impl IntoIterator<Item = T>,
        sep: &str,
        mut f: impl FnMut(&mut Self, T),
    ) -> &mut Self {
        for (i, it) in items.into_iter().enumerate() {
            if i > 0 {
                self.append(sep);
            }
            f(self, it);
        }
        self
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// `ColumnIdentifier.maybeQuote` for the dialect.
pub fn quote(name: &str, d: Dialect) -> String {
    nosql_types::quote_identifier(name, d).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_lazily_and_quotes() {
        let mut b = CqlBuilder::new(Dialect::Cassandra);
        b.append("CREATE TYPE ")
            .ident("ks")
            .append(".")
            .ident("Odd Name")
            .append(" (")
            .new_line()
            .increase_indent()
            .ident("a")
            .append(" int")
            .new_line()
            .decrease_indent()
            .append(");");
        assert_eq!(b.finish(), "CREATE TYPE ks.\"Odd Name\" (\n    a int\n);");
        let mut b = CqlBuilder::new(Dialect::Scylla);
        let m: BTreeMap<String, String> = [
            ("b".to_string(), "it's".to_string()),
            ("a".into(), "1".into()),
        ]
        .into();
        b.map(&m, true).append(" ").map(&m, false);
        assert_eq!(b.finish(), "{'a': '1', 'b': 'it''s'} {'a': 1, 'b': it's}");
    }
}
