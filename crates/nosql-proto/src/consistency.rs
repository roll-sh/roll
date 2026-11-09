//! `[consistency]` levels and their wire codes.

use std::fmt;

/// A consistency level. `NodeLocal` (11) is Cassandra's internal level, accepted on the wire
/// like the reference does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Consistency {
    Any,
    One,
    Two,
    Three,
    Quorum,
    All,
    LocalQuorum,
    EachQuorum,
    Serial,
    LocalSerial,
    LocalOne,
    NodeLocal,
}

impl Consistency {
    pub const ALL_LEVELS: [Consistency; 12] = [
        Consistency::Any,
        Consistency::One,
        Consistency::Two,
        Consistency::Three,
        Consistency::Quorum,
        Consistency::All,
        Consistency::LocalQuorum,
        Consistency::EachQuorum,
        Consistency::Serial,
        Consistency::LocalSerial,
        Consistency::LocalOne,
        Consistency::NodeLocal,
    ];

    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL_LEVELS.get(code as usize).copied()
    }

    pub fn code(self) -> u16 {
        self as u16
    }

    /// The name as CQL and error messages print it (`LOCAL_QUORUM`).
    pub fn name(self) -> &'static str {
        match self {
            Consistency::Any => "ANY",
            Consistency::One => "ONE",
            Consistency::Two => "TWO",
            Consistency::Three => "THREE",
            Consistency::Quorum => "QUORUM",
            Consistency::All => "ALL",
            Consistency::LocalQuorum => "LOCAL_QUORUM",
            Consistency::EachQuorum => "EACH_QUORUM",
            Consistency::Serial => "SERIAL",
            Consistency::LocalSerial => "LOCAL_SERIAL",
            Consistency::LocalOne => "LOCAL_ONE",
            Consistency::NodeLocal => "NODE_LOCAL",
        }
    }

    /// Parses a name case-insensitively (`CONSISTENCY local_one` in cqlsh).
    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL_LEVELS.into_iter().find(|c| c.name().eq_ignore_ascii_case(s))
    }

    pub fn is_serial(self) -> bool {
        matches!(self, Consistency::Serial | Consistency::LocalSerial)
    }
}

impl fmt::Display for Consistency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        for (i, c) in Consistency::ALL_LEVELS.iter().enumerate() {
            assert_eq!(c.code() as usize, i);
            assert_eq!(Consistency::from_code(i as u16), Some(*c));
            assert_eq!(Consistency::from_name(&c.name().to_lowercase()), Some(*c));
        }
        assert_eq!(Consistency::from_code(12), None);
        assert_eq!(Consistency::LocalOne.code(), 0x0A);
    }
}
