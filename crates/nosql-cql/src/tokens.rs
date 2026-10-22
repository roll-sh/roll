//! Token types and the keyword table.
//!
//! Token type numbers are those ANTLR 3.5.2 assigns when it builds Cassandra 5.0.9's `Cql.g`
//! (`Cql.tokens`), because the prediction tables in [`crate::parser::dfa`] are indexed by them.
//! Keywords that only ScyllaDB has are numbered after Cassandra's last token; in the Cassandra
//! dialect they are never produced (the word lexes as an `IDENT`), and vice versa.

use crate::Dialect;

/// A token type.
pub(crate) type Tt = u16;

pub(crate) const EOF: Tt = u16::MAX;

pub(crate) const BOOLEAN: Tt = 6;
pub(crate) const DURATION: Tt = 11;
pub(crate) const EMPTY_QUOTED_NAME: Tt = 17;
pub(crate) const FLOAT: Tt = 20;
pub(crate) const HEXNUMBER: Tt = 24;
pub(crate) const IDENT: Tt = 26;
pub(crate) const INTEGER: Tt = 27;
pub(crate) const K_ACCESS: Tt = 30;
pub(crate) const K_ADD: Tt = 31;
pub(crate) const K_AGGREGATE: Tt = 32;
pub(crate) const K_AGGREGATES: Tt = 33;
pub(crate) const K_ALL: Tt = 34;
pub(crate) const K_ALLOW: Tt = 35;
pub(crate) const K_ALTER: Tt = 36;
pub(crate) const K_AND: Tt = 37;
pub(crate) const K_ANN: Tt = 38;
pub(crate) const K_APPLY: Tt = 39;
pub(crate) const K_AS: Tt = 40;
pub(crate) const K_ASC: Tt = 41;
pub(crate) const K_ASCII: Tt = 42;
pub(crate) const K_AUTHORIZE: Tt = 43;
pub(crate) const K_BATCH: Tt = 44;
pub(crate) const K_BEGIN: Tt = 45;
pub(crate) const K_BIGINT: Tt = 46;
pub(crate) const K_BLOB: Tt = 47;
pub(crate) const K_BOOLEAN: Tt = 48;
pub(crate) const K_BY: Tt = 49;
pub(crate) const K_CALLED: Tt = 50;
pub(crate) const K_CAST: Tt = 51;
pub(crate) const K_CIDRS: Tt = 52;
pub(crate) const K_CLUSTER: Tt = 53;
pub(crate) const K_CLUSTERING: Tt = 54;
pub(crate) const K_COLUMNFAMILY: Tt = 55;
pub(crate) const K_COMPACT: Tt = 56;
pub(crate) const K_CONTAINS: Tt = 57;
pub(crate) const K_COUNT: Tt = 58;
pub(crate) const K_COUNTER: Tt = 59;
pub(crate) const K_CREATE: Tt = 60;
pub(crate) const K_CUSTOM: Tt = 61;
pub(crate) const K_DATACENTERS: Tt = 62;
pub(crate) const K_DATE: Tt = 63;
pub(crate) const K_DECIMAL: Tt = 64;
pub(crate) const K_DEFAULT: Tt = 65;
pub(crate) const K_DELETE: Tt = 66;
pub(crate) const K_DESC: Tt = 67;
pub(crate) const K_DESCRIBE: Tt = 68;
pub(crate) const K_DISTINCT: Tt = 69;
pub(crate) const K_DOUBLE: Tt = 70;
pub(crate) const K_DROP: Tt = 71;
pub(crate) const K_DURATION: Tt = 72;
pub(crate) const K_ENTRIES: Tt = 73;
pub(crate) const K_EXECUTE: Tt = 74;
pub(crate) const K_EXISTS: Tt = 75;
pub(crate) const K_FILTERING: Tt = 76;
pub(crate) const K_FINALFUNC: Tt = 77;
pub(crate) const K_FLOAT: Tt = 78;
pub(crate) const K_FROM: Tt = 79;
pub(crate) const K_FROZEN: Tt = 80;
pub(crate) const K_FULL: Tt = 81;
pub(crate) const K_FUNCTION: Tt = 82;
pub(crate) const K_FUNCTIONS: Tt = 83;
pub(crate) const K_GRANT: Tt = 84;
pub(crate) const K_GROUP: Tt = 85;
pub(crate) const K_HASHED: Tt = 86;
pub(crate) const K_IDENTITY: Tt = 87;
pub(crate) const K_IF: Tt = 88;
pub(crate) const K_IN: Tt = 89;
pub(crate) const K_INDEX: Tt = 90;
pub(crate) const K_INET: Tt = 91;
pub(crate) const K_INITCOND: Tt = 92;
pub(crate) const K_INPUT: Tt = 93;
pub(crate) const K_INSERT: Tt = 94;
pub(crate) const K_INT: Tt = 95;
pub(crate) const K_INTERNALS: Tt = 96;
pub(crate) const K_INTO: Tt = 97;
pub(crate) const K_IS: Tt = 98;
pub(crate) const K_JSON: Tt = 99;
pub(crate) const K_KEY: Tt = 100;
pub(crate) const K_KEYS: Tt = 101;
pub(crate) const K_KEYSPACE: Tt = 102;
pub(crate) const K_KEYSPACES: Tt = 103;
pub(crate) const K_LANGUAGE: Tt = 104;
pub(crate) const K_LIKE: Tt = 105;
pub(crate) const K_LIMIT: Tt = 106;
pub(crate) const K_LIST: Tt = 107;
pub(crate) const K_LOGIN: Tt = 108;
pub(crate) const K_MAP: Tt = 109;
pub(crate) const K_MASKED: Tt = 110;
pub(crate) const K_MATERIALIZED: Tt = 111;
pub(crate) const K_MAXWRITETIME: Tt = 112;
pub(crate) const K_MBEAN: Tt = 113;
pub(crate) const K_MBEANS: Tt = 114;
pub(crate) const K_MODIFY: Tt = 115;
pub(crate) const K_NEGATIVE_INFINITY: Tt = 116;
pub(crate) const K_NEGATIVE_NAN: Tt = 117;
pub(crate) const K_NOLOGIN: Tt = 118;
pub(crate) const K_NORECURSIVE: Tt = 119;
pub(crate) const K_NOSUPERUSER: Tt = 120;
pub(crate) const K_NOT: Tt = 121;
pub(crate) const K_NULL: Tt = 122;
pub(crate) const K_OF: Tt = 123;
pub(crate) const K_ON: Tt = 124;
pub(crate) const K_ONLY: Tt = 125;
pub(crate) const K_OPTIONS: Tt = 126;
pub(crate) const K_OR: Tt = 127;
pub(crate) const K_ORDER: Tt = 128;
pub(crate) const K_PARTITION: Tt = 129;
pub(crate) const K_PASSWORD: Tt = 130;
pub(crate) const K_PER: Tt = 131;
pub(crate) const K_PERMISSION: Tt = 132;
pub(crate) const K_PERMISSIONS: Tt = 133;
pub(crate) const K_POSITIVE_INFINITY: Tt = 134;
pub(crate) const K_POSITIVE_NAN: Tt = 135;
pub(crate) const K_PRIMARY: Tt = 136;
pub(crate) const K_RENAME: Tt = 137;
pub(crate) const K_REPLACE: Tt = 138;
pub(crate) const K_RETURNS: Tt = 139;
pub(crate) const K_REVOKE: Tt = 140;
pub(crate) const K_ROLE: Tt = 141;
pub(crate) const K_ROLES: Tt = 142;
pub(crate) const K_SCHEMA: Tt = 143;
pub(crate) const K_SELECT: Tt = 144;
pub(crate) const K_SELECT_MASKED: Tt = 145;
pub(crate) const K_SET: Tt = 146;
pub(crate) const K_SFUNC: Tt = 147;
pub(crate) const K_SMALLINT: Tt = 148;
pub(crate) const K_STATIC: Tt = 149;
pub(crate) const K_STORAGE: Tt = 150;
pub(crate) const K_STYPE: Tt = 151;
pub(crate) const K_SUPERUSER: Tt = 152;
pub(crate) const K_TABLES: Tt = 153;
pub(crate) const K_TEXT: Tt = 154;
pub(crate) const K_TIME: Tt = 155;
pub(crate) const K_TIMESTAMP: Tt = 156;
pub(crate) const K_TIMEUUID: Tt = 157;
pub(crate) const K_TINYINT: Tt = 158;
pub(crate) const K_TO: Tt = 159;
pub(crate) const K_TOKEN: Tt = 160;
pub(crate) const K_TRIGGER: Tt = 161;
pub(crate) const K_TRUNCATE: Tt = 162;
pub(crate) const K_TTL: Tt = 163;
pub(crate) const K_TUPLE: Tt = 164;
pub(crate) const K_TYPE: Tt = 165;
pub(crate) const K_TYPES: Tt = 166;
pub(crate) const K_UNLOGGED: Tt = 167;
pub(crate) const K_UNMASK: Tt = 168;
pub(crate) const K_UNSET: Tt = 169;
pub(crate) const K_UPDATE: Tt = 170;
pub(crate) const K_USE: Tt = 171;
pub(crate) const K_USER: Tt = 172;
pub(crate) const K_USERS: Tt = 173;
pub(crate) const K_USING: Tt = 174;
pub(crate) const K_UUID: Tt = 175;
pub(crate) const K_VALUES: Tt = 176;
pub(crate) const K_VARCHAR: Tt = 177;
pub(crate) const K_VARINT: Tt = 178;
pub(crate) const K_VECTOR: Tt = 179;
pub(crate) const K_VIEW: Tt = 180;
pub(crate) const K_WHERE: Tt = 181;
pub(crate) const K_WITH: Tt = 182;
pub(crate) const K_WRITETIME: Tt = 183;
pub(crate) const QMARK: Tt = 192;
pub(crate) const QUOTED_NAME: Tt = 193;
pub(crate) const RANGE: Tt = 195;
pub(crate) const STRING_LITERAL: Tt = 197;
pub(crate) const UUID: Tt = 200;
pub(crate) const NEQ: Tt = 207;
pub(crate) const PERCENT: Tt = 208;
pub(crate) const LPAREN: Tt = 209;
pub(crate) const RPAREN: Tt = 210;
pub(crate) const PLUS: Tt = 211;
pub(crate) const PLUS_EQ: Tt = 212;
pub(crate) const COMMA: Tt = 213;
pub(crate) const MINUS: Tt = 214;
pub(crate) const MINUS_EQ: Tt = 215;
pub(crate) const DOT: Tt = 216;
pub(crate) const SLASH: Tt = 217;
pub(crate) const COLON: Tt = 218;
pub(crate) const SEMI: Tt = 219;
pub(crate) const LT: Tt = 220;
pub(crate) const LTE: Tt = 221;
pub(crate) const EQ: Tt = 222;
pub(crate) const GT: Tt = 223;
pub(crate) const GTE: Tt = 224;
pub(crate) const LBRACKET: Tt = 225;
pub(crate) const STAR: Tt = 226;
pub(crate) const RBRACKET: Tt = 227;
pub(crate) const EXPR_LPAREN: Tt = 228;
pub(crate) const LBRACE: Tt = 229;
pub(crate) const RBRACE: Tt = 230;

