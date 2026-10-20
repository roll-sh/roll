//! The default text search parser (Postgres's wparser_def.c): a table-driven state machine
//! with a position stack, ported state for state under the C locale with UTF-8 (any non-ASCII
//! character is a letter, never a digit, space or punctuation).

pub const ASCIIWORD: u8 = 1;
pub const WORD_T: u8 = 2;
pub const NUMWORD: u8 = 3;
pub const EMAIL: u8 = 4;
pub const URL_T: u8 = 5;
pub const HOST: u8 = 6;
pub const SCIENTIFIC: u8 = 7;
pub const VERSIONNUMBER: u8 = 8;
pub const NUMPARTHWORD: u8 = 9;
pub const PARTHWORD: u8 = 10;
pub const ASCIIPARTHWORD: u8 = 11;
pub const SPACE: u8 = 12;
pub const TAG_T: u8 = 13;
pub const PROTOCOL: u8 = 14;
pub const NUMHWORD: u8 = 15;
pub const ASCIIHWORD: u8 = 16;
pub const HWORD: u8 = 17;
pub const URLPATH: u8 = 18;
pub const FILEPATH: u8 = 19;
pub const DECIMAL_T: u8 = 20;
pub const SIGNEDINT: u8 = 21;
pub const UNSIGNEDINT: u8 = 22;
pub const XMLENTITY: u8 = 23;
pub const LASTNUM: u8 = 23;

/// Token type aliases, indexed by type.
pub const TOK_ALIAS: [&str; 24] = [
    "",
    "asciiword",
    "word",
    "numword",
    "email",
    "url",
    "host",
    "sfloat",
    "version",
    "hword_numpart",
    "hword_part",
    "hword_asciipart",
    "blank",
    "tag",
    "protocol",
    "numhword",
    "asciihword",
    "hword",
    "url_path",
    "file",
    "float",
    "int",
    "uint",
    "entity",
];

/// Token type descriptions, indexed by type.
pub const LEX_DESCR: [&str; 24] = [
    "",
    "Word, all ASCII",
    "Word, all letters",
    "Word, letters and digits",
    "Email address",
    "URL",
    "Host",
    "Scientific notation",
    "Version number",
    "Hyphenated word part, letters and digits",
    "Hyphenated word part, all letters",
    "Hyphenated word part, all ASCII",
    "Space symbols",
    "XML tag",
    "Protocol head",
    "Hyphenated word, letters and digits",
    "Hyphenated word, all ASCII",
    "Hyphenated word, all letters",
    "URL path",
    "File or path name",
    "Decimal notation",
    "Signed integer",
    "Unsigned integer",
    "XML entity",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum S {
    Base,
    InNumWord,
    InAsciiWord,
    InWord,
    InUnsignedInt,
    InSignedIntFirst,
    InSignedInt,
    InSpace,
    InUDecimalFirst,
    InUDecimal,
    InDecimalFirst,
    InDecimal,
    InVerVersion,
    InSVerVersion,
    InVersionFirst,
    InVersion,
    InMantissaFirst,
    InMantissaSign,
    InMantissa,
    InXMLEntityFirst,
    InXMLEntity,
    InXMLEntityNumFirst,
    InXMLEntityNum,
    InXMLEntityHexNumFirst,
    InXMLEntityHexNum,
    InXMLEntityEnd,
    InTagFirst,
    InXMLBegin,
    InTagCloseFirst,
    InTagName,
    InTagBeginEnd,
    InTag,
    InTagEscapeK,
    InTagEscapeKK,
    InTagBackSleshed,
    InTagEnd,
    InCommentFirst,
    InCommentLast,
    InComment,
    InCloseCommentFirst,
    InCloseCommentLast,
    InCommentEnd,
    InHostFirstDomain,
    InHostDomainSecond,
    InHostDomain,
    InPortFirst,
    InPort,
    InHostFirstAN,
    InHost,
    InEmail,
    InFileFirst,
    InFileTwiddle,
    InPathFirst,
    InPathFirstFirst,
    InPathSecond,
    InFile,
    InFileNext,
    InURLPathFirst,
    InURLPathStart,
    InURLPath,
    InFURL,
    InProtocolFirst,
    InProtocolSecond,
    InProtocolEnd,
    InHyphenAsciiWordFirst,
    InHyphenAsciiWord,
    InHyphenWordFirst,
    InHyphenWord,
    InHyphenNumWordFirst,
    InHyphenNumWord,
    InHyphenDigitLookahead,
    InParseHyphen,
    InParseHyphenHyphen,
    InHyphenWordPart,
    InHyphenAsciiWordPart,
    InHyphenNumWordPart,
    InHyphenUnsignedInt,
    Null,
}

/// A character-class test; `Any` ends a table (C's NULL `isclass`).
#[derive(Clone, Copy)]
enum T {
    Eof,
    Eq(u8),
    Asclet,
    Alpha,
    Alnum,
    Digit,
    Xdigit,
    Space,
    NotAlnum,
    Special,
    Ignore,
    StopHost,
    Host,
    UrlPath,
    UrlChar,
    Any,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sp {
    No,
    Tags,
    Furl,
    Hyphen,
    VerVersion,
}

const NEXT: u8 = 0x00;
const BINGO: u8 = 0x01;
const POP: u8 = 0x02;
const PUSH: u8 = 0x04;
const RERUN: u8 = 0x08;
const CLEAR: u8 = 0x10;
const MERGE: u8 = 0x20;
const CLRALL: u8 = 0x40;

struct Item {
    test: T,
    flags: u8,
    to: S,
    ty: u8,
    special: Sp,
}

const fn i(test: T, flags: u8, to: S, ty: u8) -> Item {
    Item {
        test,
        flags,
        to,
        ty,
        special: Sp::No,
    }
}

const fn sp(test: T, flags: u8, to: S, ty: u8, special: Sp) -> Item {
    Item {
        test,
        flags,
        to,
        ty,
        special,
    }
}

use S::Null as N;
use T::{Alnum, Alpha, Any, Asclet, Digit, Eof, Eq, Special};

static BASE: [Item; 13] = [
    i(Eof, NEXT, N, 0),
    i(Eq(b'<'), PUSH, S::InTagFirst, 0),
    i(T::Ignore, NEXT, S::InSpace, 0),
    i(Asclet, NEXT, S::InAsciiWord, 0),
    i(Alpha, NEXT, S::InWord, 0),
    i(Digit, NEXT, S::InUnsignedInt, 0),
    i(Eq(b'-'), PUSH, S::InSignedIntFirst, 0),
    i(Eq(b'+'), PUSH, S::InSignedIntFirst, 0),
    i(Eq(b'&'), PUSH, S::InXMLEntityFirst, 0),
    i(Eq(b'~'), PUSH, S::InFileTwiddle, 0),
    i(Eq(b'/'), PUSH, S::InFileFirst, 0),
    i(Eq(b'.'), PUSH, S::InPathFirstFirst, 0),
    i(Any, NEXT, S::InSpace, 0),
];

static IN_NUM_WORD: [Item; 8] = [
    i(Eof, BINGO, S::Base, NUMWORD),
    i(Alnum, NEXT, S::InNumWord, 0),
    i(Special, NEXT, S::InNumWord, 0),
    i(Eq(b'@'), PUSH, S::InEmail, 0),
    i(Eq(b'/'), PUSH, S::InFileFirst, 0),
    i(Eq(b'.'), PUSH, S::InFileNext, 0),
    i(Eq(b'-'), PUSH, S::InHyphenNumWordFirst, 0),
    i(Any, BINGO, S::Base, NUMWORD),
];

static IN_ASCII_WORD: [Item; 15] = [
    i(Eof, BINGO, S::Base, ASCIIWORD),
    i(Asclet, NEXT, N, 0),
    i(Eq(b'.'), PUSH, S::InHostFirstDomain, 0),
    i(Eq(b'.'), PUSH, S::InFileNext, 0),
    i(Eq(b'-'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'-'), PUSH, S::InHyphenAsciiWordFirst, 0),
    i(Eq(b'_'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'@'), PUSH, S::InEmail, 0),
    i(Eq(b':'), PUSH, S::InProtocolFirst, 0),
    i(Eq(b'/'), PUSH, S::InFileFirst, 0),
    i(Digit, PUSH, S::InHost, 0),
    i(Digit, NEXT, S::InNumWord, 0),
    i(Alpha, NEXT, S::InWord, 0),
    i(Special, NEXT, S::InWord, 0),
    i(Any, BINGO, S::Base, ASCIIWORD),
];

static IN_WORD: [Item; 6] = [
    i(Eof, BINGO, S::Base, WORD_T),
    i(Alpha, NEXT, N, 0),
    i(Special, NEXT, N, 0),
    i(Digit, NEXT, S::InNumWord, 0),
    i(Eq(b'-'), PUSH, S::InHyphenWordFirst, 0),
    i(Any, BINGO, S::Base, WORD_T),
];

static IN_UNSIGNED_INT: [Item; 14] = [
    i(Eof, BINGO, S::Base, UNSIGNEDINT),
    i(Digit, NEXT, N, 0),
    i(Eq(b'.'), PUSH, S::InHostFirstDomain, 0),
    i(Eq(b'.'), PUSH, S::InUDecimalFirst, 0),
    i(Eq(b'e'), PUSH, S::InMantissaFirst, 0),
    i(Eq(b'E'), PUSH, S::InMantissaFirst, 0),
    i(Eq(b'-'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'_'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'@'), PUSH, S::InEmail, 0),
    i(Asclet, PUSH, S::InHost, 0),
    i(Alpha, NEXT, S::InNumWord, 0),
    i(Special, NEXT, S::InNumWord, 0),
    i(Eq(b'/'), PUSH, S::InFileFirst, 0),
    i(Any, BINGO, S::Base, UNSIGNEDINT),
];

static IN_SIGNED_INT_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT | CLEAR, S::InSignedInt, 0),
    i(Any, POP, N, 0),
];

