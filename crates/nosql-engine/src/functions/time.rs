//! Time functions (`TimeFcts`, Apache-2.0): `now()` and the `current*` family, conversions
//! between timestamp / date / timeuuid, `minTimeuuid` / `maxTimeuuid`, ScyllaDB's `dateOf` /
//! `unixTimestampOf`. Cassandra 5 registers the snake_case names and keeps the legacy ones;
//! ScyllaDB has only the legacy ones.

use bytes::Bytes;
use nosql_types::{CqlType, Dialect};

use super::util::{arg, bytes_i64};
use super::{FnCtx, Native, Registry};
use crate::error::{Error, Result};

type Imp = fn(&FnCtx, &[Option<Bytes>]) -> Result<Option<Bytes>>;

fn timeuuid_now(_: &FnCtx, _: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(Some(Bytes::copy_from_slice(&nosql_types::timeuuid::now())))
}

fn timestamp_now(_: &FnCtx, _: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(bytes_i64(nosql_types::temporal::now_millis()))
}

fn date_now(_: &FnCtx, _: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(Some(Bytes::copy_from_slice(
        &nosql_types::temporal::current_date().to_be_bytes(),
    )))
}

fn time_now(_: &FnCtx, _: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(bytes_i64(nosql_types::temporal::current_time()))
}

fn uuid16(b: &[u8]) -> Option<[u8; 16]> {
    <[u8; 16]>::try_from(b).ok()
}

/// Shared by all conversions: `ty` is the argument type.
fn to_date(ty: &CqlType, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(arg(args, 0)
        .filter(|b| !b.is_empty())
        .and_then(|b| nosql_types::cast::to_date(ty, b, ctx.dialect))
        .map(|d| Bytes::copy_from_slice(&d.to_be_bytes())))
}

fn to_timestamp(ty: &CqlType, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(arg(args, 0)
        .filter(|b| !b.is_empty())
        .and_then(|b| nosql_types::cast::to_millis(ty, b))
        .and_then(bytes_i64))
}

fn to_unix(ty: &CqlType, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    Ok(arg(args, 0)
        .filter(|b| !b.is_empty())
        .and_then(|b| nosql_types::cast::to_unix_timestamp(ty, b))
        .and_then(bytes_i64))
}

pub fn register(r: &mut Registry) {
    let d = r.dialect();
    let both = |legacy: &'static str, modern: &'static str| -> Vec<&'static str> {
        if d.is_scylla() {
            vec![legacy]
        } else {
            vec![modern, legacy]
        }
    };
    r.add(Native::nondeterministic(
        "now",
        vec![],
        CqlType::Timeuuid,
        timeuuid_now,
    ));
    let nows: [(&str, &str, CqlType, Imp); 4] = [
        (
            "currenttimeuuid",
            "current_timeuuid",
            CqlType::Timeuuid,
            timeuuid_now,
        ),
        (
            "currenttimestamp",
            "current_timestamp",
            CqlType::Timestamp,
            timestamp_now,
        ),
        ("currentdate", "current_date", CqlType::Date, date_now),
        ("currenttime", "current_time", CqlType::Time, time_now),
    ];
    for (legacy, modern, ty, imp) in nows {
        for n in both(legacy, modern) {
            r.add(Native::nondeterministic(n, vec![], ty.clone(), imp));
        }
    }
    for n in both("mintimeuuid", "min_timeuuid") {
        r.add(Native::new(
            n,
            vec![CqlType::Timestamp],
            CqlType::Timeuuid,
            |c, a| {
                let Some(ms) = arg(a, 0).and_then(|b| super::util::i64_of(b)) else {
                    return Ok(None);
                };
                scylla_timeuuid_range(c, "mintimeuuid", ms)?;
                Ok(Some(Bytes::copy_from_slice(
                    &nosql_types::timeuuid::min_timeuuid(ms),
                )))
            },
        ));
    }
    for n in both("maxtimeuuid", "max_timeuuid") {
        r.add(Native::new(
            n,
            vec![CqlType::Timestamp],
            CqlType::Timeuuid,
            |c, a| {
                let Some(ms) = arg(a, 0).and_then(|b| super::util::i64_of(b)) else {
                    return Ok(None);
                };
                scylla_timeuuid_range(c, "maxtimeuuid", ms)?;
                Ok(Some(Bytes::copy_from_slice(
                    &nosql_types::timeuuid::max_timeuuid(ms),
                )))
            },
        ));
    }
    for n in both("todate", "to_date") {
        r.add(Native::new(
            n,
            vec![CqlType::Timeuuid],
            CqlType::Date,
            |c, a| to_date(&CqlType::Timeuuid, c, a),
        ));
        r.add(Native::new(
            n,
            vec![CqlType::Timestamp],
            CqlType::Date,
            |c, a| to_date(&CqlType::Timestamp, c, a),
        ));
    }
    for n in both("totimestamp", "to_timestamp") {
        r.add(Native::new(
            n,
            vec![CqlType::Timeuuid],
            CqlType::Timestamp,
            |_, a| to_timestamp(&CqlType::Timeuuid, a),
        ));
        r.add(Native::new(
            n,
            vec![CqlType::Date],
            CqlType::Timestamp,
            |_, a| to_timestamp(&CqlType::Date, a),
        ));
    }
    for n in both("tounixtimestamp", "to_unix_timestamp") {
        r.add(Native::new(
            n,
            vec![CqlType::Timeuuid],
            CqlType::Bigint,
            |_, a| to_unix(&CqlType::Timeuuid, a),
        ));
        r.add(Native::new(
            n,
            vec![CqlType::Timestamp],
            CqlType::Bigint,
            |_, a| to_unix(&CqlType::Timestamp, a),
        ));
        r.add(Native::new(
            n,
            vec![CqlType::Date],
            CqlType::Bigint,
            |_, a| to_unix(&CqlType::Date, a),
        ));
    }
    if d == Dialect::Scylla {
        r.add(Native::new(
            "dateof",
            vec![CqlType::Timeuuid],
            CqlType::Timestamp,
            |_, a| to_timestamp(&CqlType::Timeuuid, a),
        ));
        r.add(Native::new(
            "unixtimestampof",
            vec![CqlType::Timeuuid],
            CqlType::Bigint,
            |_, a| {
                Ok(arg(a, 0)
                    .and_then(|b| uuid16(b))
                    .map(|u| nosql_types::timeuuid::unix_timestamp_of(&u))
                    .and_then(bytes_i64))
            },
        ));
    }
}

