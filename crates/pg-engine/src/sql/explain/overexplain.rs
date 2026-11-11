//! pg_overexplain (contrib, new in PostgreSQL 18): EXPLAIN's DEBUG and RANGE_TABLE options,
//! once the library is loaded. What they show is the planner's bookkeeping: plan node ids,
//! range table indexes, the PlannedStmt's flags. The engine numbers its own plan the way
//! PostgreSQL numbers one of that shape: node ids in preorder (set_plan_refs), the main tree
//! first, then the subplans; a query level's relations in FROM order, then its GROUP entry,
//! then the partitions its scans read (expand_inherited_rtentry). Executor parameter ids
//! (extParam / allParam, the parameter types) aren't modelled: none.

use super::*;

/// One range table entry as overexplain_range_table shows it.
struct Rte {
    kind: &'static str,
    inh: bool,
    from: bool,
    alias: Option<String>,
    eref: String,
    rel: Option<Arc<crate::catalog::Table>>,
    lock: Option<&'static str>,
    perm: usize,
}

/// `name (col, ...)` (overexplain_alias).
fn alias(name: &str, cols: &[String]) -> String {
    let cols: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
    format!("{} ({})", quote_ident(name), cols.join(", "))
}

fn columns(t: &crate::catalog::Table) -> Vec<String> {
    t.columns.iter().map(|c| if c.dropped { String::new() } else { c.name.clone() }).collect()
}

/// A plan's range table, the RTI of each scan and Append (by plan address), the ModifyTable's
/// nominal relation, the relations a statement can't prune and its result relations.
pub(super) struct RangeTable {
    rtes: Vec<Rte>,
    rtis: HashMap<usize, usize>,
    unprunable: Vec<usize>,
    result: Vec<usize>,
    command: &'static str,
    returning: bool,
}

fn scans<'p>(p: &'p Plan, out: &mut HashMap<usize, &'p crate::sql::plan::ScanPlan>) {
    if let Plan::Scan(s) = p {
        out.insert(addr(p), s);
    }
    for c in plan_inputs(p) {
        scans(c, out);
    }
}

/// The plan's statement, under the Context / projection nodes the engine adds.
fn statement(p: &Plan) -> &Plan {
    match p {
        Plan::Insert(_) | Plan::Update(_) | Plan::Delete(_) | Plan::Merge(_) => p,
        p => match plan_inputs(p).as_slice() {
            [c] => statement(c),
            _ => p,
        },
    }
}