// ScyllaDB-only keywords.
pub(crate) const K_BYPASS: Tt = 300;
pub(crate) const K_CACHE: Tt = 301;
pub(crate) const K_TIMEOUT: Tt = 302;
pub(crate) const K_PRUNE: Tt = 303;
pub(crate) const K_CONCURRENCY: Tt = 304;
pub(crate) const K_SERVICE: Tt = 305;
pub(crate) const K_LEVEL: Tt = 306;
pub(crate) const K_LEVELS: Tt = 307;
pub(crate) const K_ATTACH: Tt = 308;
pub(crate) const K_DETACH: Tt = 309;
pub(crate) const K_ATTACHED: Tt = 310;
pub(crate) const K_EFFECTIVE: Tt = 311;
pub(crate) const K_REDUCEFUNC: Tt = 312;
pub(crate) const K_SCYLLA_TIMEUUID_LIST_INDEX: Tt = 313;
pub(crate) const K_SCYLLA_COUNTER_SHARD_LIST: Tt = 314;
pub(crate) const K_SCYLLA_CLUSTERING_BOUND: Tt = 315;
pub(crate) const K_SERVICE_LEVEL: Tt = 316;
pub(crate) const K_PASSWORDS: Tt = 317;
pub(crate) const K_SERVICE_LEVELS: Tt = 318;
pub(crate) const K_LARGE_DATA_GUARDRAILS: Tt = 319;
pub(crate) const K_EMPTY: Tt = 320;

