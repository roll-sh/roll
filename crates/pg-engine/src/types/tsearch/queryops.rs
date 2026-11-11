//! tsquery functions (Postgres's tsquery_op.c and tsquery_rewrite.c): combining queries,
//! containment, `querytree` and `ts_rewrite`.

use super::{
    MAXENTRYPOS, OP_NOT, OP_PHRASE, QTN_NOCHANGE, QtNode, QueryItem, TsQuery, clean_not,
    format_tsquery, qt2qtn, qtn_binary, qtn_compare, qtn_eq, qtn_sort, qtn_ternary, qtn2qt,
};
use crate::bail;
use crate::error::Result;
use std::cmp::Ordering;

/// `join_tsqueries`: `a <op> b` (children [b, a], as the tree keeps [right, left]).
fn join(a: &TsQuery, b: &TsQuery, oper: u8, distance: i16) -> TsQuery {
    let (l, r) = (qt2qtn(&a.items, 0), qt2qtn(&b.items, 0));
    let sign = l.sign | r.sign;
    let n = QtNode {
        item: QueryItem::Opr {
            oper,
            distance: if oper == OP_PHRASE { distance } else { 0 },
            left: 0,
        },
        flags: 0,
        sign,
        child: vec![r, l],
    };
    qtn2qt(&n)
}

/// `tsquery_and` / `tsquery_or`: an empty side gives the other.
pub fn and_or(a: &TsQuery, b: &TsQuery, oper: u8) -> TsQuery {
    if a.items.is_empty() {
        return b.clone();
    }
    if b.items.is_empty() {
        return a.clone();
    }
    join(a, b, oper, 0)
}

/// `tsquery_phrase_distance`.
pub fn phrase(a: &TsQuery, b: &TsQuery, distance: i32) -> Result<TsQuery> {
    if !(0..=MAXENTRYPOS as i32).contains(&distance) {
        bail!(
            INVALID_PARAMETER_VALUE,
            "distance in phrase operator must be an integer value between zero and {} inclusive",
            MAXENTRYPOS
        );
    }
    if a.items.is_empty() {
        return Ok(b.clone());
    }
    if b.items.is_empty() {
        return Ok(a.clone());
    }
    Ok(join(a, b, OP_PHRASE, distance as u16 as i16))
}

/// `tsquery_not`.
pub fn not(a: &TsQuery) -> TsQuery {
    if a.items.is_empty() {
        return a.clone();
    }
    let c = qt2qtn(&a.items, 0);
    let sign = c.sign;
    qtn2qt(&QtNode {
        item: QueryItem::Opr {
            oper: OP_NOT,
            distance: 0,
            left: 0,
        },
        flags: 0,
        sign,
        child: vec![c],
    })
}

pub fn numnode(a: &TsQuery) -> i32 {
    a.items.len() as i32
}

/// `tsq_mcontains`: every lexeme of `ex` is in `query`.
pub fn contains(query: &TsQuery, ex: &TsQuery) -> bool {
    fn collect(q: &TsQuery) -> Vec<&[u8]> {
        let mut v: Vec<&[u8]> = q.operands().map(|o| o.word.as_bytes()).collect();
        v.sort();
        v.dedup();
        v
    }
    let (qv, ev) = (collect(query), collect(ex));
    if ev.len() > qv.len() {
        return false;
    }
    let mut j = 0;
    for e in ev {
        while j < qv.len() && qv[j] != e {
            j += 1;
        }
        if j == qv.len() {
            return false;
        }
    }
    true
}

/// `querytree`: the query without its NOT branches, "T" when nothing is left.
pub fn querytree(q: &TsQuery) -> String {
    if q.items.is_empty() {
        return String::new();
    }
    let items = clean_not(q);
    if items.is_empty() {
        return "T".to_string();
    }
    format_tsquery(&TsQuery { items })
}

fn clear_flags(n: &mut QtNode, flags: u32) {
    n.flags &= !flags;
    for c in &mut n.child {
        clear_flags(c, flags);
    }
}

