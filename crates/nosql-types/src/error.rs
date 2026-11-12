//! The one error type: a message exactly as the reference prints it.

/// A value that does not fit its type: malformed bytes, a bad literal, an impossible cast or a
/// JSON value of the wrong shape.
///
/// `message` is the reference's text for the dialect it was produced for. ScyllaDB's marshal
/// errors carry their `marshaling error: ` prefix; when ScyllaDB reports a bad bound value it
/// adds `Exception while binding column <c>: ` in front of exactly those (messages without the
/// prefix, such as `Invalid NULL element in list`, are sent as is). Cassandra adds no context.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MarshalError {
    pub message: String,
}

impl MarshalError {
    pub fn new(message: impl Into<String>) -> Self {
        MarshalError {
            message: message.into(),
        }
    }

    /// ScyllaDB's `marshaling error: …`.
    pub fn scylla(message: impl std::fmt::Display) -> Self {
        MarshalError {
            message: format!("marshaling error: {message}"),
        }
    }
}

pub type Result<T, E = MarshalError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors() {
        assert_eq!(MarshalError::new("x").to_string(), "x");
        assert_eq!(
            MarshalError::scylla("Value 1 out of range").message,
            "marshaling error: Value 1 out of range"
        );
    }
}