impl RangeTable {
    pub(super) fn build(plan: &Plan, root: &ExNode, lock_select: bool) -> RangeTable {
        let mut by_addr = HashMap::new();
        scans(plan, &mut by_addr);
        let stmt = statement(plan);
        let (command, target, returning) = match stmt {
            Plan::Insert(i) => ("insert", Some(i.rules.table.clone()), !i.returning.is_empty()),
            Plan::Update(u) => ("update", Some(u.rules.table.clone()), !u.returning.is_empty()),
            Plan::Delete(d) => ("delete", Some(d.table.clone()), !d.returning.is_empty()),
            Plan::Merge(_) => ("merge", None, false),
            _ => ("select", None, false),
        };
        let mut rt = RangeTable {
            rtes: Vec::new(),
            rtis: HashMap::new(),
            unprunable: Vec::new(),
            result: Vec::new(),
            command,
            returning,
        };
        let mut perm = 0;
        if let Some(t) = &target {
            perm += 1;
            rt.rtes.push(Rte {
                kind: "relation",
                inh: false,
                from: false,
                alias: None,
                eref: alias(&t.name, &columns(t)),
                rel: Some(t.clone()),
                lock: Some("RowExclusiveLock"),
                perm,
            });
            rt.unprunable.push(1);
            rt.result.push(1);
            rt.rtis.insert(addr(stmt), 1);
        }
        // The scans the final plan has, in its order.
        let mut seen: Vec<(usize, &crate::sql::plan::ScanPlan)> = Vec::new();
        let mut group: Option<Vec<String>> = None;
        fn walk<'p>(
            n: &ExNode,
            by_addr: &HashMap<usize, &'p crate::sql::plan::ScanPlan>,
            seen: &mut Vec<(usize, &'p crate::sql::plan::ScanPlan)>,
            group: &mut Option<Vec<String>>,
        ) {
            if let Some(k) = n.key
                && let Some(s) = by_addr.get(&k)
                && n.is_scan()
            {
                seen.push((k, s));
            }
            if group.is_none()
                && let Some(Detail::List(_, keys)) = n.details.iter().find(|d| matches!(d, Detail::List("Group Key", _)))
            {
                *group = Some(keys.iter().map(|k| k.rsplit('.').next().unwrap_or(k).to_string()).collect());
            }
            for c in n.initplans.iter().chain(&n.children).chain(&n.subplans) {
                walk(c, by_addr, seen, group);
            }
        }
        walk(root, &by_addr, &mut seen, &mut group);
        let lock = if lock_select { "RowShareLock" } else { "AccessShareLock" };
        // A level's relations in FROM order: a partition's root stands for it.
        let mut level: Vec<(usize, Arc<crate::catalog::Table>, Option<String>, bool)> = Vec::new();
        for (_, s) in &seen {
            let (t, inh) = match s.part.as_deref().and_then(|l| l.first()) {
                Some(l) => (l.parent.clone(), true),
                None => (s.table.clone(), false),
            };
            if !level.iter().any(|(_, x, _, _)| x.oid == t.oid) {
                level.push((s.rti.0, t, s.alias.clone(), inh));
            }
        }
        level.sort_by_key(|l| l.0);
        let base = rt.rtes.len();
        for (_, t, a, inh) in &level {
            perm += 1;
            let name = a.clone().unwrap_or_else(|| t.name.clone());
            rt.rtes.push(Rte {
                kind: "relation",
                inh: *inh,
                from: true,
                // (A FROM item written without an alias has none; its reference name is its own.)
                alias: a.as_ref().filter(|a| **a != t.name).map(|a| alias(a, &[])),
                eref: alias(&name, &columns(t)),
                rel: Some(t.clone()),
                lock: Some(lock),
                perm,
            });
            rt.unprunable.push(rt.rtes.len());
        }
        if let Some(keys) = group {
            rt.rtes.push(Rte {
                kind: "group",
                inh: false,
                from: false,
                alias: None,
                eref: alias("*GROUP*", &keys),
                rel: None,
                lock: None,
                perm: 0,
            });
        }
        if seen.is_empty() && command != "merge" {
            rt.rtes.push(Rte {
                kind: "result",
                inh: false,
                from: false,
                alias: None,
                eref: alias("*RESULT*", &[]),
                rel: None,
                lock: None,
                perm: 0,
            });
        }
        for (k, s) in &seen {
            let pos = level.iter().position(|(_, t, _, _)| match s.part.as_deref().and_then(|l| l.first()) {
                Some(l) => l.parent.oid == t.oid,
                None => s.table.oid == t.oid,
            });
            let Some(pos) = pos else { continue };
            if !level[pos].3 {
                rt.rtis.insert(*k, base + pos + 1);
                continue;
            }
            let parent_name = level[pos].2.clone().unwrap_or_else(|| level[pos].1.name.clone());
            let a = alias(&parent_name, &columns(&s.table));
            rt.rtes.push(Rte {
                kind: "relation",
                inh: false,
                from: true,
                alias: Some(a.clone()),
                eref: a,
                rel: Some(s.table.clone()),
                lock: Some(lock),
                perm: 0,
            });
            rt.rtis.insert(*k, rt.rtes.len());
            rt.unprunable.push(rt.rtes.len());
        }
        rt.parents(root, &by_addr, &level, base);
        rt
    }

    /// An Append over a partitioned table's scans: the root's RTI (its apprelids).
    fn parents(
        &mut self,
        n: &ExNode,
        by_addr: &HashMap<usize, &crate::sql::plan::ScanPlan>,
        level: &[(usize, Arc<crate::catalog::Table>, Option<String>, bool)],
        base: usize,
    ) {
        if matches!(n.node_type, "Append" | "Merge Append")
            && let Some(k) = n.key
            && let Some(s) = n.children.iter().find_map(|c| c.key.and_then(|k| by_addr.get(&k)))
            && let Some(root) = s.part.as_deref().and_then(|l| l.first())
            && let Some(pos) = level.iter().position(|l| l.1.oid == root.parent.oid)
        {
            self.rtis.insert(k, base + pos + 1);
        }
        for c in n.initplans.iter().chain(&n.children).chain(&n.subplans) {
            self.parents(c, by_addr, level, base);
        }
    }