/// `findeq`: replace `node` if it is `ex`, or (for AND/OR) contains all of `ex`'s children.
fn findeq(node: QtNode, ex: &QtNode, subs: Option<&QtNode>) -> Option<QtNode> {
    if (node.sign & ex.sign) != ex.sign
        || node.is_val() != ex.is_val()
        || node.flags & QTN_NOCHANGE != 0
    {
        return Some(node);
    }
    let replace = |subs: Option<&QtNode>| {
        subs.map(|s| {
            let mut s = s.clone();
            s.flags |= QTN_NOCHANGE;
            s
        })
    };
    if !node.is_val() {
        if node.oper() != ex.oper() {
            return Some(node);
        }
        let mut node = node;
        if node.child.len() == ex.child.len() {
            if qtn_eq(&node, ex) {
                return replace(subs);
            }
        } else if node.child.len() > ex.child.len() && !ex.child.is_empty() {
            let mut matched = vec![false; node.child.len()];
            let (mut i, mut j, mut nmatched) = (0, 0, 0);
            while i < node.child.len() && j < ex.child.len() {
                match qtn_compare(&node.child[i], &ex.child[j]) {
                    Ordering::Equal => {
                        matched[i] = true;
                        nmatched += 1;
                        i += 1;
                        j += 1;
                    }
                    Ordering::Less => i += 1,
                    Ordering::Greater => break,
                }
            }
            if nmatched == ex.child.len() {
                let mut kept: Vec<QtNode> = std::mem::take(&mut node.child)
                    .into_iter()
                    .zip(matched)
                    .filter(|(_, m)| !m)
                    .map(|(c, _)| c)
                    .collect();
                if let Some(s) = replace(subs) {
                    kept.push(s);
                }
                node.child = kept;
                qtn_sort(&mut node);
            }
        }
        return Some(node);
    }
    let (QueryItem::Val(a), QueryItem::Val(b)) = (&node.item, &ex.item) else {
        return Some(node);
    };
    if a.valcrc != b.valcrc {
        return Some(node);
    }
    if qtn_eq(&node, ex) {
        return replace(subs);
    }
    Some(node)
}

/// `dofindsubquery`.
fn find_subquery(root: QtNode, ex: &QtNode, subs: Option<&QtNode>) -> Option<QtNode> {
    let mut root = findeq(root, ex, subs)?;
    if root.flags & QTN_NOCHANGE == 0 && !root.is_val() {
        let children = std::mem::take(&mut root.child);
        root.child = children
            .into_iter()
            .filter_map(|c| find_subquery(c, ex, subs))
            .collect();
        if root.child.is_empty() {
            return None;
        }
        if root.child.len() == 1 && root.oper() != OP_NOT {
            return root.child.pop();
        }
    }
    Some(root)
}

/// The query as a flattened, sorted tree, as rewriting wants it.
fn normalized(q: &TsQuery) -> QtNode {
    let mut t = qt2qtn(&q.items, 0);
    qtn_ternary(&mut t);
    qtn_sort(&mut t);
    t
}

/// `tsquery_rewrite`: `ex` replaced by `subst` (removed when empty).
pub fn rewrite(query: &TsQuery, ex: &TsQuery, subst: &TsQuery) -> TsQuery {
    if query.items.is_empty() || ex.items.is_empty() {
        return query.clone();
    }
    let subs = (!subst.items.is_empty()).then(|| qt2qtn(&subst.items, 0));
    match find_subquery(normalized(query), &normalized(ex), subs.as_ref()) {
        Some(mut t) => {
            qtn_binary(&mut t);
            qtn2qt(&t)
        }
        None => TsQuery::default(),
    }
}

/// `tsquery_rewrite_query`: the (target, substitute) rows of a query applied in order (rows
/// with a NULL skipped), stopping when nothing is left.
pub fn rewrite_rows(query: &TsQuery, rows: &[(Option<TsQuery>, Option<TsQuery>)]) -> TsQuery {
    if query.items.is_empty() {
        return query.clone();
    }
    let mut tree = Some(normalized(query));
    for (ex, subst) in rows {
        let (Some(ex), Some(subst)) = (ex, subst) else {
            continue;
        };
        let Some(t) = tree.take() else { break };
        if ex.items.is_empty() {
            tree = Some(t);
            continue;
        }
        let subs = (!subst.items.is_empty()).then(|| qt2qtn(&subst.items, 0));
        tree = find_subquery(t, &normalized(ex), subs.as_ref());
        if let Some(t) = tree.as_mut() {
            clear_flags(t, QTN_NOCHANGE);
            qtn_ternary(t);
            qtn_sort(t);
        }
    }
    match tree {
        Some(mut t) => {
            qtn_binary(&mut t);
            qtn2qt(&t)
        }
        None => TsQuery::default(),
    }
}