/// Which dialects know a keyword.
const C: u8 = 1;
const S: u8 = 2;
const CS: u8 = C | S;

fn dialect_bit(d: Dialect) -> u8 {
    match d {
        Dialect::Cassandra => C,
        Dialect::Scylla => S,
    }
}

/// Keywords by upper-case spelling. Sorted, for binary search.
static KEYWORDS: &[(&str, Tt, u8)] = &[
    ("ACCESS", K_ACCESS, CS),
    ("ADD", K_ADD, CS),
    ("AGGREGATE", K_AGGREGATE, CS),
    ("AGGREGATES", K_AGGREGATES, CS),
    ("ALL", K_ALL, CS),
    ("ALLOW", K_ALLOW, CS),
    ("ALTER", K_ALTER, CS),
    ("AND", K_AND, CS),
    ("ANN", K_ANN, CS),
    ("APPLY", K_APPLY, CS),
    ("AS", K_AS, CS),
    ("ASC", K_ASC, CS),
    ("ASCII", K_ASCII, CS),
    ("ATTACH", K_ATTACH, S),
    ("ATTACHED", K_ATTACHED, S),
    ("AUTHORIZE", K_AUTHORIZE, CS),
    ("BATCH", K_BATCH, CS),
    ("BEGIN", K_BEGIN, CS),
    ("BIGINT", K_BIGINT, CS),
    ("BLOB", K_BLOB, CS),
    ("BOOLEAN", K_BOOLEAN, CS),
    ("BY", K_BY, CS),
    ("BYPASS", K_BYPASS, S),
    ("CACHE", K_CACHE, S),
    ("CALLED", K_CALLED, CS),
    ("CAST", K_CAST, CS),
    ("CIDRS", K_CIDRS, C),
    ("CLUSTER", K_CLUSTER, CS),
    ("CLUSTERING", K_CLUSTERING, CS),
    ("COLUMNFAMILIES", K_TABLES, CS),
    ("COLUMNFAMILY", K_COLUMNFAMILY, CS),
    ("COMPACT", K_COMPACT, CS),
    ("CONCURRENCY", K_CONCURRENCY, S),
    ("CONTAINS", K_CONTAINS, CS),
    ("COUNT", K_COUNT, CS),
    ("COUNTER", K_COUNTER, CS),
    ("CREATE", K_CREATE, CS),
    ("CUSTOM", K_CUSTOM, CS),
    ("DATACENTERS", K_DATACENTERS, CS),
    ("DATE", K_DATE, CS),
    ("DECIMAL", K_DECIMAL, CS),
    ("DEFAULT", K_DEFAULT, CS),
    ("DELETE", K_DELETE, CS),
    ("DESC", K_DESC, CS),
    ("DESCRIBE", K_DESCRIBE, CS),
    ("DETACH", K_DETACH, S),
    ("DISTINCT", K_DISTINCT, CS),
    ("DOUBLE", K_DOUBLE, CS),
    ("DROP", K_DROP, CS),
    ("DURATION", K_DURATION, CS),
    ("EFFECTIVE", K_EFFECTIVE, S),
    ("EMPTY", K_EMPTY, S),
    ("ENTRIES", K_ENTRIES, CS),
    ("EXECUTE", K_EXECUTE, CS),
    ("EXISTS", K_EXISTS, CS),
    ("FILTERING", K_FILTERING, CS),
    ("FINALFUNC", K_FINALFUNC, CS),
    ("FLOAT", K_FLOAT, CS),
    ("FROM", K_FROM, CS),
    ("FROZEN", K_FROZEN, CS),
    ("FULL", K_FULL, CS),
    ("FUNCTION", K_FUNCTION, CS),
    ("FUNCTIONS", K_FUNCTIONS, CS),
    ("GRANT", K_GRANT, CS),
    ("GROUP", K_GROUP, CS),
    ("HASHED", K_HASHED, CS),
    ("IDENTITY", K_IDENTITY, C),
    ("IF", K_IF, CS),
    ("IN", K_IN, CS),
    ("INDEX", K_INDEX, CS),
    ("INET", K_INET, CS),
    ("INFINITY", K_POSITIVE_INFINITY, CS),
    ("INITCOND", K_INITCOND, CS),
    ("INPUT", K_INPUT, CS),
    ("INSERT", K_INSERT, CS),
    ("INT", K_INT, CS),
    ("INTERNALS", K_INTERNALS, CS),
    ("INTO", K_INTO, CS),
    ("IS", K_IS, CS),
    ("JSON", K_JSON, CS),
    ("KEY", K_KEY, CS),
    ("KEYS", K_KEYS, CS),
    ("KEYSPACE", K_KEYSPACE, CS),
    ("KEYSPACES", K_KEYSPACES, CS),
    ("LANGUAGE", K_LANGUAGE, CS),
    ("LARGE_DATA_GUARDRAILS", K_LARGE_DATA_GUARDRAILS, S),
    ("LEVEL", K_LEVEL, S),
    ("LEVELS", K_LEVELS, S),
    ("LIKE", K_LIKE, CS),
    ("LIMIT", K_LIMIT, CS),
    ("LIST", K_LIST, CS),
    ("LOGIN", K_LOGIN, CS),
    ("MAP", K_MAP, CS),
    ("MASKED", K_MASKED, C),
    ("MATERIALIZED", K_MATERIALIZED, CS),
    ("MAXWRITETIME", K_MAXWRITETIME, C),
    ("MBEAN", K_MBEAN, CS),
    ("MBEANS", K_MBEANS, CS),
    ("MODIFY", K_MODIFY, CS),
    ("NAN", K_POSITIVE_NAN, CS),
    ("NOLOGIN", K_NOLOGIN, CS),
    ("NORECURSIVE", K_NORECURSIVE, CS),
    ("NOSUPERUSER", K_NOSUPERUSER, CS),
    ("NOT", K_NOT, CS),
    ("NULL", K_NULL, CS),
    ("OF", K_OF, CS),
    ("ON", K_ON, CS),
    ("ONLY", K_ONLY, CS),
    ("OPTIONS", K_OPTIONS, CS),
    ("OR", K_OR, CS),
    ("ORDER", K_ORDER, CS),
    ("PARTITION", K_PARTITION, CS),
    ("PASSWORD", K_PASSWORD, CS),
    ("PASSWORDS", K_PASSWORDS, S),
    ("PER", K_PER, CS),
    ("PERMISSION", K_PERMISSION, CS),
    ("PERMISSIONS", K_PERMISSIONS, CS),
    ("PRIMARY", K_PRIMARY, CS),
    ("PRUNE", K_PRUNE, S),
    ("REDUCEFUNC", K_REDUCEFUNC, S),
    ("RENAME", K_RENAME, CS),
    ("REPLACE", K_REPLACE, CS),
    ("RETURNS", K_RETURNS, CS),
    ("REVOKE", K_REVOKE, CS),
    ("ROLE", K_ROLE, CS),
    ("ROLES", K_ROLES, CS),
    ("SCHEMA", K_SCHEMA, CS),
    ("SCYLLA_CLUSTERING_BOUND", K_SCYLLA_CLUSTERING_BOUND, S),
    ("SCYLLA_COUNTER_SHARD_LIST", K_SCYLLA_COUNTER_SHARD_LIST, S),
    ("SCYLLA_TIMEUUID_LIST_INDEX", K_SCYLLA_TIMEUUID_LIST_INDEX, S),
    ("SELECT", K_SELECT, CS),
    ("SELECT_MASKED", K_SELECT_MASKED, C),
    ("SERVICE", K_SERVICE, S),
    ("SERVICE_LEVEL", K_SERVICE_LEVEL, S),
    ("SERVICE_LEVELS", K_SERVICE_LEVELS, S),
    ("SET", K_SET, CS),
    ("SFUNC", K_SFUNC, CS),
    ("SMALLINT", K_SMALLINT, CS),
    ("STATIC", K_STATIC, CS),
    ("STORAGE", K_STORAGE, CS),
    ("STYPE", K_STYPE, CS),
    ("SUPERUSER", K_SUPERUSER, CS),
    ("TABLE", K_COLUMNFAMILY, CS),
    ("TABLES", K_TABLES, CS),
    ("TEXT", K_TEXT, CS),
    ("TIME", K_TIME, CS),
    ("TIMEOUT", K_TIMEOUT, S),
    ("TIMESTAMP", K_TIMESTAMP, CS),
    ("TIMEUUID", K_TIMEUUID, CS),
    ("TINYINT", K_TINYINT, CS),
    ("TO", K_TO, CS),
    ("TOKEN", K_TOKEN, CS),
    ("TRIGGER", K_TRIGGER, CS),
    ("TRUNCATE", K_TRUNCATE, CS),
    ("TTL", K_TTL, CS),
    ("TUPLE", K_TUPLE, CS),
    ("TYPE", K_TYPE, CS),
    ("TYPES", K_TYPES, CS),
    ("UNLOGGED", K_UNLOGGED, CS),
    ("UNMASK", K_UNMASK, C),
    ("UNSET", K_UNSET, CS),
    ("UPDATE", K_UPDATE, CS),
    ("USE", K_USE, CS),
    ("USER", K_USER, CS),
    ("USERS", K_USERS, CS),
    ("USING", K_USING, CS),
    ("UUID", K_UUID, CS),
    ("VALUES", K_VALUES, CS),
    ("VARCHAR", K_VARCHAR, CS),
    ("VARINT", K_VARINT, CS),
    ("VECTOR", K_VECTOR, CS),
    ("VIEW", K_VIEW, CS),
    ("WHERE", K_WHERE, CS),
    ("WITH", K_WITH, CS),
    ("WRITETIME", K_WRITETIME, CS),
];

