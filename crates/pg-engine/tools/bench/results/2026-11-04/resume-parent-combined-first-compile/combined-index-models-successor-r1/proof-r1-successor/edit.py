from pathlib import Path
root=Path('crates/pg-engine/src')
p=root/'exec/sizing/kept.rs';s=p.read_text();s=s.replace('SET max_parallel_workers=2; SET min_parallel_table_scan_size=0;', 'SET max_parallel_workers=1024; SET min_parallel_table_scan_size=0;',1)
a='''                    let key = "CASE WHEN g=1 THEN 9000 WHEN g=300 THEN -10 WHEN g=600 THEN -20 ELSE g END";
                    (if desc { format!("-({key})") } else { key.into() },
                        "CASE WHEN g=1 THEN ci_text(2800) WHEN g=300 THEN ci_text(3000) WHEN g=600 THEN ci_text(2880) ELSE 'small' END".into(),
                        "", if desc { " DESC" } else { "" })'''
b='''                    let (key, payload) = if desc {
                        ("CASE WHEN g=1 THEN -9000 WHEN g=300 THEN 20 WHEN g=600 THEN 10 ELSE -g END",
                         "CASE WHEN g=1 THEN ci_text(2800) WHEN g=300 THEN ci_text(2880) WHEN g=600 THEN ci_text(3000) ELSE 'small' END")
                    } else {
                        ("CASE WHEN g=1 THEN 9000 WHEN g=300 THEN -10 WHEN g=600 THEN -20 ELSE g END",
                         "CASE WHEN g=1 THEN ci_text(2800) WHEN g=300 THEN ci_text(3000) WHEN g=600 THEN ci_text(2880) ELSE 'small' END")
                    };
                    (key.into(), payload.into(), "", if desc { " DESC" } else { "" })'''
assert a in s;s=s.replace(a,b,1)
a='''            let error = result.unwrap_err();
            eprintln!("combined_index_build_validation {case:?} workers={workers} estimated={estimated}: {trace:#?}");'''
b='''            eprintln!("combined_index_build_validation {case:?} workers={workers} estimated={estimated}: {trace:#?}");
            let error = match result {
                Err(error) => error,
                Ok(rows) => panic!("expected an index build error for {case:?}, workers={workers}, estimated={estimated}; got {rows:#?}; trace: {trace:#?}"),
            };'''
assert a in s;s=s.replace(a,b,1)
a='''                assert_eq!(candidates.len(), 3, "all reserved first chunks must own a different candidate: {trace:#?}");
                let winner = candidates.iter().min_by(|a, b| a.1.0.cmp(&b.1.0)).unwrap();'''
b='''                assert_eq!(candidates.len(), 3, "all reserved first chunks must own a different candidate: {trace:#?}");
                assert_eq!(candidates.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0, 1, 2], "actual fold must include leader, worker1, worker2 in order: {trace:#?}");
                let owners: std::collections::BTreeSet<_> = candidates.iter().map(|c| &c.1.2).collect();
                assert_eq!(owners.len(), 3, "candidate data keys must be distinct: {trace:#?}");
                let keys: std::collections::BTreeSet<_> = candidates.iter().map(|c| &c.1.0).collect();
                assert_eq!(keys.len(), 3, "full encoded candidate keys must be distinct: {trace:#?}");
                for (id, candidate) in &candidates {
                    assert_eq!(trace.participants[id].minimum.as_ref(), Some(*candidate));
                    let expected = if *id == 0 { 2816 } else if *id == if desc { 1 } else { 2 } { 2896 } else { 3016 };
                    assert_eq!(candidate.1, expected, "participant {id} must retain its own size: {trace:#?}");
                }
                let winner = candidates.iter().min_by(|a, b| a.1.0.cmp(&b.1.0)).unwrap();
                assert_eq!(winner.0, if desc { 1 } else { 2 }, "DESC must select the middle participant; ASC the last: {trace:#?}");'''
assert a in s;s=s.replace(a,b,1)
p.write_text(s)
p=root/'exec/rowsize/observe.rs';s=p.read_text();s=s.replace('    pub error: Option<Error>,','    pub error: Option<Error>,\n    pub coordinator_aborted: bool,',1)
s=s.replace("fn wait<'a>(&self, mut s: std::sync::MutexGuard<'a, State>, ready: impl Fn(&State) -> bool) -> Result<std::sync::MutexGuard<'a, State>>", "fn wait<'a>(&self, mut s: std::sync::MutexGuard<'a, State>, id: usize, ready: impl Fn(&State) -> bool) -> Result<Option<std::sync::MutexGuard<'a, State>>>",1)
a='''        if let Some(e) = &s.aborted {
            return Err(e.clone());
        }
        if let Some(e) = &s.trace.gate_error {
            return Err(crate::error::internal(e));
        }
        Ok(s)'''
b='''        if let Some(e) = &s.trace.gate_error {
            return Err(crate::error::internal(e));
        }
        if s.aborted.is_some() {
            s.trace.participants.entry(id).or_default().coordinator_aborted = true;
            return Ok(None);
        }
        Ok(Some(s))'''
assert a in s;s=s.replace(a,b,1)
s=s.replace('''        let mut s = self.wait(s, |s| s.claimed == id)?;''','''        let Some(mut s) = self.wait(s, id, |s| s.claimed == id)? else { return Ok(None) };''',1)
s=s.replace('''        drop(self.wait(s, |s| s.claimed == 3)?);
        Ok(next)''','''        let Some(s) = self.wait(s, id, |s| s.claimed == 3)? else { return Ok(None) };
        drop(s);
        Ok(next)''',1)
s=s.replace('pub(crate) fn completed_first(&self, id: usize) -> Result<()>','pub(crate) fn completed_first(&self, id: usize) -> Result<bool>',1)
s=s.replace('''            drop(self.wait(s, |s| s.completed.len() == 3)?);
        }
        Ok(())''','''            return Ok(self.wait(s, id, |s| s.completed.len() == 3)?.is_some());
        }
        Ok(true)''',1)
s=s.replace('''        s.trace.participants.entry(id).or_default().error = Some(error.clone());''','''        if s.trace.gate_error.is_some() {
            return;
        }
        s.trace.participants.entry(id).or_default().error = Some(error.clone());''',1)
p.write_text(s)
p=root/'session/ddl/parbuild.rs';s=p.read_text();a='''                if let Some(o) = &self.observed {
                    o.completed_first(_participant)?;
                }''';b='''                if let Some(o) = &self.observed
                    && !o.completed_first(_participant)?
                {
                    return Ok(g);
                }''';assert a in s;s=s.replace(a,b,1);p.write_text(s)