    /// RANGE_TABLE's per-node fields: a scan's RTI, an Append's, a ModifyTable's nominal one.
    fn node_fields(&self, n: &mut ExNode) {
        let rti = n.key.and_then(|k| self.rtis.get(&k)).copied();
        match (n.node_type, rti) {
            ("ModifyTable", _) => {
                n.details.push(Detail::Hook("Nominal RTI", rti.unwrap_or(1).to_string(), true));
                n.details.push(Detail::Hook("Exclude Relation RTI", "0".into(), true));
            }
            ("Append" | "Merge Append", Some(r)) => n.details.push(Detail::Hook("Append RTIs", r.to_string(), false)),
            (_, Some(r)) if n.is_scan() => n.details.push(Detail::Hook("Scan RTI", r.to_string(), true)),
            _ => {}
        }
        for c in n.initplans.iter_mut().chain(n.children.iter_mut()).chain(n.subplans.iter_mut()) {
            self.node_fields(c);
        }
    }
}

/// The node types that are never parallel-safe.
fn parallel_unsafe(t: &str) -> bool {
    matches!(
        t,
        "Gather" | "Gather Merge" | "ModifyTable" | "LockRows" | "CTE Scan" | "WorkTable Scan" | "Recursive Union"
    )
}

/// DEBUG's fields of `n` and its children (overexplain_per_node_hook): disabled nodes
/// below and at it, parallel safety, the node id. Returns (disabled nodes, parallel safe).
fn debug_node(n: &mut ExNode, next: &mut usize, ok: bool, top: bool, structured: bool, forced: bool) -> (usize, bool) {
    let id = *next;
    *next += 1;
    let mut disabled = n.disabled as usize;
    let mut safe = ok && !parallel_unsafe(n.node_type) && n.initplans.is_empty() && n.subplans.is_empty();
    // (A FROM-less Result at the top is parallel-safe only for debug_parallel_query.)
    if top && n.node_type == "Result" && n.children.is_empty() && !forced {
        safe = false;
    }
    for c in &mut n.children {
        let (d, s) = debug_node(c, next, ok, false, structured, forced);
        // (debug_parallel_query's Gather goes on the finished plan: its own count.)
        if !(top && forced && n.node_type == "Gather") {
            disabled += d;
        }
        safe &= s || n.node_type.starts_with("Gather");
    }
    n.details.push(Detail::Hook("Disabled Nodes", disabled.to_string(), true));
    n.details.push(Detail::Hook("Parallel Safe", safe.to_string(), true));
    n.details.push(Detail::Hook("Plan Node ID", id.to_string(), true));
    if structured {
        n.details.push(Detail::Hook("extParam", "none".into(), false));
        n.details.push(Detail::Hook("allParam", "none".into(), false));
    }
    (disabled, safe)
}

/// The subplans (InitPlans, SubPlans) under `n`, numbered after the main tree.
fn debug_subplans(n: &mut ExNode, next: &mut usize, ok: bool, structured: bool, forced: bool) {
    for c in n.initplans.iter_mut().chain(n.subplans.iter_mut()) {
        debug_node(c, next, ok, false, structured, forced);
        debug_subplans(c, next, ok, structured, forced);
    }
    for c in &mut n.children {
        debug_subplans(c, next, ok, structured, forced);
    }
}

/// The nodes with an EvalPlanQual parameter of their own.
fn epq_params(n: &ExNode) -> usize {
    matches!(n.node_type, "ModifyTable" | "LockRows") as usize
        + n.children.iter().chain(&n.initplans).chain(&n.subplans).map(epq_params).sum::<usize>()
}

fn has_gather(n: &ExNode) -> bool {
    n.node_type.starts_with("Gather") || n.children.iter().chain(&n.initplans).chain(&n.subplans).any(has_gather)
}

