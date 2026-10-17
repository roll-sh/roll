//! RediSearch's query errors (`QueryError`): a code with a prefix (`SEARCH_SYNTAX `) and the
//! message a client sees, which is the private message (prefix, message and any user data) or,
//! when none was set, the code's default text.

use crate::commands::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Generic,
    Syntax,
    ParseArgs,
    AddArgs,
    Expr,
    Keyword,
    NoResults,
    BadAttr,
    Inval,
    BuildPlan,
    ConstructPipeline,
    NoReducer,
    ReducerGeneric,
    AggPlan,
    CursorAlloc,
    ReducerInit,
    QString,
    NoPropKey,
    NoPropVal,
    NoDoc,
    NoOption,
    RedisKeyType,
    InvalPath,
    IndexExists,
    BadOption,
    BadOrderOption,
    Limit,
    NoIndex,
    DocExists,
    DocNotAdded,
    DupField,
    GeoFormat,
    NoDistribute,
    UnsuppType,
    TimedOut,
    NoParam,
    DupParam,
    BadVal,
    NonHybrid,
    HybridNonExist,
    AdhocWithBatchSize,
    AdhocWithEfRuntime,
    NonRange,
    Missing,
    Mismatch,
    DroppedBackground,
    AliasConflict,
    IndexBgOomFail,
    WeightNotAllowed,
    VectorNotAllowed,
    OutOfMemory,
    UnavailableSlots,
    FlexLimitNumberOfIndexes,
    FlexUnsupportedField,
    FlexUnsupportedFtCreateArgument,
    DiskCreation,
    VectorBlobSizeMismatch,
    VectorLenBad,
    NumericValueInvalid,
    ArgUnrecognized,
    GeoCoordinatesInvalid,
    JsonTypeBad,
    ClusterNoResponses,
}