/// ScyllaDB's checks of a `min/maxtimeuuid` argument (probed): a timestamp more than 2^60 100-ns
/// ticks (in milliseconds) from the Gregorian epoch is out of range (a server error), one before
/// the epoch doesn't fit a timeuuid (a function failure). Cassandra wraps silently.
fn scylla_timeuuid_range(c: &FnCtx, name: &str, ms: i64) -> Result<()> {
    if !c.dialect.is_scylla() {
        return Ok(());
    }
    const GREGORIAN_MS: i128 = 12_219_292_800_000;
    const LIMIT: i128 = 115_292_150_460_683;
    let offset = ms as i128 + GREGORIAN_MS;
    if offset.abs() > LIMIT {
        return Err(Error::server(format!(
            "{ms}: timestamp is out of range. Must be in milliseconds since epoch"
        )));
    }
    if offset < 0 {
        return Err(Error::function_failure(
            format!(
                "execution of {name} failed: Failed execution of function system.{name}: seastar::internal::backtraced<std::runtime_error> (timeuuid time must fit in 60 bits)"
            ),
            "system",
            name,
            vec!["org.apache.cassandra.db.marshal.TimestampType".into()],
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{Registry, Typed};
    use super::*;

    #[test]
    fn conversions() {
        let r = Registry::new(Dialect::Scylla);
        let ctx = FnCtx {
            dialect: Dialect::Scylla,
            protocol_version: 4,
        };
        let ts = Typed {
            ty: CqlType::Timestamp,
            text: "ts".into(),
        };
        let f = r
            .resolve(None, "mintimeuuid", &[&ts], None, None)
            .unwrap()
            .unwrap();
        // Captured: minTimeuuid('2024-01-01 00:00:00+0000').
        let out = f
            .execute(&ctx, &[bytes_i64(1_704_067_200_000)])
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::uuid::format(&<[u8; 16]>::try_from(&out[..]).unwrap()),
            "b4cc8000-a838-11ee-8080-808080808080"
        );
        let f = r
            .resolve(None, "maxtimeuuid", &[&ts], None, None)
            .unwrap()
            .unwrap();
        let out = f
            .execute(&ctx, &[bytes_i64(1_704_067_200_000)])
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::uuid::format(&<[u8; 16]>::try_from(&out[..]).unwrap()),
            "b4cca70f-a838-11ee-7f7f-7f7f7f7f7f7f"
        );
        let d = r
            .resolve(None, "todate", &[&ts], None, None)
            .unwrap()
            .unwrap();
        // 2024-02-29 12:34:56.789 → Date(19782), stored with the 2^31 epoch offset.
        let out = d
            .execute(&ctx, &[bytes_i64(1_709_210_096_789)])
            .unwrap()
            .unwrap();
        assert_eq!(
            u32::from_be_bytes(out[..].try_into().unwrap()),
            (1u32 << 31) + 19782
        );
        assert!(r.exists("dateof"));
        assert!(!Registry::new(Dialect::Cassandra).exists("dateof"));
        assert!(Registry::new(Dialect::Cassandra).exists("to_unix_timestamp"));
    }
}
