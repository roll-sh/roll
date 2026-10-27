//! contrib/test_decoding: the example output plugin, its text exactly (test_decoding.c).

use super::log::RelDesc;
use super::plugin::{Ctx, Plugin, RowChange, Startup, TxnInfo};
use crate::catalog::quote_ident;
use crate::error::{Error, Result, code};
use crate::types::Value;
use std::sync::Arc;

#[derive(Default)]
pub struct TestDecoding {
    include_xids: bool,
    include_timestamp: bool,
    skip_empty_xacts: bool,
    only_local: bool,
    /// TestDecodingTxnData of the transaction being decoded.
    xact_wrote_changes: bool,
    stream_wrote_changes: bool,
}

fn parse_bool(s: &str) -> Option<bool> {
    crate::types::text::parse_bool(s).ok()
}

fn timestamptz_to_str(t: i64, c: &Ctx) -> String {
    crate::types::datetime::format_timestamptz_styled(t, &c.fmt.tz, crate::types::datetime::DateStyle::default())
}

/// print_literal.
fn literal(out: &mut String, typid: u32, s: &str) {
    match typid {
        21 | 23 | 20 | 26 | 700 | 701 | 1700 => out.push_str(s),
        1560 | 1562 => {
            out.push_str("B'");
            out.push_str(s);
            out.push('\'');
        }
        16 => out.push_str(if s == "t" { "true" } else { "false" }),
        _ => {
            out.push('\'');
            for ch in s.chars() {
                if ch == '\'' {
                    out.push('\'');
                }
                out.push(ch);
            }
            out.push('\'');
        }
    }
}

/// tuple_to_stringinfo.
fn tuple(out: &mut String, rel: &RelDesc, row: &[Value], skip_nulls: bool, unchanged_toast: &[usize], c: &Ctx) {
    for (i, col) in rel.cols.iter().enumerate() {
        if col.dropped {
            continue;
        }
        let v = row.get(i).unwrap_or(&Value::Null);
        if v.is_null() && skip_nulls {
            continue;
        }
        out.push(' ');
        out.push_str(&quote_ident(&col.name));
        out.push('[');
        out.push_str(&col.typname);
        out.push(']');
        out.push(':');
        let varlena = crate::catalog::relsize::typlen(&col.ty) == -1;
        if v.is_null() {
            out.push_str("null");
        } else if varlena && unchanged_toast.contains(&i) {
            out.push_str("unchanged-toast-datum");
        } else {
            literal(out, col.type_oid, &crate::types::text::format_value(v, &col.ty, &c.fmt));
        }
    }
}

fn qualified(rel: &RelDesc) -> String {
    format!("{}.{}", quote_ident(&rel.nsp), quote_ident(&rel.name))
}

impl TestDecoding {
    fn output_begin(&mut self, c: &mut Ctx, t: &TxnInfo) {
        c.write(if self.include_xids { format!("BEGIN {}", t.xid as u32) } else { "BEGIN".into() }.into_bytes());
    }

    fn output_stream_start(&mut self, c: &mut Ctx, t: &TxnInfo) {
        c.write(
            if self.include_xids {
                format!("opening a streamed block for transaction TXN {}", t.xid as u32)
            } else {
                "opening a streamed block for transaction".into()
            }
            .into_bytes(),
        );
    }

    fn at(&self, s: &mut String, time: i64, c: &Ctx) {
        if self.include_timestamp {
            s.push_str(&format!(" (at {})", timestamptz_to_str(time, c)));
        }
    }

    fn stream_begin_lazily(&mut self, c: &mut Ctx, t: &TxnInfo) {
        if self.skip_empty_xacts && !self.stream_wrote_changes {
            self.output_stream_start(c, t);
        }
        self.xact_wrote_changes = true;
        self.stream_wrote_changes = true;
    }

    fn begin_lazily(&mut self, c: &mut Ctx, t: &TxnInfo) {
        if self.skip_empty_xacts && !self.xact_wrote_changes {
            self.output_begin(c, t);
        }
        self.xact_wrote_changes = true;
    }
}

