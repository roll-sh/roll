//! pgcrypto's error codes (`px.h` / `px.c`): the `PXE_*` codes its layers pass up, and the
//! errors the SQL functions turn them into.

use crate::Error;
use crate::error::code;

/// `ERRCODE_EXTERNAL_ROUTINE_INVOCATION_EXCEPTION`.
pub(super) const EXTERNAL_ROUTINE: &str = "39000";

pub(super) const PXE_NO_HASH: i32 = -2;
pub(super) const PXE_NO_CIPHER: i32 = -3;
pub(super) const PXE_BAD_OPTION: i32 = -5;
pub(super) const PXE_BAD_FORMAT: i32 = -6;
pub(super) const PXE_KEY_TOO_BIG: i32 = -7;
pub(super) const PXE_CIPHER_INIT: i32 = -8;
pub(super) const PXE_HASH_UNUSABLE_FOR_HMAC: i32 = -9;
pub(super) const PXE_BUG: i32 = -12;
pub(super) const PXE_ARGUMENT_ERROR: i32 = -13;
pub(super) const PXE_UNKNOWN_SALT_ALGO: i32 = -14;
pub(super) const PXE_BAD_SALT_ROUNDS: i32 = -15;
pub(super) const PXE_NO_RANDOM: i32 = -17;
pub(super) const PXE_DECRYPT_FAILED: i32 = -18;
pub(super) const PXE_ENCRYPT_FAILED: i32 = -19;
pub(super) const PXE_PGP_CORRUPT_DATA: i32 = -100;
pub(super) const PXE_PGP_CORRUPT_ARMOR: i32 = -101;
pub(super) const PXE_PGP_UNSUPPORTED_COMPR: i32 = -102;
pub(super) const PXE_PGP_UNSUPPORTED_CIPHER: i32 = -103;
pub(super) const PXE_PGP_UNSUPPORTED_HASH: i32 = -104;
pub(super) const PXE_PGP_COMPRESSION_ERROR: i32 = -105;
pub(super) const PXE_PGP_NOT_TEXT: i32 = -106;
pub(super) const PXE_PGP_UNEXPECTED_PKT: i32 = -107;
pub(super) const PXE_PGP_MATH_FAILED: i32 = -109;
pub(super) const PXE_PGP_KEY_TOO_BIG: i32 = -111;
pub(super) const PXE_PGP_UNKNOWN_PUBALGO: i32 = -112;
pub(super) const PXE_PGP_WRONG_KEY: i32 = -113;
pub(super) const PXE_PGP_MULTIPLE_KEYS: i32 = -114;
pub(super) const PXE_PGP_EXPECT_PUBLIC_KEY: i32 = -115;
pub(super) const PXE_PGP_EXPECT_SECRET_KEY: i32 = -116;
pub(super) const PXE_PGP_NOT_V4_KEYPKT: i32 = -117;
pub(super) const PXE_PGP_KEYPKT_CORRUPT: i32 = -118;
pub(super) const PXE_PGP_NO_USABLE_KEY: i32 = -119;
pub(super) const PXE_PGP_NEED_SECRET_PSW: i32 = -120;
pub(super) const PXE_PGP_BAD_S2K_MODE: i32 = -121;
pub(super) const PXE_PGP_MULTIPLE_SUBKEYS: i32 = -123;