/// The keyword spelled `word` (any case) in `dialect`, if any.
pub(crate) fn keyword(word: &str, dialect: Dialect) -> Option<Tt> {
    let b = word.as_bytes();
    if b.len() > 26 || b.len() < 2 {
        return None;
    }
    let mut buf = [0u8; 26];
    for (i, c) in b.iter().enumerate() {
        buf[i] = c.to_ascii_uppercase();
    }
    let up = &buf[..b.len()];
    let bit = dialect_bit(dialect);
    KEYWORDS
        .binary_search_by(|(k, _, _)| k.as_bytes().cmp(up))
        .ok()
        .map(|i| KEYWORDS[i])
        .filter(|(_, _, d)| d & bit != 0)
        .map(|(_, t, _)| t)
}

/// Every keyword of a dialect, upper case (for tests and identifier quoting).
pub fn keywords(dialect: Dialect) -> impl Iterator<Item = &'static str> {
    let bit = dialect_bit(dialect);
    KEYWORDS.iter().filter(move |(_, _, d)| d & bit != 0).map(|(k, _, _)| *k)
}

/// Cassandra's `reserved_keywords.txt`: words `ColumnIdentifier.maybeQuote` quotes.
pub(crate) fn is_reserved_word(s: &str) -> bool {
    const RESERVED: &[&str] = &[
        "ADD", "ALLOW", "ALTER", "AND", "APPLY", "ASC", "AUTHORIZE", "BATCH", "BEGIN", "BY", "COLUMNFAMILY", "CREATE",
        "DELETE", "DESC", "DESCRIBE", "DROP", "ENTRIES", "EXECUTE", "FROM", "FULL", "GRANT", "IF", "IN", "INDEX",
        "INFINITY", "INSERT", "INTO", "IS", "KEYSPACE", "LIMIT", "MATERIALIZED", "MODIFY", "NAN", "NORECURSIVE", "NOT",
        "NULL", "OF", "ON", "OR", "ORDER", "PRIMARY", "RENAME", "REVOKE", "SCHEMA", "SELECT", "SET", "TABLE", "TO",
        "TOKEN", "TRUNCATE", "UNLOGGED", "UPDATE", "USE", "USING", "VIEW", "WHERE", "WITH",
    ];
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(s))
}

