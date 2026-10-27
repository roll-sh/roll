//! Errors carry a Postgres SQLSTATE, message and optional detail/hint, exactly as clients see them.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<Box<str>>,
    /// 1-based character offset into the query, for syntax errors.
    pub position: Option<u32>,
    pub severity: Severity,
    /// Where it happened (CONTEXT: "COPY t, line 3", PL/pgSQL call stacks).
    pub context: Option<Box<str>>,
    /// The database objects it concerns (SCHEMA / TABLE / COLUMN / DATATYPE / CONSTRAINT NAME).
    pub objects: Option<Box<Objects>>,
    /// `context` already lists the frames it was raised in (`in_frames`).
    pub stacked: bool,
}

/// What was running, for CONTEXT lines (an entry of `error_context_stack`).
#[derive(Debug, Clone)]
pub enum Frame {
    /// A PL/pgSQL function (its signature) and where in it (`plpgsql_exec_error_callback`).
    Pl { sig: String, at: PlAt },
    /// A query run on behalf of PL/pgSQL or a trigger (`_SPI_error_callback`): its kind
    /// ("SQL statement", "PL/pgSQL expression", "PL/pgSQL assignment") and text. An error
    /// with a position gets it as its internal query instead; `shift` takes the position
    /// (a byte offset into the text bound) to one into `text`.
    Query {
        kind: &'static str,
        text: String,
        shift: isize,
    },
    /// A SQL function (`sql_exec_error_callback`, `sql_inline_error_callback`,
    /// `sql_function_parse_error_callback`): its name, source, and what of it is running.
    /// An error with a position gets the source as its internal query.
    SqlFn {
        name: std::sync::Arc<str>,
        src: std::sync::Arc<str>,
        at: SqlFnAt,
    },
    /// Anything else, as its line ("COPY t, line 2: ...").
    Text(String),
    /// COPY FROM's row being inserted (`CopyFromErrorCallback`): the table, the line number
    /// and its text (None: left out, an error found after the line was buffered). Made into
    /// text only when shown.
    Copy {
        table: std::sync::Arc<str>,
        line: usize,
        raw: Option<String>,
    },
}

/// What of a SQL function is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlFnAt {
    /// Its body being checked (CREATE FUNCTION).
    Checked,
    /// Statement N (1-based) being prepared or run.
    Statement(usize),
    /// The planner putting its body in place of the call.
    Inlining,
    /// Getting ready to run (its body parsed).
    Startup,
}

