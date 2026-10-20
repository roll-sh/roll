//! pg_snapshot / txid_snapshot: the transactions a snapshot sees as finished (`xmin:xmax:xip`).

use crate::error::{Error, Result, code};

/// Transactions below `xmin` are finished; `xmax` and later had not started; in between, those
/// in `xip` (ascending, distinct) were still running.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Snapshot {
    pub xmin: u64,
    pub xmax: u64,
    pub xip: Vec<u64>,
}

impl Snapshot {
    /// Whether transaction `xid` had committed (or aborted) when the snapshot was taken.
    pub fn visible(&self, xid: u64) -> bool {
        xid < self.xmin || (xid < self.xmax && self.xip.binary_search(&xid).is_err())
    }

    pub fn parse(s: &str, typname: &str) -> Result<Snapshot> {
        let bad = || {
            Error::new(
                code::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type {typname}: \"{s}\""),
            )
        };
        let mut rest = s;
        let num = |rest: &mut &str| -> Result<u64> {
            let n = rest.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                return Err(bad());
            }
            let v = rest[..n].parse::<u64>().map_err(|_| bad())?;
            *rest = &rest[n..];
            Ok(v)
        };
        let xmin = num(&mut rest)?;
        rest = rest.strip_prefix(':').ok_or_else(bad)?;
        let xmax = num(&mut rest)?;
        rest = rest.strip_prefix(':').ok_or_else(bad)?;
        // Postgres's FirstNormalTransactionId: 0 to 2 are special.
        if xmin == 0 || xmax == 0 || xmin > xmax || xmax > i64::MAX as u64 {
            return Err(bad());
        }
        let mut xip = Vec::new();
        while !rest.is_empty() {
            let x = num(&mut rest)?;
            if x < xmin || x >= xmax || xip.last().is_some_and(|l| *l > x) {
                return Err(bad());
            }
            if xip.last() != Some(&x) {
                xip.push(x);
            }
            if let Some(r) = rest.strip_prefix(',') {
                rest = r;
            } else if !rest.is_empty() {
                return Err(bad());
            }
        }
        Ok(Snapshot { xmin, xmax, xip })
    }

    pub fn format(&self) -> String {
        let xip: Vec<String> = self.xip.iter().map(u64::to_string).collect();
        format!("{}:{}:{}", self.xmin, self.xmax, xip.join(","))
    }
}

/// xid8's input: like strtoull with base 0 (decimal, 0x hex or 0 octal).
pub fn parse_xid8(s: &str) -> Result<u64> {
    let bad = || {
        Error::new(
            code::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type xid8: \"{s}\""),
        )
    };
    let t = s.trim_start();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, h)
    } else if t.len() > 1 && t.starts_with('0') {
        (8, &t[1..])
    } else {
        (10, t)
    };
    let n = digits.chars().take_while(|c| c.is_digit(radix)).count();
    if n == 0 && radix != 8 {
        return Err(bad());
    }
    if !digits[n..].trim().is_empty() {
        return Err(bad());
    }
    let v = if n == 0 {
        0
    } else {
        u64::from_str_radix(&digits[..n], radix).map_err(|_| {
            Error::new(
                code::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("value \"{s}\" is out of range for type xid8"),
            )
        })?
    };
    Ok(if neg { v.wrapping_neg() } else { v })
}