/// The native type keywords (`native_type` in `Parser.g`).
pub(crate) fn is_native_type(t: Tt) -> bool {
    matches!(
        t,
        K_ASCII
            | K_BIGINT
            | K_BLOB
            | K_BOOLEAN
            | K_COUNTER
            | K_DECIMAL
            | K_DOUBLE
            | K_DURATION
            | K_FLOAT
            | K_INET
            | K_INT
            | K_SMALLINT
            | K_TEXT
            | K_TIMESTAMP
            | K_TINYINT
            | K_UUID
            | K_VARCHAR
            | K_VARINT
            | K_TIMEUUID
            | K_DATE
            | K_TIME
    )
}

/// `basic_unreserved_keyword` in Cassandra's `Parser.g`.
fn is_basic_unreserved_cassandra(t: Tt) -> bool {
    matches!(
        t,
        K_KEYS
            | K_AS
            | K_CLUSTER
            | K_CLUSTERING
            | K_COMPACT
            | K_STORAGE
            | K_TABLES
            | K_TYPE
            | K_TYPES
            | K_VALUES
            | K_MAP
            | K_LIST
            | K_FILTERING
            | K_PERMISSION
            | K_PERMISSIONS
            | K_KEYSPACES
            | K_ALL
            | K_USER
            | K_USERS
            | K_ROLE
            | K_ROLES
            | K_IDENTITY
            | K_SUPERUSER
            | K_NOSUPERUSER
            | K_LOGIN
            | K_NOLOGIN
            | K_OPTIONS
            | K_PASSWORD
            | K_HASHED
            | K_EXISTS
            | K_CUSTOM
            | K_TRIGGER
            | K_CONTAINS
            | K_INTERNALS
            | K_ONLY
            | K_STATIC
            | K_FROZEN
            | K_TUPLE
            | K_FUNCTION
            | K_FUNCTIONS
            | K_AGGREGATE
            | K_AGGREGATES
            | K_SFUNC
            | K_STYPE
            | K_FINALFUNC
            | K_INITCOND
            | K_RETURNS
            | K_LANGUAGE
            | K_CALLED
            | K_INPUT
            | K_LIKE
            | K_PER
            | K_PARTITION
            | K_GROUP
            | K_DATACENTERS
            | K_CIDRS
            | K_ACCESS
            | K_DEFAULT
            | K_MBEAN
            | K_MBEANS
            | K_REPLACE
            | K_UNSET
            | K_MASKED
            | K_UNMASK
            | K_SELECT_MASKED
            | K_VECTOR
            | K_ANN
    )
}