impl Code {
    /// `(prefix, default message)`.
    pub const fn strings(self) -> (&'static str, &'static str) {
        use Code::*;
        match self {
            Generic => ("SEARCH_GENERIC ", "Generic error evaluating the query"),
            Syntax => ("SEARCH_SYNTAX ", "Parsing/Syntax error for query string"),
            ParseArgs => ("SEARCH_PARSE_ARGS ", "Error parsing query/aggregation arguments"),
            AddArgs => ("SEARCH_ADD_ARGS ", "Error parsing document indexing arguments"),
            Expr => ("SEARCH_EXPR ", "Parsing/Evaluating dynamic expression failed"),
            Keyword => ("SEARCH_KEYWORD ", "Could not handle query keyword"),
            NoResults => ("SEARCH_NO_RESULTS ", "Query matches no results"),
            BadAttr => ("SEARCH_ATTR_BAD ", "Attribute not supported for term"),
            Inval => ("SEARCH_QUERY_BAD ", "Could not validate the query nodes (bad attribute?)"),
            BuildPlan => ("SEARCH_BUILD_PLAN ", "Could not build plan from query"),
            ConstructPipeline => ("SEARCH_CONSTRUCT_PIPELINE ", "Could not construct query pipeline"),
            NoReducer => ("SEARCH_REDUCER_NOT_FOUND ", "Reducer not found"),
            ReducerGeneric => ("SEARCH_REDUCER_ERROR ", "Generic reducer error"),
            AggPlan => ("SEARCH_AGG_PLAN ", "Could not plan aggregation request"),
            CursorAlloc => ("SEARCH_CURSOR_ALLOC_FAILED ", "Could not allocate a cursor"),
            ReducerInit => ("SEARCH_REDUCER_INIT_FAILED ", "Could not initialize reducer"),
            QString => ("SEARCH_QUERY_STRING_BAD ", "Bad query string"),
            NoPropKey => ("SEARCH_PROP_NOT_FOUND ", "Property not loaded nor in pipeline"),
            NoPropVal => ("SEARCH_VALUE_NOT_FOUND ", "Value not found in result (not a hard error)"),
            NoDoc => ("SEARCH_DOC_NOT_FOUND ", "Document not found"),
            NoOption => ("SEARCH_OPTION_INVALID ", "Invalid option"),
            RedisKeyType => ("SEARCH_REDIS_KEY_TYPE_BAD ", "Invalid Redis key"),
            InvalPath => ("SEARCH_PATH_BAD ", "Invalid path"),
            IndexExists => ("SEARCH_INDEX_EXISTS ", "Index already exists"),
            BadOption => ("SEARCH_OPTION_BAD ", "Option not supported for current mode"),
            BadOrderOption => ("SEARCH_ORDER_OPTION_BAD ", "Path with undefined ordering does not support slop/inorder"),
            Limit => ("SEARCH_LIMIT_OVER ", "Limit exceeded"),
            NoIndex => ("SEARCH_INDEX_NOT_FOUND ", "Index not found"),
            DocExists => ("SEARCH_DOCUMENT_EXISTS ", "Document already exists"),
            DocNotAdded => ("SEARCH_DOCUMENT_NOT_ADDED ", "Document was not added because condition was unmet"),
            DupField => ("SEARCH_FIELD_DUP ", "Field was specified twice"),
            GeoFormat => ("SEARCH_GEO_FORMAT_BAD ", "Invalid lon/lat format. Use \"lon lat\" or \"lon,lat\""),
            NoDistribute => ("SEARCH_DIST_FAILED ", "Could not distribute the operation"),
            UnsuppType => ("SEARCH_TYPE_UNSUP ", "Unsupported index type"),
            TimedOut => ("SEARCH_TIMEOUT ", "Timeout limit was reached"),
            NoParam => ("SEARCH_PARAM_NOT_FOUND ", "Parameter not found"),
            DupParam => ("SEARCH_PARAM_DUP ", "Parameter was specified twice"),
            BadVal => ("SEARCH_VALUE_BAD ", "Invalid value was given"),
            NonHybrid => ("SEARCH_HYBRID_ATTR_NON_HYBRID ", "hybrid query attributes were sent for a non-hybrid query"),
            HybridNonExist => ("SEARCH_HYBRID_POLICY_BAD ", "invalid hybrid policy was given"),
            AdhocWithBatchSize => ("SEARCH_ADHOC_BATCH_SIZE_IRRELEVANT ", "'batch size' is irrelevant for the selected policy"),
            AdhocWithEfRuntime => ("SEARCH_ADHOC_EF_RUNTIME_IRRELEVANT ", "'EF_RUNTIME' is irrelevant for the selected policy"),
            NonRange => ("SEARCH_RANGE_ATTR_NON_RANGE ", "range query attributes were sent for a non-range query"),
            Missing => ("SEARCH_FIELD_MISSING_REQ ", "'ismissing' requires field to be defined with 'INDEXMISSING'"),
            Mismatch => ("SEARCH_INDEX_MISMATCH ", "Index mismatch: Shard index is different than queried index"),
            DroppedBackground => ("SEARCH_INDEX_DROPPED_BG ", "The index was dropped before the query could be executed"),
            AliasConflict => ("SEARCH_ALIAS_CONFLICT ", "Alias conflicts with an existing index name"),
            IndexBgOomFail => ("SEARCH_INDEX_BG_OOM_FAIL ", "Index background scan did not complete due to OOM"),
            WeightNotAllowed => ("SEARCH_WEIGHT_NOT_ALLOWED ", "Weight attributes are not allowed"),
            VectorNotAllowed => ("SEARCH_VECTOR_NOT_ALLOWED ", "Vector queries are not allowed"),
            OutOfMemory => ("SEARCH_OOM ", "Not enough memory available to execute the query"),
            UnavailableSlots => ("SEARCH_SLOTS_UNAVAIL ", "Query requires unavailable slots"),
            FlexLimitNumberOfIndexes => ("SEARCH_FLEX_LIMIT_NUMBER_OF_INDEXES ", "Flex index limit was reached"),
            FlexUnsupportedField => ("SEARCH_FLEX_UNSUPPORTED_FIELD ", "Unsupported field for Flex index"),
            FlexUnsupportedFtCreateArgument => ("SEARCH_FLEX_UNSUPPORTED_FT_CREATE_ARGUMENT ", "Unsupported FT.CREATE argument for Flex index"),
            DiskCreation => ("SEARCH_DISK_CREATION ", "Could not create disk index"),
            VectorBlobSizeMismatch => ("SEARCH_VECTOR_BLOB_SIZE_MISMATCH ", "Vector blob size does not match expected dimensions"),
            VectorLenBad => ("SEARCH_VECTOR_LEN_BAD ", "Invalid vector length"),
            NumericValueInvalid => ("SEARCH_NUMERIC_VALUE_INVALID ", "Invalid numeric value"),
            ArgUnrecognized => ("SEARCH_ARG_UNRECOGNIZED ", "Unknown argument"),
            GeoCoordinatesInvalid => ("SEARCH_GEO_COORDINATES_INVALID ", "Invalid geo coordinates"),
            JsonTypeBad => ("SEARCH_JSON_TYPE_INVALID ", "Invalid JSON type"),
            ClusterNoResponses => ("SEARCH_CLUSTER_NO_RESPONSES ", "no responses received"),
        }
    }