static IN_SIGNED_INT: [Item; 6] = [
    i(Eof, BINGO, S::Base, SIGNEDINT),
    i(Digit, NEXT, N, 0),
    i(Eq(b'.'), PUSH, S::InDecimalFirst, 0),
    i(Eq(b'e'), PUSH, S::InMantissaFirst, 0),
    i(Eq(b'E'), PUSH, S::InMantissaFirst, 0),
    i(Any, BINGO, S::Base, SIGNEDINT),
];

static IN_SPACE: [Item; 9] = [
    i(Eof, BINGO, S::Base, SPACE),
    i(Eq(b'<'), BINGO, S::Base, SPACE),
    i(T::Ignore, NEXT, N, 0),
    i(Eq(b'-'), BINGO, S::Base, SPACE),
    i(Eq(b'+'), BINGO, S::Base, SPACE),
    i(Eq(b'&'), BINGO, S::Base, SPACE),
    i(Eq(b'/'), BINGO, S::Base, SPACE),
    i(T::NotAlnum, NEXT, S::InSpace, 0),
    i(Any, BINGO, S::Base, SPACE),
];

static IN_UDECIMAL_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, CLEAR, S::InUDecimal, 0),
    i(Any, POP, N, 0),
];

static IN_UDECIMAL: [Item; 6] = [
    i(Eof, BINGO, S::Base, DECIMAL_T),
    i(Digit, NEXT, S::InUDecimal, 0),
    i(Eq(b'.'), PUSH, S::InVersionFirst, 0),
    i(Eq(b'e'), PUSH, S::InMantissaFirst, 0),
    i(Eq(b'E'), PUSH, S::InMantissaFirst, 0),
    i(Any, BINGO, S::Base, DECIMAL_T),
];

static IN_DECIMAL_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, CLEAR, S::InDecimal, 0),
    i(Any, POP, N, 0),
];

static IN_DECIMAL: [Item; 6] = [
    i(Eof, BINGO, S::Base, DECIMAL_T),
    i(Digit, NEXT, S::InDecimal, 0),
    i(Eq(b'.'), PUSH, S::InVerVersion, 0),
    i(Eq(b'e'), PUSH, S::InMantissaFirst, 0),
    i(Eq(b'E'), PUSH, S::InMantissaFirst, 0),
    i(Any, BINGO, S::Base, DECIMAL_T),
];