/// The unreserved keywords of ScyllaDB (probed on the reference image): Cassandra's set without
/// `ANN`, `DEFAULT` and `UNSET` (reserved there), with `DESC`, `DESCRIBE` and `EXECUTE` (reserved
/// in Cassandra) and ScyllaDB's own keywords except `CONCURRENCY`.
fn is_basic_unreserved_scylla(t: Tt) -> bool {
    (is_basic_unreserved_cassandra(t) && !matches!(t, K_ANN | K_DEFAULT | K_UNSET | K_REPLACE))
        || matches!(
            t,
            K_JSON
                | K_DISTINCT
                | K_DESC
                | K_DESCRIBE
                | K_EXECUTE
                | K_BYPASS
                | K_CACHE
                | K_TIMEOUT
                | K_PRUNE
                | K_SERVICE
                | K_SERVICE_LEVEL
                | K_SERVICE_LEVELS
                | K_LEVEL
                | K_LEVELS
                | K_ATTACH
                | K_DETACH
                | K_ATTACHED
                | K_EFFECTIVE
                | K_REDUCEFUNC
                | K_PASSWORDS
                | K_LARGE_DATA_GUARDRAILS
        )
}

pub(crate) fn is_basic_unreserved(t: Tt, d: Dialect) -> bool {
    match d {
        Dialect::Cassandra => is_basic_unreserved_cassandra(t),
        Dialect::Scylla => is_basic_unreserved_scylla(t),
    }
}