    pub const fn prefix(self) -> &'static str {
        self.strings().0
    }

    /// `QueryError_Strerror`: prefix and default message.
    pub fn strerror(self) -> String {
        let (p, m) = self.strings();
        format!("{p}{m}")
    }
}

/// A set `QueryError`: its code, the public (obfuscatable) message and the private one the
/// client gets.
#[derive(Clone, Debug, PartialEq)]
pub struct QErr {
    pub code: Code,
    pub public: Option<String>,
    pub private: Option<String>,
}

impl QErr {
    /// `QueryError_SetCode` / `QueryError_SetError(code, NULL)`: the default text.
    pub fn code(code: Code) -> Self {
        QErr { code, public: None, private: None }
    }

    /// `QueryError_SetError(code, message)`: the client sees prefix + message.
    pub fn new(code: Code, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        QErr { code, private: Some(format!("{}{msg}", code.prefix())), public: Some(msg) }
    }

    /// `QueryError_SetWithUserDataFmt(code, message, fmt…)`: user data after the message.
    pub fn user(code: Code, msg: &str, user: impl AsRef<str>) -> Self {
        QErr { code, public: Some(msg.to_string()), private: Some(format!("{}{msg}{}", code.prefix(), user.as_ref())) }
    }

    /// `QueryError_GetUserError`.
    pub fn text(&self) -> String {
        self.private.clone().unwrap_or_else(|| self.code.strerror())
    }

    /// `QueryError_GetDisplayableError(obfuscate)`.
    pub fn displayable(&self, obfuscate: bool) -> String {
        let m = if obfuscate { &self.public } else { &self.private };
        m.clone().unwrap_or_else(|| self.code.strerror())
    }

    /// `QERR_MKBADARGS_AC(name, rc)`.
    pub fn bad_args(name: &str, rc: super::args::AcErr) -> Self {
        QErr::user(Code::ParseArgs, "Bad arguments", format!(" for {name}: {}", rc.strerror()))
    }

    /// `QueryError_FmtUnknownArg`: the argument at the cursor (not consumed).
    pub fn unknown_arg(ac: &super::args::Ac, name: &str) -> Self {
        let s = ac.peek().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_else(|| "Unknown (FIXME)".into());
        let s = s.split('\0').next().unwrap_or("").to_string();
        QErr::user(Code::ArgUnrecognized, "Unknown argument", format!(" `{s}` at position {} for {name}", ac.offset))
    }
}

impl From<QErr> for Error {
    fn from(e: QErr) -> Error {
        Error::msg(e.text())
    }
}

pub type QResult<T> = Result<T, QErr>;