static IN_VER_VERSION: [Item; 3] = [
    i(Eof, POP, N, 0),
    sp(Digit, RERUN, S::InSVerVersion, 0, Sp::VerVersion),
    i(Any, POP, N, 0),
];

static IN_SVER_VERSION: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, BINGO | CLRALL, S::InUnsignedInt, SPACE),
    i(Any, NEXT, N, 0),
];

static IN_VERSION_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, CLEAR, S::InVersion, 0),
    i(Any, POP, N, 0),
];

static IN_VERSION: [Item; 4] = [
    i(Eof, BINGO, S::Base, VERSIONNUMBER),
    i(Digit, NEXT, S::InVersion, 0),
    i(Eq(b'.'), PUSH, S::InVersionFirst, 0),
    i(Any, BINGO, S::Base, VERSIONNUMBER),
];

static IN_MANTISSA_FIRST: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Digit, CLEAR, S::InMantissa, 0),
    i(Eq(b'+'), NEXT, S::InMantissaSign, 0),
    i(Eq(b'-'), NEXT, S::InMantissaSign, 0),
    i(Any, POP, N, 0),
];

static IN_MANTISSA_SIGN: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, CLEAR, S::InMantissa, 0),
    i(Any, POP, N, 0),
];

static IN_MANTISSA: [Item; 3] = [
    i(Eof, BINGO, S::Base, SCIENTIFIC),
    i(Digit, NEXT, S::InMantissa, 0),
    i(Any, BINGO, S::Base, SCIENTIFIC),
];

static IN_XML_ENTITY_FIRST: [Item; 6] = [
    i(Eof, POP, N, 0),
    i(Eq(b'#'), NEXT, S::InXMLEntityNumFirst, 0),
    i(Asclet, NEXT, S::InXMLEntity, 0),
    i(Eq(b':'), NEXT, S::InXMLEntity, 0),
    i(Eq(b'_'), NEXT, S::InXMLEntity, 0),
    i(Any, POP, N, 0),
];

static IN_XML_ENTITY: [Item; 8] = [
    i(Eof, POP, N, 0),
    i(Alnum, NEXT, S::InXMLEntity, 0),
    i(Eq(b':'), NEXT, S::InXMLEntity, 0),
    i(Eq(b'_'), NEXT, S::InXMLEntity, 0),
    i(Eq(b'.'), NEXT, S::InXMLEntity, 0),
    i(Eq(b'-'), NEXT, S::InXMLEntity, 0),
    i(Eq(b';'), NEXT, S::InXMLEntityEnd, 0),
    i(Any, POP, N, 0),
];

static IN_XML_ENTITY_NUM_FIRST: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Eq(b'x'), NEXT, S::InXMLEntityHexNumFirst, 0),
    i(Eq(b'X'), NEXT, S::InXMLEntityHexNumFirst, 0),
    i(Digit, NEXT, S::InXMLEntityNum, 0),
    i(Any, POP, N, 0),
];

static IN_XML_ENTITY_HEX_NUM_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(T::Xdigit, NEXT, S::InXMLEntityHexNum, 0),
    i(Any, POP, N, 0),
];

static IN_XML_ENTITY_NUM: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT, S::InXMLEntityNum, 0),
    i(Eq(b';'), NEXT, S::InXMLEntityEnd, 0),
    i(Any, POP, N, 0),
];

static IN_XML_ENTITY_HEX_NUM: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(T::Xdigit, NEXT, S::InXMLEntityHexNum, 0),
    i(Eq(b';'), NEXT, S::InXMLEntityEnd, 0),
    i(Any, POP, N, 0),
];

static IN_XML_ENTITY_END: [Item; 1] = [i(Any, BINGO | CLEAR, S::Base, XMLENTITY)];

static IN_TAG_FIRST: [Item; 8] = [
    i(Eof, POP, N, 0),
    i(Eq(b'/'), PUSH, S::InTagCloseFirst, 0),
    i(Eq(b'!'), PUSH, S::InCommentFirst, 0),
    i(Eq(b'?'), PUSH, S::InXMLBegin, 0),
    i(Asclet, PUSH, S::InTagName, 0),
    i(Eq(b':'), PUSH, S::InTagName, 0),
    i(Eq(b'_'), PUSH, S::InTagName, 0),
    i(Any, POP, N, 0),
];

static IN_XML_BEGIN: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'x'), NEXT, S::InTag, 0),
    i(Any, POP, N, 0),
];

static IN_TAG_CLOSE_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InTagName, 0),
    i(Any, POP, N, 0),
];

static IN_TAG_NAME: [Item; 10] = [
    i(Eof, POP, N, 0),
    i(Eq(b'/'), NEXT, S::InTagBeginEnd, 0),
    sp(Eq(b'>'), NEXT, S::InTagEnd, 0, Sp::Tags),
    sp(T::Space, NEXT, S::InTag, 0, Sp::Tags),
    i(Alnum, NEXT, N, 0),
    i(Eq(b':'), NEXT, N, 0),
    i(Eq(b'_'), NEXT, N, 0),
    i(Eq(b'.'), NEXT, N, 0),
    i(Eq(b'-'), NEXT, N, 0),
    i(Any, POP, N, 0),
];

static IN_TAG_BEGIN_END: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'>'), NEXT, S::InTagEnd, 0),
    i(Any, POP, N, 0),
];