/// `unreserved_function_keyword`: basic unreserved keywords and native type names.
pub(crate) fn is_unreserved_function_keyword(t: Tt, d: Dialect) -> bool {
    is_basic_unreserved(t, d) || is_native_type(t) || t == K_EMPTY
}

/// `unreserved_keyword`: usable wherever an identifier is.
pub(crate) fn is_unreserved(t: Tt, d: Dialect) -> bool {
    is_unreserved_function_keyword(t, d)
        || matches!(t, K_TTL | K_COUNT | K_WRITETIME | K_MAXWRITETIME | K_KEY | K_JSON | K_DISTINCT)
        || t == K_EMPTY
        || (t == K_CAST && d == Dialect::Cassandra)
}

/// Whether `t` can start an identifier (`ident`, `cident` without the empty name).
pub(crate) fn is_ident(t: Tt, d: Dialect) -> bool {
    t == IDENT || t == QUOTED_NAME || is_unreserved(t, d)
}

/// The ANTLR token name used in "expecting X" messages.
pub(crate) fn token_name(t: Tt) -> &'static str {
    match t {
        EOF => "EOF",
        BOOLEAN => "BOOLEAN",
        DURATION => "DURATION",
        EMPTY_QUOTED_NAME => "EMPTY_QUOTED_NAME",
        FLOAT => "FLOAT",
        HEXNUMBER => "HEXNUMBER",
        IDENT => "IDENT",
        INTEGER => "INTEGER",
        QMARK => "QMARK",
        QUOTED_NAME => "QUOTED_NAME",
        RANGE => "RANGE",
        STRING_LITERAL => "STRING_LITERAL",
        UUID => "UUID",
        NEQ => "'!='",
        PERCENT => "'%'",
        LPAREN => "'('",
        RPAREN => "')'",
        PLUS => "'+'",
        PLUS_EQ => "'+='",
        COMMA => "','",
        MINUS => "'-'",
        MINUS_EQ => "'-='",
        DOT => "'.'",
        SLASH => "'/'",
        COLON => "':'",
        SEMI => "';'",
        LT => "'<'",
        LTE => "'<='",
        EQ => "'='",
        GT => "'>'",
        GTE => "'>='",
        LBRACKET => "'['",
        STAR => "'\\*'",
        RBRACKET => "']'",
        EXPR_LPAREN => "'expr('",
        LBRACE => "'{'",
        RBRACE => "'}'",
        K_NEGATIVE_INFINITY => "K_NEGATIVE_INFINITY",
        K_NEGATIVE_NAN => "K_NEGATIVE_NAN",
        K_POSITIVE_INFINITY => "K_POSITIVE_INFINITY",
        K_POSITIVE_NAN => "K_POSITIVE_NAN",
        K_COLUMNFAMILY => "K_COLUMNFAMILY",
        K_TABLES => "K_TABLES",
        _ => KEYWORDS
            .iter()
            .find(|(_, tt, _)| *tt == t)
            .map(|(k, _, _)| keyword_token_name(k))
            .unwrap_or("<invalid>"),
    }
}