/// Where a PL/pgSQL function is.
#[derive(Debug, Clone, Default)]
pub enum PlAt {
    /// Nowhere in particular (before its body runs, after it ends).
    #[default]
    Function,
    /// Running a statement: its line and kind ("RAISE", "SQL statement", ...).
    Stmt(u32, &'static str),
    /// Doing something described by text, at a statement's line or not.
    Text(Option<u32>, &'static str),
}

impl Frame {
    /// The CONTEXT line.
    pub fn line(&self) -> String {
        match self {
            Frame::Pl {
                sig,
                at: PlAt::Function,
            } => format!("PL/pgSQL function {sig}"),
            Frame::Pl {
                sig,
                at: PlAt::Stmt(line, kind),
            } => format!("PL/pgSQL function {sig} line {line} at {kind}"),
            Frame::Pl {
                sig,
                at: PlAt::Text(Some(line), text),
            } => format!("PL/pgSQL function {sig} line {line} {text}"),
            Frame::Pl {
                sig,
                at: PlAt::Text(None, text),
            } => format!("PL/pgSQL function {sig} {text}"),
            Frame::Query { kind, text, .. } => format!("{kind} \"{text}\""),
            Frame::SqlFn {
                name,
                at: SqlFnAt::Checked,
                ..
            } => format!("SQL function \"{name}\""),
            Frame::SqlFn {
                name,
                at: SqlFnAt::Statement(n),
                ..
            } => format!("SQL function \"{name}\" statement {n}"),
            Frame::SqlFn {
                name,
                at: SqlFnAt::Inlining,
                ..
            } => format!("SQL function \"{name}\" during inlining"),
            Frame::SqlFn {
                name,
                at: SqlFnAt::Startup,
                ..
            } => format!("SQL function \"{name}\" during startup"),
            Frame::Text(t) => t.clone(),
            Frame::Copy {
                table,
                line,
                raw: Some(raw),
            } => format!("COPY {table}, line {line}: \"{raw}\""),
            Frame::Copy { table, line, raw: None } => format!("COPY {table}, line {line}"),
        }
    }
}

/// CONTEXT lines for a frame stack (outermost first), innermost first.
pub fn frames_text(frames: &[Frame]) -> Option<String> {
    (!frames.is_empty()).then(|| {
        frames
            .iter()
            .rev()
            .map(Frame::line)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// A byte offset into `text` (clamped to a character boundary) as a 1-based character position.
fn char_position(text: &str, byte: isize) -> u32 {
    let mut b = byte.clamp(0, text.len() as isize) as usize;
    while !text.is_char_boundary(b) {
        b -= 1;
    }
    text[..b].chars().count() as u32 + 1
}

/// The object fields of an error (`errtable`, `errtablecol`, `errdatatype`, `errconstraint`),
/// and its internal query: fields few errors have.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Objects {
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub datatype: Option<String>,
    pub constraint: Option<String>,
    /// A query run on the client's behalf that the error's position points into (a
    /// PL/pgSQL expression): the 1-based character position and the query.
    pub internal: Option<(u32, String)>,
    /// The ROUTINE field, where clients read it to tell errors apart (asyncpg and pgjdbc
    /// recognise a plan invalidated by DDL by RevalidateCachedQuery).
    pub routine: Option<&'static str>,
}

/// How bad it is: the `S`/`V` field of an ErrorResponse or NoticeResponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Fatal,
    Warning,
    Notice,
    Info,
    Debug,
    Log,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Fatal => "FATAL",
            Severity::Warning => "WARNING",
            Severity::Notice => "NOTICE",
            Severity::Info => "INFO",
            Severity::Debug => "DEBUG",
            Severity::Log => "LOG",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            hint: None,
            position: None,
            severity: Severity::Error,
            context: None,
            objects: None,
            stacked: false,
        }
    }
    fn objects_mut(&mut self) -> &mut Objects {
        self.objects.get_or_insert_with(Default::default)
    }
    /// The source function Postgres reports raising it (the ROUTINE field).
    pub fn routine(mut self, r: &'static str) -> Self {
        self.objects_mut().routine = Some(r);
        self
    }
    /// `errtable`: the table (and its schema) it concerns.
    pub fn table(mut self, schema: &str, table: &str) -> Self {
        let o = self.objects_mut();
        o.schema = Some(schema.to_string());
        o.table = Some(table.to_string());
        self
    }
    /// `errtablecol`: a column of the table.
    pub fn column(mut self, column: &str) -> Self {
        self.objects_mut().column = Some(column.to_string());
        self
    }
    /// `errdatatype`: the data type (and its schema) it concerns.
    pub fn datatype(mut self, schema: &str, name: &str) -> Self {
        let o = self.objects_mut();
        o.schema = Some(schema.to_string());
        o.datatype = Some(name.to_string());
        self
    }
    /// `errconstraint`: the constraint it concerns.
    pub fn constraint(mut self, name: &str) -> Self {
        self.objects_mut().constraint = Some(name.to_string());
        self
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into().into_boxed_str());
        self
    }
    pub fn context(mut self, c: impl Into<String>) -> Self {
        self.context = Some(c.into().into_boxed_str());
        self
    }
    /// A CONTEXT line of the innermost frame (an error context callback's, such as
    /// XactLockTableWait's), ahead of any the error has.
    pub fn inner_context(mut self, line: Option<String>) -> Self {
        if let Some(line) = line {
            self.context = Some(match self.context.take() {
                Some(c) => format!("{line}\n{c}").into_boxed_str(),
                None => line.into_boxed_str(),
            });
        }
        self
    }
    /// Add the CONTEXT lines of the frames it was raised in (outermost first), once.
    pub fn in_frames(mut self, frames: &[Frame]) -> Self {
        if self.stacked {
            return self;
        }
        self.stacked = true;
        let mut lines = Vec::new();
        // The innermost frame that can own a position (a query run for a PL, a SQL function).
        let mut innermost = true;
        for f in frames.iter().rev() {
            match (f, self.position) {
                // A position in a checked SQL function went into the CREATE statement (or
                // became the internal query): no line (`function_parse_error_transpose`).
                (
                    Frame::SqlFn {
                        at: SqlFnAt::Checked,
                        ..
                    },
                    _,
                ) if self.position.is_some()
                    || self.objects.as_ref().is_some_and(|o| o.internal.is_some()) =>
                {
                    continue;
                }
                // A position points into the query it was raised in.
                (Frame::Query { text, shift, .. }, Some(p)) if innermost => {
                    self.position = None;
                    self.objects_mut().internal =
                        Some((char_position(text, p as isize - 1 + shift), text.clone()));
                }
                (Frame::SqlFn { src, .. }, Some(p)) if innermost => {
                    self.position = None;
                    self.objects_mut().internal =
                        Some((char_position(src, p as isize - 1), src.to_string()));
                    lines.push(f.line());
                }
                _ => lines.push(f.line()),
            }
            innermost = false;
        }
        if !lines.is_empty() {
            let lines = lines.join("\n");
            self.context = Some(
                match self.context.take() {
                    Some(c) => format!("{c}\n{lines}"),
                    None => lines,
                }
                .into_boxed_str(),
            );
        }
        self
    }
    /// The position (a 1-based byte offset into `text`) as one into `text` run internally.
    pub fn internal_query(mut self, p: u32, text: &str) -> Self {
        self.position = None;
        self.objects_mut().internal = Some((char_position(text, p as isize - 1), text.to_string()));
        self
    }
    pub fn position(mut self, p: u32) -> Self {
        self.position = Some(p);
        self
    }
    pub fn fatal(mut self) -> Self {
        self.severity = Severity::Fatal;
        self
    }
    /// Positions are tracked as byte offsets; clients get character offsets into `sql`.
    pub fn in_chars(mut self, sql: &str) -> Self {
        if let Some(p) = self.position {
            let byte = (p as usize - 1).min(sql.len());
            let byte = (0..=byte)
                .rev()
                .find(|&b| sql.is_char_boundary(b))
                .unwrap_or(0);
            self.position = Some(sql[..byte].chars().count() as u32 + 1);
        }
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(code::IO_ERROR, format!("I/O error: {e}"))
    }
}