static IN_TAG: [Item; 19] = [
    i(Eof, POP, N, 0),
    sp(Eq(b'>'), NEXT, S::InTagEnd, 0, Sp::Tags),
    i(Eq(b'\''), NEXT, S::InTagEscapeK, 0),
    i(Eq(b'"'), NEXT, S::InTagEscapeKK, 0),
    i(Asclet, NEXT, N, 0),
    i(Digit, NEXT, N, 0),
    i(Eq(b'='), NEXT, N, 0),
    i(Eq(b'-'), NEXT, N, 0),
    i(Eq(b'_'), NEXT, N, 0),
    i(Eq(b'#'), NEXT, N, 0),
    i(Eq(b'/'), NEXT, N, 0),
    i(Eq(b':'), NEXT, N, 0),
    i(Eq(b'.'), NEXT, N, 0),
    i(Eq(b'&'), NEXT, N, 0),
    i(Eq(b'?'), NEXT, N, 0),
    i(Eq(b'%'), NEXT, N, 0),
    i(Eq(b'~'), NEXT, N, 0),
    sp(T::Space, NEXT, N, 0, Sp::Tags),
    i(Any, POP, N, 0),
];

static IN_TAG_ESCAPE_K: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Eq(b'\\'), PUSH, S::InTagBackSleshed, 0),
    i(Eq(b'\''), NEXT, S::InTag, 0),
    i(Any, NEXT, S::InTagEscapeK, 0),
];

static IN_TAG_ESCAPE_KK: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Eq(b'\\'), PUSH, S::InTagBackSleshed, 0),
    i(Eq(b'"'), NEXT, S::InTag, 0),
    i(Any, NEXT, S::InTagEscapeKK, 0),
];

static IN_TAG_BACK_SLESHED: [Item; 2] = [i(Eof, POP, N, 0), i(Any, MERGE, N, 0)];

static IN_TAG_END: [Item; 1] = [i(Any, BINGO | CLRALL, S::Base, TAG_T)];

static IN_COMMENT_FIRST: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Eq(b'-'), NEXT, S::InCommentLast, 0),
    i(Eq(b'D'), NEXT, S::InTag, 0),
    i(Eq(b'd'), NEXT, S::InTag, 0),
    i(Any, POP, N, 0),
];

static IN_COMMENT_LAST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'-'), NEXT, S::InComment, 0),
    i(Any, POP, N, 0),
];

static IN_COMMENT: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'-'), NEXT, S::InCloseCommentFirst, 0),
    i(Any, NEXT, N, 0),
];

static IN_CLOSE_COMMENT_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'-'), NEXT, S::InCloseCommentLast, 0),
    i(Any, NEXT, S::InComment, 0),
];

static IN_CLOSE_COMMENT_LAST: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Eq(b'-'), NEXT, N, 0),
    i(Eq(b'>'), NEXT, S::InCommentEnd, 0),
    i(Any, NEXT, S::InComment, 0),
];

static IN_COMMENT_END: [Item; 1] = [i(Any, BINGO | CLRALL, S::Base, TAG_T)];

static IN_HOST_FIRST_DOMAIN: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InHostDomainSecond, 0),
    i(Digit, NEXT, S::InHost, 0),
    i(Any, POP, N, 0),
];

static IN_HOST_DOMAIN_SECOND: [Item; 8] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InHostDomain, 0),
    i(Digit, PUSH, S::InHost, 0),
    i(Eq(b'-'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'_'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'.'), PUSH, S::InHostFirstDomain, 0),
    i(Eq(b'@'), PUSH, S::InEmail, 0),
    i(Any, POP, N, 0),
];

static IN_HOST_DOMAIN: [Item; 12] = [
    i(Eof, BINGO | CLRALL, S::Base, HOST),
    i(Asclet, NEXT, S::InHostDomain, 0),
    i(Digit, PUSH, S::InHost, 0),
    i(Eq(b':'), PUSH, S::InPortFirst, 0),
    i(Eq(b'-'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'_'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'.'), PUSH, S::InHostFirstDomain, 0),
    i(Eq(b'@'), PUSH, S::InEmail, 0),
    i(Digit, POP, N, 0),
    i(T::StopHost, BINGO | CLRALL, S::InURLPathStart, HOST),
    i(Eq(b'/'), PUSH, S::InFURL, 0),
    i(Any, BINGO | CLRALL, S::Base, HOST),
];

static IN_PORT_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT, S::InPort, 0),
    i(Any, POP, N, 0),
];

static IN_PORT: [Item; 5] = [
    i(Eof, BINGO | CLRALL, S::Base, HOST),
    i(Digit, NEXT, S::InPort, 0),
    i(T::StopHost, BINGO | CLRALL, S::InURLPathStart, HOST),
    i(Eq(b'/'), PUSH, S::InFURL, 0),
    i(Any, BINGO | CLRALL, S::Base, HOST),
];

static IN_HOST_FIRST_AN: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT, S::InHost, 0),
    i(Asclet, NEXT, S::InHost, 0),
    i(Any, POP, N, 0),
];

static IN_HOST: [Item; 8] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT, S::InHost, 0),
    i(Asclet, NEXT, S::InHost, 0),
    i(Eq(b'@'), PUSH, S::InEmail, 0),
    i(Eq(b'.'), PUSH, S::InHostFirstDomain, 0),
    i(Eq(b'-'), PUSH, S::InHostFirstAN, 0),
    i(Eq(b'_'), PUSH, S::InHostFirstAN, 0),
    i(Any, POP, N, 0),
];

static IN_EMAIL: [Item; 3] = [
    i(T::StopHost, POP, N, 0),
    i(T::Host, BINGO | CLRALL, S::Base, EMAIL),
    i(Any, POP, N, 0),
];

static IN_FILE_FIRST: [Item; 7] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InFile, 0),
    i(Digit, NEXT, S::InFile, 0),
    i(Eq(b'.'), NEXT, S::InPathFirst, 0),
    i(Eq(b'_'), NEXT, S::InFile, 0),
    i(Eq(b'~'), PUSH, S::InFileTwiddle, 0),
    i(Any, POP, N, 0),
];

static IN_FILE_TWIDDLE: [Item; 6] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InFile, 0),
    i(Digit, NEXT, S::InFile, 0),
    i(Eq(b'_'), NEXT, S::InFile, 0),
    i(Eq(b'/'), NEXT, S::InFileFirst, 0),
    i(Any, POP, N, 0),
];

static IN_PATH_FIRST: [Item; 7] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InFile, 0),
    i(Digit, NEXT, S::InFile, 0),
    i(Eq(b'_'), NEXT, S::InFile, 0),
    i(Eq(b'.'), NEXT, S::InPathSecond, 0),
    i(Eq(b'/'), NEXT, S::InFileFirst, 0),
    i(Any, POP, N, 0),
];