/// `K_` + the keyword, as a static string.
fn keyword_token_name(k: &str) -> &'static str {
    macro_rules! names {
        ($($k:literal),* $(,)?) => {
            match k {
                $($k => concat!("K_", $k),)*
                _ => "<invalid>",
            }
        };
    }
    names!(
        "ACCESS", "ADD", "AGGREGATE", "AGGREGATES", "ALL", "ALLOW", "ALTER", "AND", "ANN", "APPLY", "AS", "ASC",
        "ASCII", "ATTACH", "ATTACHED", "AUTHORIZE", "BATCH", "BEGIN", "BIGINT", "BLOB", "BOOLEAN", "BY", "BYPASS",
        "CACHE", "CALLED", "CAST", "CIDRS", "CLUSTER", "CLUSTERING", "COMPACT", "CONCURRENCY", "CONTAINS", "COUNT",
        "COUNTER", "CREATE", "CUSTOM", "DATACENTERS", "DATE", "DECIMAL", "DEFAULT", "DELETE", "DESC", "DESCRIBE",
        "DETACH", "DISTINCT", "DOUBLE", "DROP", "DURATION", "EFFECTIVE", "EMPTY", "ENTRIES", "EXECUTE", "EXISTS", "FILTERING",
        "FINALFUNC", "FLOAT", "FROM", "FROZEN", "FULL", "FUNCTION", "FUNCTIONS", "GRANT", "GROUP", "HASHED",
        "IDENTITY", "IF", "IN", "INDEX", "INET", "INITCOND", "INPUT", "INSERT", "INT", "INTERNALS", "INTO", "IS",
        "JSON", "KEY", "KEYS", "KEYSPACE", "KEYSPACES", "LANGUAGE", "LARGE_DATA_GUARDRAILS", "LEVEL", "LEVELS",
        "LIKE", "LIMIT", "LIST", "LOGIN", "MAP", "MASKED", "MATERIALIZED", "MAXWRITETIME", "MBEAN", "MBEANS",
        "MODIFY", "NOLOGIN", "NORECURSIVE", "NOSUPERUSER", "NOT", "NULL", "OF", "ON", "ONLY",
        "OPTIONS", "OR", "ORDER", "PARTITION", "PASSWORD", "PASSWORDS", "PER", "PERMISSION", "PERMISSIONS",
        "PRIMARY", "PRUNE", "REDUCEFUNC", "RENAME", "REPLACE", "RETURNS", "REVOKE", "ROLE", "ROLES", "SCHEMA",
        "SCYLLA_CLUSTERING_BOUND", "SCYLLA_COUNTER_SHARD_LIST", "SCYLLA_TIMEUUID_LIST_INDEX", "SELECT",
        "SELECT_MASKED", "SERVICE", "SERVICE_LEVEL", "SERVICE_LEVELS", "SET", "SFUNC", "SMALLINT", "STATIC", "STORAGE", "STYPE", "SUPERUSER", "TEXT",
        "TIME", "TIMEOUT", "TIMESTAMP", "TIMEUUID", "TINYINT", "TO", "TOKEN", "TRIGGER", "TRUNCATE", "TTL", "TUPLE",
        "TYPE", "TYPES", "UNLOGGED", "UNMASK", "UNSET", "UPDATE", "USE", "USER", "USERS", "USING", "UUID", "VALUES",
        "VARCHAR", "VARINT", "VECTOR", "VIEW", "WHERE", "WITH", "WRITETIME",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_table_is_sorted() {
        for w in KEYWORDS.windows(2) {
            assert!(w[0].0 < w[1].0, "{} >= {}", w[0].0, w[1].0);
        }
    }

    #[test]
    fn every_keyword_has_a_token_name() {
        for (k, t, _) in KEYWORDS {
            assert_ne!(token_name(*t), "<invalid>", "{k}");
        }
    }

    #[test]
    fn lookup_is_case_insensitive_and_dialect_aware() {
        assert_eq!(keyword("select", Dialect::Cassandra), Some(K_SELECT));
        assert_eq!(keyword("SeLeCt", Dialect::Scylla), Some(K_SELECT));
        assert_eq!(keyword("table", Dialect::Cassandra), Some(K_COLUMNFAMILY));
        assert_eq!(keyword("bypass", Dialect::Cassandra), None);
        assert_eq!(keyword("bypass", Dialect::Scylla), Some(K_BYPASS));
        assert_eq!(keyword("masked", Dialect::Scylla), None);
        assert_eq!(keyword("selectx", Dialect::Cassandra), None);
    }
}
