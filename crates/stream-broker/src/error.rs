use kafka_protocol::ResponseError;
use kafka_protocol::protocol::StrBytes;

/// A Kafka error code with an optional message, used by both the wire protocol and the HTTP API.
#[derive(Clone, Debug, PartialEq)]
pub struct KafkaError {
    pub code: ResponseError,
    pub msg: Option<String>,
}

impl KafkaError {
    pub fn new(code: ResponseError, msg: impl Into<String>) -> Self {
        Self { code, msg: Some(msg.into()) }
    }
    pub fn code(code: ResponseError) -> Self {
        Self { code, msg: None }
    }
    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(ResponseError::InvalidRequest, msg)
    }
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(ResponseError::ResourceNotFound, format!("{} not found", what.into()))
    }
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::new(ResponseError::UnknownServerError, e.to_string())
    }
    pub fn storage(e: impl std::fmt::Display) -> Self {
        Self::new(ResponseError::KafkaStorageError, e.to_string())
    }
    pub fn as_i16(&self) -> i16 {
        self.code.code()
    }
    pub fn message(&self) -> String {
        self.msg.clone().unwrap_or_else(|| self.code.to_string())
    }
    pub fn str_message(&self) -> Option<StrBytes> {
        Some(StrBytes::from_string(self.message()))
    }
    /// HTTP status for the control-plane API.
    pub fn http_status(&self) -> u16 {
        use ResponseError::*;
        match self.code {
            ResourceNotFound | UnknownTopicOrPartition | UnknownTopicId | GroupIdNotFound => 404,
            TopicAlreadyExists | DuplicateResource | NonEmptyGroup => 409,
            UnknownServerError | KafkaStorageError => 500,
            _ => 400,
        }
    }
}

impl std::fmt::Display for KafkaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for KafkaError {}

pub fn code(e: ResponseError) -> i16 {
    e.code()
}