static IN_PATH_FIRST_FIRST: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Eq(b'.'), NEXT, S::InPathSecond, 0),
    i(Eq(b'/'), NEXT, S::InFileFirst, 0),
    i(Any, POP, N, 0),
];

static IN_PATH_SECOND: [Item; 5] = [
    i(Eof, BINGO | CLEAR, S::Base, FILEPATH),
    i(Eq(b'/'), NEXT | PUSH, S::InFileFirst, 0),
    i(Eq(b'/'), BINGO | CLEAR, S::Base, FILEPATH),
    i(T::Space, BINGO | CLEAR, S::Base, FILEPATH),
    i(Any, POP, N, 0),
];

static IN_FILE: [Item; 8] = [
    i(Eof, BINGO, S::Base, FILEPATH),
    i(Asclet, NEXT, S::InFile, 0),
    i(Digit, NEXT, S::InFile, 0),
    i(Eq(b'.'), PUSH, S::InFileNext, 0),
    i(Eq(b'_'), NEXT, S::InFile, 0),
    i(Eq(b'-'), NEXT, S::InFile, 0),
    i(Eq(b'/'), PUSH, S::InFileFirst, 0),
    i(Any, BINGO, S::Base, FILEPATH),
];

static IN_FILE_NEXT: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Asclet, CLEAR, S::InFile, 0),
    i(Digit, CLEAR, S::InFile, 0),
    i(Eq(b'_'), CLEAR, S::InFile, 0),
    i(Any, POP, N, 0),
];

static IN_URL_PATH_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(T::UrlChar, NEXT, S::InURLPath, 0),
    i(Any, POP, N, 0),
];

static IN_URL_PATH_START: [Item; 1] = [i(Any, NEXT, S::InURLPath, 0)];

static IN_URL_PATH: [Item; 3] = [
    i(Eof, BINGO, S::Base, URLPATH),
    i(T::UrlChar, NEXT, S::InURLPath, 0),
    i(Any, BINGO, S::Base, URLPATH),
];

static IN_FURL: [Item; 3] = [
    i(Eof, POP, N, 0),
    sp(T::UrlPath, BINGO | CLRALL, S::Base, URL_T, Sp::Furl),
    i(Any, POP, N, 0),
];

static IN_PROTOCOL_FIRST: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'/'), NEXT, S::InProtocolSecond, 0),
    i(Any, POP, N, 0),
];

static IN_PROTOCOL_SECOND: [Item; 3] = [
    i(Eof, POP, N, 0),
    i(Eq(b'/'), NEXT, S::InProtocolEnd, 0),
    i(Any, POP, N, 0),
];

static IN_PROTOCOL_END: [Item; 1] = [i(Any, BINGO | CLRALL, S::Base, PROTOCOL)];

static IN_HYPHEN_ASCII_WORD_FIRST: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Asclet, NEXT, S::InHyphenAsciiWord, 0),
    i(Alpha, NEXT, S::InHyphenWord, 0),
    i(Digit, NEXT, S::InHyphenDigitLookahead, 0),
    i(Any, POP, N, 0),
];

static IN_HYPHEN_ASCII_WORD: [Item; 7] = [
    sp(
        Eof,
        BINGO | CLRALL,
        S::InParseHyphen,
        ASCIIHWORD,
        Sp::Hyphen,
    ),
    i(Asclet, NEXT, S::InHyphenAsciiWord, 0),
    i(Alpha, NEXT, S::InHyphenWord, 0),
    i(Special, NEXT, S::InHyphenWord, 0),
    i(Digit, NEXT, S::InHyphenNumWord, 0),
    i(Eq(b'-'), PUSH, S::InHyphenAsciiWordFirst, 0),
    sp(
        Any,
        BINGO | CLRALL,
        S::InParseHyphen,
        ASCIIHWORD,
        Sp::Hyphen,
    ),
];

static IN_HYPHEN_WORD_FIRST: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Alpha, NEXT, S::InHyphenWord, 0),
    i(Digit, NEXT, S::InHyphenDigitLookahead, 0),
    i(Any, POP, N, 0),
];

static IN_HYPHEN_WORD: [Item; 6] = [
    sp(Eof, BINGO | CLRALL, S::InParseHyphen, HWORD, Sp::Hyphen),
    i(Alpha, NEXT, S::InHyphenWord, 0),
    i(Special, NEXT, S::InHyphenWord, 0),
    i(Digit, NEXT, S::InHyphenNumWord, 0),
    i(Eq(b'-'), PUSH, S::InHyphenWordFirst, 0),
    sp(Any, BINGO | CLRALL, S::InParseHyphen, HWORD, Sp::Hyphen),
];

static IN_HYPHEN_NUM_WORD_FIRST: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Alpha, NEXT, S::InHyphenNumWord, 0),
    i(Digit, NEXT, S::InHyphenDigitLookahead, 0),
    i(Any, POP, N, 0),
];

static IN_HYPHEN_NUM_WORD: [Item; 5] = [
    sp(Eof, BINGO | CLRALL, S::InParseHyphen, NUMHWORD, Sp::Hyphen),
    i(Alnum, NEXT, S::InHyphenNumWord, 0),
    i(Special, NEXT, S::InHyphenNumWord, 0),
    i(Eq(b'-'), PUSH, S::InHyphenNumWordFirst, 0),
    sp(Any, BINGO | CLRALL, S::InParseHyphen, NUMHWORD, Sp::Hyphen),
];

static IN_HYPHEN_DIGIT_LOOKAHEAD: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT, S::InHyphenDigitLookahead, 0),
    i(Alpha, NEXT, S::InHyphenNumWord, 0),
    i(Special, NEXT, S::InHyphenNumWord, 0),
    i(Any, POP, N, 0),
];