/// SQLSTATE codes (a subset of Postgres's errcodes.txt).
pub mod code {
    pub const SUCCESSFUL_COMPLETION: &str = "00000";
    pub const TRIGGER_PROTOCOL_VIOLATED: &str = "39P01";
    pub const TRIGGERED_ACTION_EXCEPTION: &str = "09000";
    pub const GENERATED_ALWAYS: &str = "428C9";
    pub const BAD_COPY_FILE_FORMAT: &str = "22P04";
    pub const TOO_MANY_ARGUMENTS: &str = "54023";
    pub const AMBIGUOUS_ALIAS: &str = "42P09";
    pub const WARNING: &str = "01000";
    pub const WARNING_PRIVILEGE_NOT_GRANTED: &str = "01007";
    pub const WARNING_DEPRECATED_FEATURE: &str = "01P01";
    pub const WARNING_PRIVILEGE_NOT_REVOKED: &str = "01006";
    pub const NO_DATA: &str = "02000";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const INVALID_TRANSACTION_INITIATION: &str = "0B000";
    pub const CARDINALITY_VIOLATION: &str = "21000";
    pub const DATA_EXCEPTION: &str = "22000";
    pub const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";
    pub const STRING_DATA_LENGTH_MISMATCH: &str = "22026";
    pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: &str = "2200H";
    pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const INVALID_ARGUMENT_FOR_LOGARITHM: &str = "2201E";
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
    pub const INVALID_ARGUMENT_FOR_WIDTH_BUCKET: &str = "2201G";
    pub const SUBSTRING_ERROR: &str = "22011";
    pub const INVALID_PRECEDING_OR_FOLLOWING_SIZE: &str = "22013";
    pub const INVALID_ARGUMENT_FOR_NTILE: &str = "22014";
    pub const INVALID_ARGUMENT_FOR_NTH_VALUE: &str = "22016";
    pub const ZERO_LENGTH_CHARACTER_STRING: &str = "2200F";
    pub const NONSTANDARD_USE_OF_ESCAPE_CHARACTER: &str = "22P06";
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: &str = "2201W";
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: &str = "2201X";
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    pub const WITH_CHECK_OPTION_VIOLATION: &str = "44000";
    pub const INVALID_BINARY_REPRESENTATION: &str = "22P03";
    pub const INVALID_ESCAPE_SEQUENCE: &str = "22025";
    pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";
    pub const INVALID_USE_OF_ESCAPE_CHARACTER: &str = "2200C";
    pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
    pub const UNTRANSLATABLE_CHARACTER: &str = "22P05";
    pub const INVALID_JSON_TEXT: &str = "22P02";
    pub const SQL_JSON_MEMBER_NOT_FOUND: &str = "2203A";
    pub const DUPLICATE_JSON_OBJECT_KEY_VALUE: &str = "22030";
    pub const MORE_THAN_ONE_SQL_JSON_ITEM: &str = "22034";
    pub const NO_SQL_JSON_ITEM: &str = "22035";
    pub const SQL_JSON_SCALAR_REQUIRED: &str = "2203F";
    pub const INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION: &str = "22031";
    pub const INVALID_SQL_JSON_SUBSCRIPT: &str = "22033";
    pub const NON_NUMERIC_SQL_JSON_ITEM: &str = "22036";
    pub const SINGLETON_SQL_JSON_ITEM_REQUIRED: &str = "22038";
    pub const SQL_JSON_ARRAY_NOT_FOUND: &str = "22039";
    pub const SQL_JSON_NUMBER_NOT_FOUND: &str = "2203B";
    pub const SQL_JSON_OBJECT_NOT_FOUND: &str = "2203C";
    pub const INTEGRITY_CONSTRAINT_VIOLATION: &str = "23000";
    pub const RESTRICT_VIOLATION: &str = "23001";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const INVALID_CURSOR_STATE: &str = "24000";
    pub const INVALID_TRANSACTION_STATE: &str = "25000";
    pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
    pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
    pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
    pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
    pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
    pub const INVALID_AUTHORIZATION_SPECIFICATION: &str = "28000";
    pub const INVALID_PASSWORD: &str = "28P01";
    pub const INVALID_CURSOR_NAME: &str = "34000";
    pub const INVALID_TRANSACTION_TERMINATION: &str = "2D000";
    pub const INVALID_SAVEPOINT_SPECIFICATION: &str = "3B001";
    pub const INVALID_CATALOG_NAME: &str = "3D000";
    pub const INVALID_SCHEMA_NAME: &str = "3F000";
    pub const TRANSACTION_ROLLBACK: &str = "40000";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const DEADLOCK_DETECTED: &str = "40P01";
    pub const SYNTAX_ERROR: &str = "42601";
    pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const INVALID_FUNCTION_DEFINITION: &str = "42P13";
    pub const UNDEFINED_FILE: &str = "58P01";
    pub const UNDEFINED_FUNCTION: &str = "42883";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const INVALID_TABLESAMPLE_ARGUMENT: &str = "2202H";
    pub const INVALID_TABLESAMPLE_REPEAT: &str = "2202G";
    pub const UNDEFINED_PARAMETER: &str = "42P02";
    pub const UNDEFINED_SCHEMA: &str = "3F000";
    pub const DUPLICATE_COLUMN: &str = "42701";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const DUPLICATE_OBJECT: &str = "42710";
    pub const DUPLICATE_SCHEMA: &str = "42P06";
    pub const DUPLICATE_DATABASE: &str = "42P04";
    pub const DUPLICATE_ALIAS: &str = "42712";
    pub const DUPLICATE_FUNCTION: &str = "42723";
    pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";
    pub const DUPLICATE_CURSOR: &str = "42P03";
    pub const AMBIGUOUS_COLUMN: &str = "42702";
    pub const AMBIGUOUS_FUNCTION: &str = "42725";
    pub const AMBIGUOUS_PARAMETER: &str = "42P08";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const COLLATION_MISMATCH: &str = "42P21";
    pub const INDETERMINATE_COLLATION: &str = "42P22";
    pub const WRONG_OBJECT_TYPE: &str = "42809";
    pub const CANNOT_COERCE: &str = "42846";
    pub const GROUPING_ERROR: &str = "42803";
    pub const WINDOWING_ERROR: &str = "42P20";
    pub const INVALID_RECURSION: &str = "42P19";
    pub const INVALID_FOREIGN_KEY: &str = "42830";
    pub const INVALID_NAME: &str = "42602";
    pub const NAME_TOO_LONG: &str = "42622";
    pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
    pub const INVALID_COLUMN_DEFINITION: &str = "42611";
    pub const INVALID_TABLE_DEFINITION: &str = "42P16";
    pub const INVALID_OBJECT_DEFINITION: &str = "42P17";
    pub const INDETERMINATE_DATATYPE: &str = "42P18";
    pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";
    pub const OBJECT_IN_USE: &str = "55006";
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";
    pub const UNSAFE_NEW_ENUM_VALUE_USAGE: &str = "55P04";
    pub const QUERY_CANCELED: &str = "57014";
    pub const ADMIN_SHUTDOWN: &str = "57P01";
    pub const CANNOT_CONNECT_NOW: &str = "57P03";
    pub const IO_ERROR: &str = "58030";
    pub const EXTERNAL_ROUTINE_EXCEPTION: &str = "38000";
    pub const CONFIG_FILE_ERROR: &str = "F0000";
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const CONNECTION_EXCEPTION: &str = "08000";
    pub const CONNECTION_DOES_NOT_EXIST: &str = "08003";
    pub const SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION: &str = "08001";
    pub const CONNECTION_FAILURE: &str = "08006";
    pub const S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED: &str = "2F003";
    pub const FDW_ERROR: &str = "HV000";
    pub const FDW_OUT_OF_MEMORY: &str = "HV001";
    pub const FDW_INVALID_OPTION_NAME: &str = "HV00D";
    pub const FDW_OPTION_NAME_NOT_FOUND: &str = "HV00J";
    pub const FDW_INVALID_ATTRIBUTE_VALUE: &str = "HV024";
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
    pub const STATEMENT_TOO_COMPLEX: &str = "54001";
    pub const TOO_MANY_COLUMNS: &str = "54011";
    pub const CONFIGURATION_LIMIT_EXCEEDED: &str = "53400";
    pub const OUT_OF_MEMORY: &str = "53200";
    pub const RAISE_EXCEPTION: &str = "P0001";
    pub const INTERNAL_ERROR: &str = "XX000";
    pub const DATA_CORRUPTED: &str = "XX001";
    pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";
    pub const EXCLUSION_VIOLATION: &str = "23P01";
    pub const TOO_MANY_CONNECTIONS: &str = "53300";
    pub const INVALID_CURSOR_DEFINITION: &str = "42P11";
    pub const INVALID_PSTATEMENT_DEFINITION: &str = "42P14";
    pub const INVALID_SCHEMA_DEFINITION: &str = "42P15";
    pub const UNDEFINED_DATABASE: &str = "3D000";
    pub const INVALID_ROW_COUNT_IN_LIMIT: &str = "2201W";
    pub const MOST_SPECIFIC_TYPE_MISMATCH: &str = "2200G";
    pub const DUPLICATE_ALIAS_NAME: &str = "42712";
    pub const RESERVED_NAME: &str = "42939";
    pub const INVALID_GRANT_OPERATION: &str = "0LP01";
    pub const INVALID_TEXT_REPR: &str = "22P02";
    pub const NOT_AN_XML_DOCUMENT: &str = "2200L";
    pub const INVALID_XML_DOCUMENT: &str = "2200M";
    pub const INVALID_XML_CONTENT: &str = "2200N";
    pub const INVALID_XML_COMMENT: &str = "2200S";
    pub const INVALID_XML_PROCESSING_INSTRUCTION: &str = "2200T";
    pub const INVALID_ARGUMENT_FOR_XQUERY: &str = "10608";
    pub const ASSERT_FAILURE: &str = "P0004";
    pub const NO_DATA_FOUND: &str = "P0002";
    pub const TOO_MANY_ROWS: &str = "P0003";
    pub const CASE_NOT_FOUND: &str = "20000";
    pub const SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION: &str = "42000";
    pub const OBJECT_NOT_IN_PREREQUISITE: &str = "55000";
    pub const TRIGGERED_DATA_CHANGE_VIOLATION: &str = "27000";
    pub const STATEMENT_TIMEOUT: &str = "57014";
    pub const IDLE_SESSION_TIMEOUT: &str = "57P05";
    pub const IDLE_IN_TRANSACTION_SESSION_TIMEOUT: &str = "25P03";
    pub const TRANSACTION_TIMEOUT: &str = "25P04";
}

#[macro_export]
macro_rules! err {
    ($code:ident, $($arg:tt)*) => {
        $crate::error::Error::new($crate::error::code::$code, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! bail {
    ($code:ident, $($arg:tt)*) => {
        return Err($crate::err!($code, $($arg)*))
    };
}

pub fn internal(msg: impl Into<String>) -> Error {
    Error::new(code::INTERNAL_ERROR, msg)
}

pub fn unsupported(what: impl fmt::Display) -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        format!("{what} is not supported yet"),
    )
}
