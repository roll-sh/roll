//! `ArgsCursor` (`rmutil/args.c`): the cursor RediSearch parses every command with. Arguments
//! are Redis strings; name matches are C string comparisons (up to a NUL).

use crate::commands::zsets::zfloat;
use crate::util::{cstr, parse_i64};
use bytes::Bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcErr {
    Parse,
    NoArg,
    Limit,
    NoEnt,
}

impl AcErr {
    /// `AC_Strerror`.
    pub fn strerror(self) -> &'static str {
        match self {
            AcErr::Limit => "Value is outside acceptable bounds",
            AcErr::NoArg => "Expected an argument, but none provided",
            AcErr::Parse => "Could not convert argument to expected type",
            AcErr::NoEnt => "Unknown argument",
        }
    }
}

pub type AcResult<T> = Result<T, AcErr>;

/// `AC_F_GE1`, `AC_F_GE0`, `AC_F_COALESCE`.
pub const GE1: u32 = 0x100;
pub const GE0: u32 = 0x200;
pub const COALESCE: u32 = 0x800;

#[derive(Clone)]
pub struct Ac<'a> {
    pub objs: &'a [Bytes],
    pub offset: usize,
    /// `AC_TYPE_SDS`: numbers are read with `strtoll`/`strtod` (as C strings) rather than
    /// Redis' string conversions.
    pub sds: bool,
}

/// A C string comparison of an argument with a name, ignoring case.
pub fn c_eq(arg: &[u8], name: &str) -> bool {
    cstr(arg).eq_ignore_ascii_case(name.as_bytes())
}

/// `strcasecmp` of two arguments (as C strings).
pub fn c_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    cstr(a).eq_ignore_ascii_case(cstr(b))
}

impl<'a> Ac<'a> {
    pub fn new(objs: &'a [Bytes]) -> Self {
        Ac { objs, offset: 0, sds: false }
    }

    /// `ArgsCursor_InitSDS`.
    pub fn new_sds(objs: &'a [Bytes]) -> Self {
        Ac { objs, offset: 0, sds: true }
    }

    pub fn argc(&self) -> usize {
        self.objs.len()
    }

    pub fn at_end(&self) -> bool {
        self.offset >= self.objs.len()
    }

    pub fn remaining(&self) -> usize {
        self.objs.len().saturating_sub(self.offset)
    }