/// `px_strerror`.
pub(super) fn strerror(err: i32) -> &'static str {
    match err {
        0 => "Everything ok",
        PXE_NO_HASH => "No such hash algorithm",
        PXE_NO_CIPHER => "No such cipher algorithm",
        PXE_BAD_OPTION => "Unknown option",
        PXE_BAD_FORMAT => "Badly formatted type",
        PXE_KEY_TOO_BIG => "Key was too big",
        PXE_CIPHER_INIT => "Cipher cannot be initialized",
        PXE_HASH_UNUSABLE_FOR_HMAC => "This hash algorithm is unusable for HMAC",
        PXE_BUG => "pgcrypto bug",
        PXE_ARGUMENT_ERROR => "Illegal argument to function",
        PXE_UNKNOWN_SALT_ALGO => "Unknown salt algorithm",
        PXE_BAD_SALT_ROUNDS => "Incorrect number of rounds",
        PXE_NO_RANDOM => "Failed to generate strong random bits",
        PXE_DECRYPT_FAILED => "Decryption failed",
        PXE_ENCRYPT_FAILED => "Encryption failed",
        PXE_PGP_CORRUPT_DATA => "Wrong key or corrupt data",
        PXE_PGP_CORRUPT_ARMOR => "Corrupt ascii-armor",
        PXE_PGP_UNSUPPORTED_COMPR => "Unsupported compression algorithm",
        PXE_PGP_UNSUPPORTED_CIPHER => "Unsupported cipher algorithm",
        PXE_PGP_UNSUPPORTED_HASH => "Unsupported digest algorithm",
        PXE_PGP_COMPRESSION_ERROR => "Compression error",
        PXE_PGP_NOT_TEXT => "Not text data",
        PXE_PGP_UNEXPECTED_PKT => "Unexpected packet in key data",
        PXE_PGP_MATH_FAILED => "Math operation failed",
        -110 => "Elgamal keys must be at least 1024 bits long",
        PXE_PGP_KEY_TOO_BIG => "Session key too big",
        PXE_PGP_UNKNOWN_PUBALGO => "Unknown public-key encryption algorithm",
        PXE_PGP_WRONG_KEY => "Wrong key",
        PXE_PGP_MULTIPLE_KEYS => "Several keys given - pgcrypto does not handle keyring",
        PXE_PGP_EXPECT_PUBLIC_KEY => "Refusing to encrypt with secret key",
        PXE_PGP_EXPECT_SECRET_KEY => "Cannot decrypt with public key",
        PXE_PGP_NOT_V4_KEYPKT => "Only V4 key packets are supported",
        PXE_PGP_KEYPKT_CORRUPT => "Corrupt key packet",
        PXE_PGP_NO_USABLE_KEY => "No encryption key found",
        PXE_PGP_NEED_SECRET_PSW => "Need password for secret key",
        PXE_PGP_BAD_S2K_MODE => "Bad S2K mode",
        -122 => "Unsupported public key algorithm",
        PXE_PGP_MULTIPLE_SUBKEYS => "Several subkeys not supported",
        _ => "Bad error code",
    }
}

/// A failure inside pgcrypto: a `PXE_*` code its caller reports, or an error raised on the
/// spot (`ereport` / `elog` deep inside, as PostgreSQL's code does).
#[derive(Debug)]
pub(super) enum PxErr {
    Code(i32),
    Raised(Error),
}

pub(super) type Px<T> = std::result::Result<T, PxErr>;

impl From<Error> for PxErr {
    fn from(e: Error) -> Self {
        PxErr::Raised(e)
    }
}

/// `px_THROW_ERROR`.
pub(super) fn throw(err: i32) -> Error {
    if err == PXE_NO_RANDOM {
        return Error::new(code::INTERNAL_ERROR, "could not generate a random number");
    }
    Error::new(EXTERNAL_ROUTINE, strerror(err))
}

/// A `PxErr` as `px_THROW_ERROR` reports it.
pub(super) fn thrown(e: PxErr) -> Error {
    match e {
        PxErr::Code(c) => throw(c),
        PxErr::Raised(e) => e,
    }
}

/// `elog(ERROR, ...)`.
pub(super) fn elog(msg: impl Into<String>) -> PxErr {
    PxErr::Raised(Error::new(code::INTERNAL_ERROR, msg))
}

/// `ereport(ERROR, errcode(ERRCODE_INVALID_PARAMETER_VALUE), errmsg(msg))`.
pub(super) fn invalid_param(msg: impl Into<String>) -> Error {
    Error::new(code::INVALID_PARAMETER_VALUE, msg)
}