impl Plugin for TestDecoding {
    fn startup(&mut self, opts: &[(String, Option<String>)], _is_init: bool, s: &mut Startup) -> Result<()> {
        self.include_xids = true;
        let mut streaming = false;
        for (name, arg) in opts {
            let flag = |target: &mut bool| -> Result<()> {
                match arg {
                    None => *target = true,
                    Some(v) => match parse_bool(v) {
                        Some(b) => *target = b,
                        None => {
                            return Err(Error::new(
                                code::INVALID_PARAMETER_VALUE,
                                format!("could not parse value \"{v}\" for parameter \"{name}\""),
                            ));
                        }
                    },
                }
                Ok(())
            };
            match name.as_str() {
                "include-xids" => flag(&mut self.include_xids)?,
                "include-timestamp" => flag(&mut self.include_timestamp)?,
                "skip-empty-xacts" => flag(&mut self.skip_empty_xacts)?,
                "only-local" => flag(&mut self.only_local)?,
                "force-binary" | "include-rewrites" | "stream-changes" => {
                    if arg.is_some() {
                        let mut b = false;
                        flag(&mut b)?;
                        match name.as_str() {
                            "force-binary" => s.binary |= b,
                            "stream-changes" => streaming = b,
                            _ => s.output_rewrites = b,
                        }
                    }
                }
                _ => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!(
                            "option \"{name}\" = \"{}\" is unknown",
                            arg.as_deref().unwrap_or("(null)")
                        ),
                    ));
                }
            }
        }
        s.streaming = streaming;
        Ok(())
    }

    fn begin(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()> {
        self.xact_wrote_changes = false;
        if !self.skip_empty_xacts {
            self.output_begin(c, t);
        }
        Ok(())
    }

    fn commit(&mut self, c: &mut Ctx, t: &TxnInfo, _lsn: u64) -> Result<()> {
        if self.skip_empty_xacts && !self.xact_wrote_changes {
            return Ok(());
        }
        let mut s = if self.include_xids { format!("COMMIT {}", t.xid as u32) } else { "COMMIT".into() };
        self.at(&mut s, t.time, c);
        c.write(s.into_bytes());
        Ok(())
    }

    fn change(&mut self, c: &mut Ctx, t: &TxnInfo, rel: &Arc<RelDesc>, ch: &RowChange) -> Result<()> {
        self.begin_lazily(c, t);
        let mut s = format!("table {}:", qualified(rel));
        match ch {
            RowChange::Insert { new } => {
                s.push_str(" INSERT:");
                tuple(&mut s, rel, new, false, &[], c);
            }
            RowChange::Update { old, new, unchanged_toast } => {
                s.push_str(" UPDATE:");
                if let Some((o, _)) = old {
                    s.push_str(" old-key:");
                    tuple(&mut s, rel, o, true, &[], c);
                    s.push_str(" new-tuple:");
                }
                tuple(&mut s, rel, new, false, unchanged_toast, c);
            }
            RowChange::Delete { old } => {
                s.push_str(" DELETE:");
                match old {
                    None => s.push_str(" (no-tuple-data)"),
                    Some((o, _)) => tuple(&mut s, rel, o, true, &[], c),
                }
            }
        }
        c.write(s.into_bytes());
        Ok(())
    }

    fn truncate(&mut self, c: &mut Ctx, t: &TxnInfo, rels: &[Arc<RelDesc>], cascade: bool, restart: bool) -> Result<()> {
        self.begin_lazily(c, t);
        let names: Vec<String> = rels.iter().map(|r| qualified(r)).collect();
        let mut s = format!("table {}: TRUNCATE:", names.join(", "));
        if restart || cascade {
            if restart {
                s.push_str(" restart_seqs");
            }
            if cascade {
                s.push_str(" cascade");
            }
        } else {
            s.push_str(" (no-flags)");
        }
        c.write(s.into_bytes());
        Ok(())
    }

    fn message(&mut self, c: &mut Ctx, t: Option<&TxnInfo>, _lsn: u64, transactional: bool, prefix: &str, content: &[u8]) -> Result<()> {
        if transactional && let Some(t) = t {
            self.begin_lazily(c, t);
        }
        let mut b = format!(
            "message: transactional: {} prefix: {prefix}, sz: {} content:",
            transactional as i32,
            content.len()
        )
        .into_bytes();
        b.extend_from_slice(content);
        c.write(b);
        Ok(())
    }

    fn filter_by_origin(&mut self, origin: u16) -> bool {
        self.only_local && origin != 0
    }

    fn filter_prepare(&mut self, _xid: u64, gid: &str) -> bool {
        gid.contains("_nodecode")
    }

    fn begin_prepare(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()> {
        self.begin(c, t)
    }

    fn prepare(&mut self, c: &mut Ctx, t: &TxnInfo, _lsn: u64) -> Result<()> {
        if self.skip_empty_xacts && !self.xact_wrote_changes {
            return Ok(());
        }
        let mut s = format!("PREPARE TRANSACTION {}", crate::sql::func::string::quote_literal_str(&t.gid));
        if self.include_xids {
            s.push_str(&format!(", txid {}", t.xid as u32));
        }
        self.at(&mut s, t.time, c);
        c.write(s.into_bytes());
        Ok(())
    }

    fn commit_prepared(&mut self, c: &mut Ctx, t: &TxnInfo, _lsn: u64) -> Result<()> {
        let mut s = format!("COMMIT PREPARED {}", crate::sql::func::string::quote_literal_str(&t.gid));
        if self.include_xids {
            s.push_str(&format!(", txid {}", t.xid as u32));
        }
        self.at(&mut s, t.time, c);
        c.write(s.into_bytes());
        Ok(())
    }

    fn rollback_prepared(&mut self, c: &mut Ctx, t: &TxnInfo, _end: u64, _time: i64) -> Result<()> {
        let mut s = format!("ROLLBACK PREPARED {}", crate::sql::func::string::quote_literal_str(&t.gid));
        if self.include_xids {
            s.push_str(&format!(", txid {}", t.xid as u32));
        }
        self.at(&mut s, t.time, c);
        c.write(s.into_bytes());
        Ok(())
    }

    fn stream_start(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()> {
        self.stream_wrote_changes = false;
        if !self.skip_empty_xacts {
            self.output_stream_start(c, t);
        }
        Ok(())
    }

    fn stream_stop(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()> {
        if self.skip_empty_xacts && !self.stream_wrote_changes {
            return Ok(());
        }
        c.write(
            if self.include_xids {
                format!("closing a streamed block for transaction TXN {}", t.xid as u32)
            } else {
                "closing a streamed block for transaction".into()
            }
            .into_bytes(),
        );
        Ok(())
    }

    fn stream_abort(&mut self, c: &mut Ctx, t: &TxnInfo, sub_xid: u64, _lsn: u64) -> Result<()> {
        let wrote = self.xact_wrote_changes;
        if sub_xid == t.xid {
            self.xact_wrote_changes = false;
        }
        if self.skip_empty_xacts && !wrote {
            return Ok(());
        }
        c.write(
            if self.include_xids {
                format!("aborting streamed (sub)transaction TXN {}", sub_xid as u32)
            } else {
                "aborting streamed (sub)transaction".into()
            }
            .into_bytes(),
        );
        Ok(())
    }

    fn stream_prepare(&mut self, c: &mut Ctx, t: &TxnInfo, _lsn: u64) -> Result<()> {
        if self.skip_empty_xacts && !self.xact_wrote_changes {
            return Ok(());
        }
        let gid = crate::sql::func::string::quote_literal_str(&t.gid);
        let mut s = if self.include_xids {
            format!("preparing streamed transaction TXN {gid}, txid {}", t.xid as u32)
        } else {
            format!("preparing streamed transaction {gid}")
        };
        self.at(&mut s, t.time, c);
        c.write(s.into_bytes());
        Ok(())
    }

    fn stream_commit(&mut self, c: &mut Ctx, t: &TxnInfo, _lsn: u64) -> Result<()> {
        let wrote = self.xact_wrote_changes;
        self.xact_wrote_changes = false;
        if self.skip_empty_xacts && !wrote {
            return Ok(());
        }
        let mut s = if self.include_xids {
            format!("committing streamed transaction TXN {}", t.xid as u32)
        } else {
            "committing streamed transaction".into()
        };
        self.at(&mut s, t.time, c);
        c.write(s.into_bytes());
        Ok(())
    }

    fn stream_change(&mut self, c: &mut Ctx, t: &TxnInfo, _rel: &Arc<RelDesc>, _ch: &RowChange) -> Result<()> {
        self.stream_begin_lazily(c, t);
        c.write(
            if self.include_xids {
                format!("streaming change for TXN {}", t.xid as u32)
            } else {
                "streaming change for transaction".into()
            }
            .into_bytes(),
        );
        Ok(())
    }

    fn stream_message(&mut self, c: &mut Ctx, t: &TxnInfo, _lsn: u64, transactional: bool, prefix: &str, content: &[u8]) -> Result<()> {
        if transactional {
            self.stream_begin_lazily(c, t);
            c.write(
                format!("streaming message: transactional: 1 prefix: {prefix}, sz: {}", content.len()).into_bytes(),
            );
        } else {
            let mut b = format!("streaming message: transactional: 0 prefix: {prefix}, sz: {} content:", content.len())
                .into_bytes();
            b.extend_from_slice(content);
            c.write(b);
        }
        Ok(())
    }

    fn stream_truncate(&mut self, c: &mut Ctx, t: &TxnInfo, _rels: &[Arc<RelDesc>], _cascade: bool, _restart: bool) -> Result<()> {
        self.stream_begin_lazily(c, t);
        c.write(
            if self.include_xids {
                format!("streaming truncate for TXN {}", t.xid as u32)
            } else {
                "streaming truncate for transaction".into()
            }
            .into_bytes(),
        );
        Ok(())
    }
}
