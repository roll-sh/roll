//! Protocol versions, the two server dialects, and [`Ctx`]: everything an encoder or decoder
//! needs to know about the connection.

use std::fmt;

/// A native protocol version this crate speaks. `V6` is Cassandra 5.0's `6/v6-beta`: it is
/// only accepted with the USE_BETA flag and its wire format is v5's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Version {
    V3 = 3,
    V4 = 4,
    V5 = 5,
    V6 = 6,
}

impl Version {
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            3 => Some(Version::V3),
            4 => Some(Version::V4),
            5 => Some(Version::V5),
            6 => Some(Version::V6),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn is_beta(self) -> bool {
        self == Version::V6
    }

    /// After STARTUP, v5+ connections wrap envelopes in CRC-protected segments.
    pub fn uses_segments(self) -> bool {
        self >= Version::V5
    }

    /// `v4`, `v6-beta`: the description Cassandra pairs with the number.
    pub fn description(self) -> &'static str {
        match self {
            Version::V3 => "v3",
            Version::V4 => "v4",
            Version::V5 => "v5",
            Version::V6 => "v6-beta",
        }
    }

    /// The version byte of a request (`0x04`) or response (`0x84`).
    pub fn byte(self, dir: Direction) -> u8 {
        match dir {
            Direction::Request => self.as_u8(),
            Direction::Response => self.as_u8() | RESPONSE_BIT,
        }
    }
}

/// `4/v4` — how `SUPPORTED` and Cassandra's error messages print a version.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.as_u8(), self.description())
    }
}

/// The direction bit of the version byte.
pub const RESPONSE_BIT: u8 = 0x80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Request,
    Response,
}

impl Direction {
    pub fn of(version_byte: u8) -> Self {
        if version_byte & RESPONSE_BIT == 0 { Direction::Request } else { Direction::Response }
    }

    /// `REQUEST` / `RESPONSE`, as Cassandra prints it.
    pub fn name(self) -> &'static str {
        match self {
            Direction::Request => "REQUEST",
            Direction::Response => "RESPONSE",
        }
    }
}

/// Which reference a server imitates. Error texts, negotiation and a few leniencies differ;
/// the wire format does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// Apache Cassandra 5.0.9.
    #[default]
    Cassandra,
    /// ScyllaDB 2026.3.
    Scylla,
}

impl Dialect {
    /// The versions the reference accepts, oldest first.
    pub fn versions(self) -> &'static [Version] {
        match self {
            Dialect::Cassandra => &[Version::V3, Version::V4, Version::V5, Version::V6],
            Dialect::Scylla => &[Version::V3, Version::V4],
        }
    }

    pub fn supports(self, v: Version) -> bool {
        self.versions().contains(&v)
    }

    /// The highest non-beta version: what the reference answers with before it knows the
    /// connection's version (Cassandra's `ProtocolVersion.CURRENT`).
    pub fn current(self) -> Version {
        match self {
            Dialect::Cassandra => Version::V5,
            Dialect::Scylla => Version::V4,
        }
    }
}

/// What the peers agreed on, beyond the version: ScyllaDB's protocol extensions change the
/// layout of a few messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Features {
    /// `SCYLLA_USE_METADATA_ID`: v5's result metadata ids (Prepared, EXECUTE,
    /// Metadata_changed) on v4.
    pub metadata_id: bool,
    /// `TABLETS_ROUTING_V2_EXPERIMENTAL`: EXECUTE carries a trailing `tablet_version_block`
    /// byte.
    pub tablets_v2: bool,
    /// `SCYLLA_RATE_LIMIT_ERROR`: the error code that carries the rate-limit body.
    pub rate_limit_code: Option<i32>,
}

/// The connection context every body encoder/decoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ctx {
    pub version: Version,
    pub dialect: Dialect,
    pub features: Features,
}

impl Ctx {
    pub fn new(version: Version) -> Self {
        Ctx { version, dialect: Dialect::default(), features: Features::default() }
    }

    pub fn dialect(mut self, d: Dialect) -> Self {
        self.dialect = d;
        self
    }

    pub fn features(mut self, f: Features) -> Self {
        self.features = f;
        self
    }

    /// Whether result metadata ids are on the wire (v5+, or v4 with the ScyllaDB extension).
    pub fn metadata_id(&self) -> bool {
        self.version >= Version::V5 || self.features.metadata_id
    }
}

impl From<Version> for Ctx {
    fn from(v: Version) -> Self {
        Ctx::new(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(Version::V4.to_string(), "4/v4");
        assert_eq!(Version::V6.to_string(), "6/v6-beta");
        assert_eq!(Version::V5.byte(Direction::Response), 0x85);
        assert_eq!(Direction::of(0x84), Direction::Response);
        assert!(Version::V6.uses_segments() && !Version::V4.uses_segments());
        assert!(!Dialect::Scylla.supports(Version::V5));
        assert_eq!(Dialect::Cassandra.current(), Version::V5);
    }

    #[test]
    fn metadata_id_gate() {
        assert!(!Ctx::new(Version::V4).metadata_id());
        assert!(Ctx::new(Version::V5).metadata_id());
        let f = Features { metadata_id: true, ..Default::default() };
        assert!(Ctx::new(Version::V4).features(f).metadata_id());
    }
}