static IN_PARSE_HYPHEN: [Item; 6] = [
    i(Eof, RERUN, S::Base, 0),
    i(Asclet, NEXT, S::InHyphenAsciiWordPart, 0),
    i(Alpha, NEXT, S::InHyphenWordPart, 0),
    i(Digit, PUSH, S::InHyphenUnsignedInt, 0),
    i(Eq(b'-'), PUSH, S::InParseHyphenHyphen, 0),
    i(Any, RERUN, S::Base, 0),
];

static IN_PARSE_HYPHEN_HYPHEN: [Item; 4] = [
    i(Eof, POP, N, 0),
    i(Alnum, BINGO | CLEAR, S::InParseHyphen, SPACE),
    i(Special, BINGO | CLEAR, S::InParseHyphen, SPACE),
    i(Any, POP, N, 0),
];

static IN_HYPHEN_WORD_PART: [Item; 5] = [
    i(Eof, BINGO, S::Base, PARTHWORD),
    i(Alpha, NEXT, S::InHyphenWordPart, 0),
    i(Special, NEXT, S::InHyphenWordPart, 0),
    i(Digit, NEXT, S::InHyphenNumWordPart, 0),
    i(Any, BINGO, S::InParseHyphen, PARTHWORD),
];

static IN_HYPHEN_ASCII_WORD_PART: [Item; 6] = [
    i(Eof, BINGO, S::Base, ASCIIPARTHWORD),
    i(Asclet, NEXT, S::InHyphenAsciiWordPart, 0),
    i(Alpha, NEXT, S::InHyphenWordPart, 0),
    i(Special, NEXT, S::InHyphenWordPart, 0),
    i(Digit, NEXT, S::InHyphenNumWordPart, 0),
    i(Any, BINGO, S::InParseHyphen, ASCIIPARTHWORD),
];

static IN_HYPHEN_NUM_WORD_PART: [Item; 4] = [
    i(Eof, BINGO, S::Base, NUMPARTHWORD),
    i(Alnum, NEXT, S::InHyphenNumWordPart, 0),
    i(Special, NEXT, S::InHyphenNumWordPart, 0),
    i(Any, BINGO, S::InParseHyphen, NUMPARTHWORD),
];

static IN_HYPHEN_UNSIGNED_INT: [Item; 5] = [
    i(Eof, POP, N, 0),
    i(Digit, NEXT, N, 0),
    i(Alpha, CLEAR, S::InHyphenNumWordPart, 0),
    i(Special, CLEAR, S::InHyphenNumWordPart, 0),
    i(Any, POP, N, 0),
];

fn actions(s: S) -> &'static [Item] {
    match s {
        S::Base => &BASE,
        S::InNumWord => &IN_NUM_WORD,
        S::InAsciiWord => &IN_ASCII_WORD,
        S::InWord => &IN_WORD,
        S::InUnsignedInt => &IN_UNSIGNED_INT,
        S::InSignedIntFirst => &IN_SIGNED_INT_FIRST,
        S::InSignedInt => &IN_SIGNED_INT,
        S::InSpace => &IN_SPACE,
        S::InUDecimalFirst => &IN_UDECIMAL_FIRST,
        S::InUDecimal => &IN_UDECIMAL,
        S::InDecimalFirst => &IN_DECIMAL_FIRST,
        S::InDecimal => &IN_DECIMAL,
        S::InVerVersion => &IN_VER_VERSION,
        S::InSVerVersion => &IN_SVER_VERSION,
        S::InVersionFirst => &IN_VERSION_FIRST,
        S::InVersion => &IN_VERSION,
        S::InMantissaFirst => &IN_MANTISSA_FIRST,
        S::InMantissaSign => &IN_MANTISSA_SIGN,
        S::InMantissa => &IN_MANTISSA,
        S::InXMLEntityFirst => &IN_XML_ENTITY_FIRST,
        S::InXMLEntity => &IN_XML_ENTITY,
        S::InXMLEntityNumFirst => &IN_XML_ENTITY_NUM_FIRST,
        S::InXMLEntityNum => &IN_XML_ENTITY_NUM,
        S::InXMLEntityHexNumFirst => &IN_XML_ENTITY_HEX_NUM_FIRST,
        S::InXMLEntityHexNum => &IN_XML_ENTITY_HEX_NUM,
        S::InXMLEntityEnd => &IN_XML_ENTITY_END,
        S::InTagFirst => &IN_TAG_FIRST,
        S::InXMLBegin => &IN_XML_BEGIN,
        S::InTagCloseFirst => &IN_TAG_CLOSE_FIRST,
        S::InTagName => &IN_TAG_NAME,
        S::InTagBeginEnd => &IN_TAG_BEGIN_END,
        S::InTag => &IN_TAG,
        S::InTagEscapeK => &IN_TAG_ESCAPE_K,
        S::InTagEscapeKK => &IN_TAG_ESCAPE_KK,
        S::InTagBackSleshed => &IN_TAG_BACK_SLESHED,
        S::InTagEnd => &IN_TAG_END,
        S::InCommentFirst => &IN_COMMENT_FIRST,
        S::InCommentLast => &IN_COMMENT_LAST,
        S::InComment => &IN_COMMENT,
        S::InCloseCommentFirst => &IN_CLOSE_COMMENT_FIRST,
        S::InCloseCommentLast => &IN_CLOSE_COMMENT_LAST,
        S::InCommentEnd => &IN_COMMENT_END,
        S::InHostFirstDomain => &IN_HOST_FIRST_DOMAIN,
        S::InHostDomainSecond => &IN_HOST_DOMAIN_SECOND,
        S::InHostDomain => &IN_HOST_DOMAIN,
        S::InPortFirst => &IN_PORT_FIRST,
        S::InPort => &IN_PORT,
        S::InHostFirstAN => &IN_HOST_FIRST_AN,
        S::InHost => &IN_HOST,
        S::InEmail => &IN_EMAIL,
        S::InFileFirst => &IN_FILE_FIRST,
        S::InFileTwiddle => &IN_FILE_TWIDDLE,
        S::InPathFirst => &IN_PATH_FIRST,
        S::InPathFirstFirst => &IN_PATH_FIRST_FIRST,
        S::InPathSecond => &IN_PATH_SECOND,
        S::InFile => &IN_FILE,
        S::InFileNext => &IN_FILE_NEXT,
        S::InURLPathFirst => &IN_URL_PATH_FIRST,
        S::InURLPathStart => &IN_URL_PATH_START,
        S::InURLPath => &IN_URL_PATH,
        S::InFURL => &IN_FURL,
        S::InProtocolFirst => &IN_PROTOCOL_FIRST,
        S::InProtocolSecond => &IN_PROTOCOL_SECOND,
        S::InProtocolEnd => &IN_PROTOCOL_END,
        S::InHyphenAsciiWordFirst => &IN_HYPHEN_ASCII_WORD_FIRST,
        S::InHyphenAsciiWord => &IN_HYPHEN_ASCII_WORD,
        S::InHyphenWordFirst => &IN_HYPHEN_WORD_FIRST,
        S::InHyphenWord => &IN_HYPHEN_WORD,
        S::InHyphenNumWordFirst => &IN_HYPHEN_NUM_WORD_FIRST,
        S::InHyphenNumWord => &IN_HYPHEN_NUM_WORD,
        S::InHyphenDigitLookahead => &IN_HYPHEN_DIGIT_LOOKAHEAD,
        S::InParseHyphen => &IN_PARSE_HYPHEN,
        S::InParseHyphenHyphen => &IN_PARSE_HYPHEN_HYPHEN,
        S::InHyphenWordPart => &IN_HYPHEN_WORD_PART,
        S::InHyphenAsciiWordPart => &IN_HYPHEN_ASCII_WORD_PART,
        S::InHyphenNumWordPart => &IN_HYPHEN_NUM_WORD_PART,
        S::InHyphenUnsignedInt => &IN_HYPHEN_UNSIGNED_INT,
        S::Null => &[],
    }
}