/// Adds the options' per-node fields to the finished plan.
pub(super) fn per_node(root: &mut ExNode, o: &Options, rt: &RangeTable, ctx: &ExecCtx) {
    if o.debug {
        let st = ctx.sess.settings.borrow();
        let forced = st.get_str("debug_parallel_query").is_some_and(|v| v != "off");
        let ok = rt.command == "select" && st.get_str("max_parallel_workers_per_gather").is_some_and(|v| v != "0");
        drop(st);
        let structured = o.format != Format::Text;
        let mut next = 0;
        debug_node(root, &mut next, ok, true, structured, forced);
        debug_subplans(root, &mut next, ok, structured, forced);
    }
    if o.range_table {
        rt.node_fields(root);
    }
}

/// The options' per-plan sections (overexplain_per_plan_hook): the PlannedStmt, the range
/// table.
pub(super) fn per_plan(w: &mut Writer, o: &Options, rt: &RangeTable, root: &ExNode) {
    let text = o.format == Format::Text;
    let bms = |v: &[usize]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" ")
        }
    };
    if o.debug {
        if text {
            w.buf.push_str(&" ".repeat(w.indent * 2));
            w.buf.push_str("PlannedStmt:\n");
            w.indent += 1;
        } else {
            w.open("PlannedStmt", Some("PlannedStmt"), true);
        }
        w.prop("Command Type", rt.command, false);
        let mut flags = Vec::new();
        if rt.returning {
            flags.push("hasReturning");
        }
        flags.push("canSetTag");
        if has_gather(root) {
            flags.push("parallelModeNeeded");
        }
        w.prop("Flags", &flags.join(", "), false);
        w.prop("Subplans Needing Rewind", "none", false);
        let mut oids: Vec<u32> = Vec::new();
        for r in &rt.rtes {
            if let Some(t) = &r.rel
                && !oids.contains(&t.oid)
            {
                oids.push(t.oid);
            }
        }
        let oids: Vec<usize> = oids.iter().map(|o| *o as usize).collect();
        w.prop("Relation OIDs", &bms(&oids), false);
        // (A ModifyTable's and a LockRows' EvalPlanQual parameter, SS_assign_special_param:
        // type 0. Nested loop parameters aren't modelled.)
        let epq = vec![0usize; epq_params(root)];
        w.prop("Executor Parameter Types", &bms(&epq), false);
        w.prop("Parse Location", "0 to end", false);
        if text {
            w.indent -= 1;
        } else {
            w.close("PlannedStmt", true);
        }
    }
    if !o.range_table {
        return;
    }
    if !text {
        w.open("Range Table", Some("Range Table"), false);
    }
    for (i, r) in rt.rtes.iter().enumerate() {
        if text {
            w.buf.push_str(&" ".repeat(w.indent * 2));
            w.buf.push_str(&format!(
                "RTI {} ({}{}{}):\n",
                i + 1,
                r.kind,
                if r.inh { ", inherited" } else { "" },
                if r.from { ", in-from-clause" } else { "" }
            ));
            w.indent += 1;
        } else {
            w.open("Range Table Entry", None, true);
            w.prop("RTI", &(i + 1).to_string(), true);
            w.prop("Kind", r.kind, false);
            w.prop("Inherited", &r.inh.to_string(), true);
            w.prop("In From Clause", &r.from.to_string(), true);
        }
        if let Some(a) = &r.alias {
            w.prop("Alias", a, false);
        }
        w.prop("Eref", &r.eref, false);
        if let Some(t) = &r.rel {
            w.prop("Relation", &quote_ident(&t.name), false);
            let kind = if t.partkey.is_some() {
                "partitioned_table"
            } else if t.is_foreign() {
                "foreign_table"
            } else {
                "relation"
            };
            w.prop("Relation Kind", kind, false);
        }
        if let Some(l) = r.lock {
            w.prop("Relation Lock Mode", l, false);
        }
        if r.perm != 0 {
            w.prop("Permission Info Index", &r.perm.to_string(), true);
        }
        if !text {
            w.prop("Security Barrier", "false", true);
            w.prop("Lateral", "false", true);
        }
        if text {
            w.indent -= 1;
        } else {
            w.close("Range Table Entry", true);
        }
    }
    if !text {
        w.close("Range Table", false);
    }
    if !text || !rt.unprunable.is_empty() {
        w.prop("Unprunable RTIs", &bms(&rt.unprunable), false);
    }
    if !text || !rt.result.is_empty() {
        w.prop("Result RTIs", &bms(&rt.result), false);
    }
}
