//! Validation of custom type strings (`'org.apache.cassandra.db.marshal.UTF8Type'`), which
//! Cassandra resolves while parsing (`CQL3Type.Custom` through `TypeParser.parse`). Only the
//! errors a parse can raise are reproduced: unknown classes and malformed parameter lists.

const PREFIX: &str = "org.apache.cassandra.db.marshal.";

/// The marshal classes of Cassandra 5.0 (simple ones, then parameterised ones).
const SIMPLE: &[&str] = &[
    "AsciiType",
    "BooleanType",
    "ByteType",
    "BytesType",
    "CounterColumnType",
    "DateType",
    "DecimalType",
    "DoubleType",
    "DurationType",
    "EmptyType",
    "FloatType",
    "InetAddressType",
    "Int32Type",
    "IntegerType",
    "LexicalUUIDType",
    "LongType",
    "ShortType",
    "SimpleDateType",
    "TimeType",
    "TimeUUIDType",
    "TimestampType",
    "UTF8Type",
    "UUIDType",
    "PartitionerDefinedOrder",
];

const PARAMETERISED: &[&str] = &[
    "ListType",
    "SetType",
    "MapType",
    "ReversedType",
    "FrozenType",
    "TupleType",
    "UserType",
    "CompositeType",
    "DynamicCompositeType",
    "ColumnToCollectionType",
    "VectorType",
];

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.' | '_' | '&')
}

fn qualify(name: &str) -> String {
    if name.contains('.') { name.to_owned() } else { format!("{PREFIX}{name}") }
}

/// The error Cassandra reports for `'s'` as a type, if any.
pub(crate) fn check(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let name: String = trimmed.chars().take_while(|c| is_identifier_char(*c)).collect();
    if name.is_empty() {
        return None;
    }
    let class = qualify(&name);
    let short = class.strip_prefix(PREFIX);
    let known_simple = short.is_some_and(|n| SIMPLE.contains(&n));
    let known_param = short.is_some_and(|n| PARAMETERISED.contains(&n));
    if !known_simple && !known_param {
        if class.starts_with("java.") || class.starts_with("javax.") {
            return Some(format!(
                "Error setting type {s}: Invalid abstract-type class '{class}': must extend or implement {PREFIX}AbstractType"
            ));
        }
        return Some(format!("Error setting type {s}: Unable to find abstract-type class '{class}'"));
    }
    let rest = trimmed[name.len()..].trim_start();
    if known_param && rest.starts_with('(') {
        let mut depth = 0i32;
        let mut closed = false;
        for c in rest.chars() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closed {
            return Some(format!("Error setting type {s}: Invalid definition for comparator {class}."));
        }
    }
    None
}

/// How Cassandra prints a custom type (`AbstractType.toString`): class names qualified.
pub(crate) fn display(s: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    for c in s.chars() {
        if is_identifier_char(c) {
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push_str(&qualify(&word));
                word.clear();
            }
            if !c.is_whitespace() {
                out.push(c);
            }
        }
    }
    if !word.is_empty() {
        out.push_str(&qualify(&word));
    }
    out
}
