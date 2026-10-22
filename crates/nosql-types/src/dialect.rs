//! Which reference a database imitates.

/// The product a database behaves like. Everything that differs between the two (error texts,
/// number formatting, accepted literal forms, marshal class spellings…) takes one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// ScyllaDB 2026.3.
    #[default]
    Scylla,
    /// Apache Cassandra 5.0.
    Cassandra,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Scylla => "scylla",
            Dialect::Cassandra => "cassandra",
        }
    }

    pub fn parse(s: &str) -> Option<Dialect> {
        match s.to_ascii_lowercase().as_str() {
            "scylla" | "scylladb" => Some(Dialect::Scylla),
            "cassandra" => Some(Dialect::Cassandra),
            _ => None,
        }
    }

    pub fn is_scylla(self) -> bool {
        self == Dialect::Scylla
    }
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_parsing() {
        assert_eq!(Dialect::default(), Dialect::Scylla);
        assert_eq!(Dialect::Scylla.name(), "scylla");
        assert_eq!(Dialect::Cassandra.to_string(), "cassandra");
        assert_eq!(Dialect::parse("ScyllaDB"), Some(Dialect::Scylla));
        assert_eq!(Dialect::parse("scylla"), Some(Dialect::Scylla));
        assert_eq!(Dialect::parse("CASSANDRA"), Some(Dialect::Cassandra));
        assert_eq!(Dialect::parse("postgres"), None);
        assert!(Dialect::Scylla.is_scylla());
        assert!(!Dialect::Cassandra.is_scylla());
    }
}
