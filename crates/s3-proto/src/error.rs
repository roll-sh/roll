//! S3 error codes and the XML error body.

use crate::xml::escape;
use std::fmt;

macro_rules! codes {
    ($($name:ident $status:literal $msg:literal,)*) => {
        /// An S3 error code, with its HTTP status and default message.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Code { $($name,)* }

        impl Code {
            pub fn as_str(self) -> &'static str {
                match self { $(Code::$name => stringify!($name),)* }
            }
            pub fn status(self) -> u16 {
                match self { $(Code::$name => $status,)* }
            }
            pub fn message(self) -> &'static str {
                match self { $(Code::$name => $msg,)* }
            }
            pub fn parse(s: &str) -> Option<Code> {
                match s { $(stringify!($name) => Some(Code::$name),)* _ => None }
            }
        }
    };
}

codes! {
    AccessDenied 403 "Access Denied",
    AccessForbidden 403 "CORSResponse: This CORS request is not allowed. This is usually because the evaluation of Origin, request method / Access-Control-Request-Method or Access-Control-Request-Headers are not whitelisted by the resource's CORS spec.",
    AuthorizationHeaderMalformed 400 "The authorization header is malformed.",
    AuthorizationQueryParametersError 400 "Error parsing the X-Amz-Credential parameter.",
    BadDigest 400 "The Content-MD5 or checksum value that you specified did not match what the server received.",
    BucketAlreadyExists 409 "The requested bucket name is not available. The bucket namespace is shared by all users of the system. Please select a different name and try again.",
    BucketAlreadyOwnedByYou 409 "Your previous request to create the named bucket succeeded and you already own it.",
    BucketNotEmpty 409 "The bucket you tried to delete is not empty.",
    ConditionalRequestConflict 409 "A conflicting conditional action is currently in progress against this resource.",
    EntityTooLarge 400 "Your proposed upload exceeds the maximum allowed object size.",
    EntityTooSmall 400 "Your proposed upload is smaller than the minimum allowed object size.",
    ExpiredToken 400 "The provided token has expired.",
    IncompleteBody 400 "You did not provide the number of bytes specified by the Content-Length HTTP header.",
    IncorrectNumberOfFilesInPostRequest 400 "POST requires exactly one file upload per request.",
    InternalError 500 "We encountered an internal error. Please try again.",
    InvalidAccessKeyId 403 "The AWS Access Key Id you provided does not exist in our records.",
    InvalidArgument 400 "Invalid Argument",
    InvalidBucketState 409 "The request is not valid with the current state of the bucket.",
    InvalidBucketName 400 "The specified bucket is not valid.",
    InvalidDigest 400 "The Content-MD5 or checksum value you specified is not valid.",
    InvalidLocationConstraint 400 "The specified location constraint is not valid.",
    InvalidObjectState 403 "The operation is not valid for the current state of the object.",
    InvalidPart 400 "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not match the part's entity tag.",
    InvalidPartOrder 400 "The list of parts was not in ascending order. Parts must be ordered by part number.",
    InvalidPolicyDocument 400 "The content of the form does not meet the conditions specified in the policy document.",
    InvalidRange 416 "The requested range is not satisfiable",
    InvalidRequest 400 "Invalid Request",
    InvalidStorageClass 400 "The storage class you specified is not valid.",
    InvalidTag 400 "The tag provided was not a valid tag.",
    KeyTooLongError 400 "Your key is too long.",
    MalformedPOSTRequest 400 "The body of your POST request is not well-formed multipart/form-data.",
    MalformedXML 400 "The XML you provided was not well-formed or did not validate against our published schema.",
    MaxMessageLengthExceeded 400 "Your request was too big.",
    MetadataTooLarge 400 "Your metadata headers exceed the maximum allowed metadata size.",
    MethodNotAllowed 405 "The specified method is not allowed against this resource.",
    MissingContentLength 411 "You must provide the Content-Length HTTP header.",
    MissingSecurityHeader 400 "Your request is missing a required header.",
    NoSuchBucket 404 "The specified bucket does not exist",
    NoSuchCORSConfiguration 404 "The CORS configuration does not exist",
    NoSuchKey 404 "The specified key does not exist.",
    NoSuchLifecycleConfiguration 404 "The lifecycle configuration does not exist",
    NoSuchObjectLockConfiguration 404 "The specified object does not have a ObjectLock configuration",
    NoSuchTagSet 404 "The TagSet does not exist",
    NoSuchUpload 404 "The specified upload does not exist. The upload ID may be invalid, or the upload may have been aborted or completed.",
    NotImplemented 501 "A header you provided implies functionality that is not implemented.",
    NoSuchVersion 404 "The specified version does not exist.",
    NotModified 304 "Not Modified",
    ObjectLockConfigurationNotFoundError 404 "Object Lock configuration does not exist for this bucket",
    PreconditionFailed 412 "At least one of the pre-conditions you specified did not hold",
    RequestTimeTooSkewed 403 "The difference between the request time and the server's time is too large.",
    RequestTimeout 400 "Your socket connection to the server was not read from or written to within the timeout period.",
    ServiceUnavailable 503 "Service is unable to handle request.",
    SignatureDoesNotMatch 403 "The request signature we calculated does not match the signature you provided. Check your key and signing method.",
    SlowDown 503 "Please reduce your request rate.",
    XAmzContentSHA256Mismatch 400 "The provided 'x-amz-content-sha256' header does not match what was computed.",
    // Bucket configuration, policies, ACLs and websites.
    AccessControlListNotSupported 400 "The bucket does not allow ACLs",
    InvalidBucketAclWithObjectOwnership 400 "Bucket cannot have ACLs set with ObjectOwnership's BucketOwnerEnforced setting",
    InvalidRedirectLocation 400 "The website redirect location must have a prefix of 'http://' or 'https://' or '/'.",
    InvalidTargetBucketForLogging 400 "The target bucket for logging does not exist",
    MalformedACLError 400 "The XML you provided was not well-formed or did not validate against our published schema",
    MalformedPolicy 400 "Policies must be valid JSON and the first byte must be '{'",
    NoSuchBucketPolicy 404 "The bucket policy does not exist",
    NoSuchConfiguration 404 "The specified configuration does not exist.",
    NoSuchPublicAccessBlockConfiguration 404 "The public access block configuration was not found",
    NoSuchWebsiteConfiguration 404 "The specified bucket does not have a website configuration",
    OwnershipControlsNotFoundError 404 "The bucket ownership controls were not found",
    ReplicationConfigurationNotFoundError 404 "The replication configuration was not found",
    TooManyConfigurations 400 "You are attempting to create a new configuration but have already reached the 1,000-configuration limit.",
    UnresolvableGrantByEmailAddress 400 "The email address you provided does not match any account on record.",
}