    pub fn peek(&self) -> Option<&'a Bytes> {
        self.objs.get(self.offset)
    }

    pub fn advance(&mut self) -> AcResult<()> {
        self.advance_by(1)
    }

    pub fn advance_by(&mut self, by: usize) -> AcResult<()> {
        if self.offset + by > self.objs.len() {
            return Err(AcErr::NoArg);
        }
        self.offset += by;
        Ok(())
    }

    /// `AC_AdvanceIfMatch`.
    pub fn advance_if(&mut self, name: &str) -> bool {
        match self.peek() {
            Some(a) if c_eq(a, name) => {
                self.offset += 1;
                true
            }
            _ => false,
        }
    }

    /// `AC_GetString` / `AC_GetRString`.
    pub fn get(&mut self) -> AcResult<&'a Bytes> {
        let a = self.objs.get(self.offset).ok_or(AcErr::NoArg)?;
        self.offset += 1;
        Ok(a)
    }

    /// `AC_GetStringNC`: `None` at the end.
    pub fn take(&mut self) -> Option<&'a Bytes> {
        self.get().ok()
    }

    /// `AC_GetDouble`'s conversion.
    fn double_at(&self, a: &[u8]) -> Option<f64> {
        if self.sds {
            let s = cstr(a);
            let (d, n) = super::strconv::c_strtod(s);
            (n == s.len() && d.abs() != f64::INFINITY).then_some(d)
        } else {
            zfloat(a)
        }
    }

    fn ll_at(&self, flags: u32) -> AcResult<i64> {
        let a = self.objs.get(self.offset).ok_or(AcErr::NoArg)?;
        let int = if self.sds {
            crate::util::strtoll_full(cstr(a)).filter(|&v| v != i64::MIN && v != i64::MAX)
        } else {
            parse_i64(a)
        };
        let v = match int {
            Some(v) => v,
            None => {
                // `tryReadAsDouble`.
                let d = self.double_at(a).ok_or(AcErr::Parse)?;
                if d.is_nan() {
                    return Err(AcErr::Parse);
                }
                if flags & COALESCE != 0 {
                    d as i64
                } else if (d as i64) as f64 != d {
                    return Err(AcErr::Parse);
                } else {
                    d as i64
                }
            }
        };
        if flags & GE0 != 0 && v < 0 {
            return Err(AcErr::Limit);
        }
        if flags & GE1 != 0 && v < 1 {
            return Err(AcErr::Limit);
        }
        Ok(v)
    }

    /// `AC_GetLongLong`.
    pub fn get_ll(&mut self, flags: u32) -> AcResult<i64> {
        let v = self.ll_at(flags)?;
        self.offset += 1;
        Ok(v)
    }

    fn ranged(&mut self, flags: u32, min: i128, max: i128, unsigned: bool) -> AcResult<i64> {
        let flags = if unsigned { flags | GE0 } else { flags };
        let v = self.ll_at(flags)?;
        if (v as i128) > max || (v as i128) < min {
            return Err(AcErr::Limit);
        }
        self.offset += 1;
        Ok(v)
    }

    /// `AC_GetUnsigned`.
    pub fn get_u32(&mut self, flags: u32) -> AcResult<u32> {
        self.ranged(flags, 0, u32::MAX as i128, true).map(|v| v as u32)
    }

    /// `AC_GetInt`.
    pub fn get_int(&mut self, flags: u32) -> AcResult<i32> {
        self.ranged(flags, i32::MIN as i128, i32::MAX as i128, false).map(|v| v as i32)
    }

    /// `AC_GetU8`.
    pub fn get_u8(&mut self, flags: u32) -> AcResult<u8> {
        self.ranged(flags, 0, u8::MAX as i128, true).map(|v| v as u8)
    }

    /// `AC_GetU16`.
    pub fn get_u16(&mut self, flags: u32) -> AcResult<u16> {
        self.ranged(flags, 0, u16::MAX as i128, true).map(|v| v as u16)
    }

    /// `AC_GetSize` / `AC_GetU64` / `AC_GetUnsignedLongLong` (a `long long` underneath).
    pub fn get_size(&mut self, flags: u32) -> AcResult<u64> {
        self.ranged(flags, 0, i64::MAX as i128, true).map(|v| v as u64)
    }

    /// `AC_GetDouble`.
    pub fn get_double(&mut self, flags: u32) -> AcResult<f64> {
        let a = self.objs.get(self.offset).ok_or(AcErr::NoArg)?;
        let d = self.double_at(a).ok_or(AcErr::Parse)?;
        if flags & GE0 != 0 && d < 0.0 {
            return Err(AcErr::Limit);
        }
        if flags & GE1 != 0 && d < 1.0 {
            return Err(AcErr::Limit);
        }
        self.offset += 1;
        Ok(d)
    }

    /// `AC_GetSlice`.
    pub fn get_slice(&mut self, n: usize) -> AcResult<Ac<'a>> {
        if n > self.remaining() {
            return Err(AcErr::NoArg);
        }
        let s = Ac { objs: &self.objs[self.offset..self.offset + n], offset: 0, sds: self.sds };
        self.offset += n;
        Ok(s)
    }

    /// `AC_GetVarArgs`: a count, then that many arguments.
    pub fn get_varargs(&mut self) -> AcResult<Ac<'a>> {
        // The count stays consumed when the slice is short (as in C).
        let n = self.get_u32(0)?;
        self.get_slice(n as usize)
    }

    /// The arguments left (`objs + offset`).
    pub fn rest(&self) -> &'a [Bytes] {
        &self.objs[self.offset.min(self.objs.len())..]
    }
}