/// One stack entry (`TParserPosition`).
#[derive(Clone, Copy)]
struct Pos {
    posbyte: usize,
    poschar: usize,
    charlen: usize,
    lenbytetoken: usize,
    lenchartoken: usize,
    state: S,
    /// Index of the PUSH action in this state's table, to resume after a POP.
    pushed_at: Option<usize>,
}

impl Pos {
    fn new(state: S) -> Pos {
        Pos {
            posbyte: 0,
            poschar: 0,
            charlen: 0,
            lenbytetoken: 0,
            lenchartoken: 0,
            state,
            pushed_at: None,
        }
    }
}

/// The parser over a string (`TParser`); `w` holds the string's code points.
struct TParser<'a> {
    s: &'a [u8],
    w: &'a [u32],
    stack: Vec<Pos>,
    ignore: bool,
    wanthost: bool,
    c: u8,
    token: usize,
    lenbytetoken: usize,
    lenchartoken: usize,
    ty: u8,
}

fn utf8_len(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// `ucs_wcwidth(c) == 0`: NUL and non-spacing characters.
fn zero_width(c: u32) -> bool {
    if c == 0 {
        return true;
    }
    if c < 0x20 || (0x7f..0xa0).contains(&c) || c > 0x10ffff {
        return false;
    }
    NONSPACING
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

impl<'a> TParser<'a> {
    fn new(s: &'a [u8], w: &'a [u32]) -> Self {
        TParser {
            s,
            w,
            stack: vec![Pos::new(S::Base)],
            ignore: false,
            wanthost: false,
            c: 0,
            token: 0,
            lenbytetoken: 0,
            lenchartoken: 0,
            ty: 0,
        }
    }

    /// `TParserCopyInit`: a parser over the rest of the string from the current position.
    fn copy(&self) -> TParser<'a> {
        let st = self.cur();
        TParser::new(
            &self.s[st.posbyte..],
            &self.w[st.poschar.min(self.w.len())..],
        )
    }

    fn cur(&self) -> &Pos {
        self.stack.last().unwrap()
    }

    fn cur_mut(&mut self) -> &mut Pos {
        self.stack.last_mut().unwrap()
    }

    fn ch(&self) -> u32 {
        self.w.get(self.cur().poschar).copied().unwrap_or(0)
    }

    fn byte(&self) -> u8 {
        self.s.get(self.cur().posbyte).copied().unwrap_or(0)
    }

    fn is_eq(&self, c: u8) -> bool {
        self.cur().charlen == 1 && self.byte() == c
    }

    /// The `p_iswhat` tests: non-ASCII is `nonascii`, ASCII uses the C classification.
    fn class(&self, nonascii: bool, f: fn(&u8) -> bool) -> bool {
        let c = self.ch();
        if c > 0x7f { nonascii } else { f(&(c as u8)) }
    }

    fn is_alpha(&self) -> bool {
        self.class(true, u8::is_ascii_alphabetic)
    }

    fn is_alnum(&self) -> bool {
        self.class(true, u8::is_ascii_alphanumeric)
    }

    fn is_special(&self) -> bool {
        let c = if self.cur().posbyte < self.s.len() {
            self.ch()
        } else {
            0
        };
        if zero_width(c) {
            return true;
        }
        let (mut lo, mut hi) = (0, STRANGE_LETTER.len());
        while lo < hi {
            let mid = lo + ((hi - lo) >> 1);
            match STRANGE_LETTER[mid].cmp(&c) {
                std::cmp::Ordering::Equal => return true,
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        false
    }

    fn is_urlchar(&self) -> bool {
        if self.cur().charlen != 1 {
            return false;
        }
        let ch = self.byte();
        if ch <= 0x20 || ch >= 0x7f {
            return false;
        }
        !matches!(
            ch,
            b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}'
        )
    }

    /// `p_ishost` / `p_isURLPath`: run a sub-parser here and absorb its token.
    fn sub_token(&mut self, start: Option<S>, want: u8) -> bool {
        let mut tmp = self.copy();
        match start {
            Some(st) => {
                let mut p = *tmp.cur();
                p.state = st;
                p.pushed_at = None;
                tmp.stack.push(p);
            }
            None => tmp.wanthost = true,
        }
        if tmp.get() && tmp.ty == want {
            let charlen = tmp.cur().charlen;
            let st = self.cur_mut();
            st.posbyte += tmp.lenbytetoken;
            st.poschar += tmp.lenchartoken;
            st.lenbytetoken += tmp.lenbytetoken;
            st.lenchartoken += tmp.lenchartoken;
            st.charlen = charlen;
            return true;
        }
        false
    }

    fn test(&mut self, t: T) -> bool {
        match t {
            T::Eof => {
                let st = self.cur();
                st.posbyte == self.s.len() || st.charlen == 0
            }
            T::Eq(c) => self.is_eq(c),
            T::Asclet => self.cur().charlen == 1 && self.byte().is_ascii() && self.is_alpha(),
            T::Alpha => self.is_alpha(),
            T::Alnum => self.is_alnum(),
            T::NotAlnum => !self.is_alnum(),
            T::Digit => self.class(false, u8::is_ascii_digit),
            T::Xdigit => self.class(false, u8::is_ascii_hexdigit),
            // C isspace: space, \t \n \v \f \r.
            T::Space => self.class(false, |c| {
                matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
            }),
            T::Special => self.is_special(),
            T::Ignore => self.ignore,
            T::StopHost => std::mem::take(&mut self.wanthost),
            T::Host => self.sub_token(None, HOST),
            T::UrlPath => self.sub_token(Some(S::InURLPathFirst), URLPATH),
            T::UrlChar => self.is_urlchar(),
            T::Any => true,
        }
    }

    fn token_has_prefix(&self, prefix: &str) -> bool {
        let t = &self.s[self.token..];
        t.len() >= prefix.len() && t[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    }

    fn special(&mut self, sp: Sp) {
        match sp {
            Sp::No => {}
            Sp::Tags => match self.cur().lenchartoken {
                8 => {
                    if self.token_has_prefix("</script") {
                        self.ignore = false;
                    }
                }
                7 => {
                    if self.token_has_prefix("</style") {
                        self.ignore = false;
                    } else if self.token_has_prefix("<script") {
                        self.ignore = true;
                    }
                }
                6 if self.token_has_prefix("<style") => self.ignore = true,
                _ => {}
            },
            Sp::Furl => {
                self.wanthost = true;
                let st = self.cur_mut();
                st.posbyte -= st.lenbytetoken;
                st.poschar -= st.lenchartoken;
            }
            Sp::Hyphen => {
                let st = self.cur_mut();
                st.posbyte -= st.lenbytetoken;
                st.poschar -= st.lenchartoken;
            }
            Sp::VerVersion => {
                let st = self.cur_mut();
                st.posbyte -= st.lenbytetoken;
                st.poschar -= st.lenchartoken;
                st.lenbytetoken = 0;
                st.lenchartoken = 0;
            }
        }
    }

    /// `TParserGet`: find the next token; false at the end of the string.
    fn get(&mut self) -> bool {
        let len = self.s.len();
        if self.cur().posbyte >= len {
            return false;
        }
        self.token = self.cur().posbyte;
        self.cur_mut().pushed_at = None;
        let mut last: Option<&'static Item> = None;
        while self.cur().posbyte <= len {
            let posbyte = self.cur().posbyte;
            let charlen = if posbyte == len {
                0
            } else {
                utf8_len(self.s[posbyte]).min(len - posbyte)
            };
            self.cur_mut().charlen = charlen;
            let state = self.cur().state;
            let table = actions(state);
            let mut idx = match self.cur_mut().pushed_at.take() {
                Some(p) => p + 1,
                None => 0,
            };
            loop {
                let it = &table[idx];
                if let T::Any = it.test {
                    break;
                }
                if let T::Eq(c) = it.test {
                    self.c = c;
                }
                if self.test(it.test) {
                    break;
                }
                idx += 1;
            }
            let item = &table[idx];
            last = Some(item);
            self.special(item.special);
            if item.flags & BINGO != 0 {
                let st = self.cur_mut();
                let (b, c) = (st.lenbytetoken, st.lenchartoken);
                st.lenbytetoken = 0;
                st.lenchartoken = 0;
                self.lenbytetoken = b;
                self.lenchartoken = c;
                self.ty = item.ty;
            }
            if item.flags & POP != 0 {
                self.stack.pop();
            } else if item.flags & PUSH != 0 {
                self.cur_mut().pushed_at = Some(idx);
                let mut p = *self.cur();
                p.pushed_at = None;
                self.stack.push(p);
            } else if item.flags & CLEAR != 0 {
                let n = self.stack.len();
                self.stack.remove(n - 2);
            } else if item.flags & CLRALL != 0 {
                let top = self.stack.pop().unwrap();
                self.stack.clear();
                self.stack.push(top);
            } else if item.flags & MERGE != 0 {
                let top = self.stack.pop().unwrap();
                let st = self.cur_mut();
                st.posbyte = top.posbyte;
                st.poschar = top.poschar;
                st.charlen = top.charlen;
                st.lenbytetoken = top.lenbytetoken;
                st.lenchartoken = top.lenchartoken;
            }
            if item.to != S::Null {
                self.cur_mut().state = item.to;
            }
            if item.flags & BINGO != 0 || (self.cur().posbyte >= len && item.flags & RERUN == 0) {
                break;
            }
            if item.flags & (RERUN | POP) != 0 {
                continue;
            }
            let st = self.cur_mut();
            if st.charlen != 0 {
                st.posbyte += st.charlen;
                st.lenbytetoken += st.charlen;
                st.poschar += 1;
                st.lenchartoken += 1;
            }
        }
        last.is_some_and(|it| it.flags & BINGO != 0)
    }
}

/// Splits `text` into (token type, token) pairs, the default parser's `prsd_nexttoken` loop.
pub fn tokens(text: &str) -> Vec<(u8, &str)> {
    let w: Vec<u32> = text.chars().map(|c| c as u32).collect();
    let mut p = TParser::new(text.as_bytes(), &w);
    let mut out = Vec::new();
    while p.get() {
        out.push((p.ty, &text[p.token..p.token + p.lenbytetoken]));
    }
    out
}

include!("parser_tables.rs");

// The parser's corpus test against Postgres is the golden tests/sql/fts_parse_corpus.sql.