/// An S3 error response.
#[derive(Clone, Debug)]
pub struct S3Error {
    pub code: Code,
    pub message: String,
    /// `<Resource>`: the bucket or key path the request was about.
    pub resource: Option<String>,
    /// Extra elements, e.g. `StringToSign` for signature mismatches.
    pub extra: Vec<(&'static str, String)>,
    /// Response headers sent with the error (`x-amz-delete-marker`, `x-amz-version-id`, …).
    pub headers: Vec<(&'static str, String)>,
}

impl S3Error {
    pub fn new(code: Code) -> Self {
        Self { code, message: code.message().to_string(), resource: None, extra: Vec::new(), headers: Vec::new() }
    }

    pub fn msg(code: Code, message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Self::new(code) }
    }

    pub fn with_resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = Some(resource.into());
        self
    }

    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.extra.push((name, value.into()));
        self
    }

    /// Send a response header with the error.
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// The XML body (not sent for HEAD requests or 304s).
    pub fn to_xml(&self, request_id: &str) -> String {
        let mut s = String::with_capacity(256);
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>");
        s.push_str(self.code.as_str());
        s.push_str("</Code><Message>");
        s.push_str(&escape(&self.message));
        s.push_str("</Message>");
        if let Some(r) = &self.resource {
            s.push_str("<Resource>");
            s.push_str(&escape(r));
            s.push_str("</Resource>");
        }
        for (k, v) in &self.extra {
            s.push_str(&format!("<{k}>{}</{k}>", escape(v)));
        }
        s.push_str("<RequestId>");
        s.push_str(&escape(request_id));
        s.push_str("</RequestId></Error>");
        s
    }
}

impl From<Code> for S3Error {
    fn from(c: Code) -> Self {
        S3Error::new(c)
    }
}

impl fmt::Display for S3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for S3Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_body() {
        let e = S3Error::new(Code::NoSuchKey).with_resource("/b/a<b>").with("Key", "a<b>");
        assert_eq!(e.status(), 404);
        assert_eq!(
            e.to_xml("req1"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message><Resource>/b/a&lt;b&gt;</Resource><Key>a&lt;b&gt;</Key><RequestId>req1</RequestId></Error>"
        );
        assert_eq!(Code::parse("InvalidRange"), Some(Code::InvalidRange));
        assert_eq!(Code::InvalidRange.status(), 416);
        assert_eq!(Code::NotModified.status(), 304);
    }

    #[test]
    fn codes_statuses_and_bodies() {
        for (c, s) in [
            (Code::MethodNotAllowed, 405),
            (Code::MissingContentLength, 411),
            (Code::ServiceUnavailable, 503),
            (Code::SlowDown, 503),
            (Code::InternalError, 500),
            (Code::NotImplemented, 501),
            (Code::PreconditionFailed, 412),
            (Code::NoSuchUpload, 404),
            (Code::NoSuchBucket, 404),
            (Code::BucketNotEmpty, 409),
            (Code::SignatureDoesNotMatch, 403),
            (Code::RequestTimeTooSkewed, 403),
            (Code::InvalidAccessKeyId, 403),
            (Code::AccessForbidden, 403),
            (Code::KeyTooLongError, 400),
            (Code::EntityTooSmall, 400),
            (Code::XAmzContentSHA256Mismatch, 400),
        ] {
            assert_eq!(c.status(), s, "{c:?}");
            assert_eq!(Code::parse(c.as_str()), Some(c));
            assert!(!c.message().is_empty());
        }
        assert_eq!(Code::parse("nosuchkey"), None);
        let e: S3Error = Code::SlowDown.into();
        assert_eq!(e.message, Code::SlowDown.message());
        assert_eq!(e.to_string(), "SlowDown: Please reduce your request rate.");
        let e = S3Error::msg(Code::InvalidArgument, "bad \"x\" & <y>").with("ArgumentName", "a&b");
        let x = e.to_xml("r&1");
        assert!(!x.contains("<Resource>"));
        let n = crate::xml::parse(x.as_bytes()).unwrap();
        assert_eq!((n.name.as_str(), n.text_of("Code"), n.text_of("Message"), n.text_of("ArgumentName"), n.text_of("RequestId")), ("Error", Some("InvalidArgument"), Some("bad \"x\" & <y>"), Some("a&b"), Some("r&1")));
    }
}
