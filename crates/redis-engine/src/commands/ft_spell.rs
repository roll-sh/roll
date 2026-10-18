//! FT.SPELLCHECK (`SpellCheckCommand`, `spell_check.c`): every term of the parsed query that
//! the index doesn't hold, with the index's terms (and any INCLUDE dictionaries') within the
//! Levenshtein distance as suggestions, scored by their document frequency.

use super::ft::verify_acl;
use super::*;
use crate::search::areq;
use crate::search::args::{Ac, GE1};
use crate::search::error::{Code, QErr};
use crate::search::index::Index;
use crate::search::levenshtein::MatchMode;
use crate::search::qast::{self, Kind, Node};
use crate::search::qparse::{self, ParseOpts};
use crate::search::trie::{self, SortMode, Trie};
use crate::util::cstr;
use std::cmp::Ordering;

const MAX_LEV_DISTANCE: i64 = 4;
const FOUND_IN_INDEX: &str = "term exists in index";

/// `RMUtil_ArgExists`: the first argument from `offset` equal to `name` (ignoring case), 0
/// for none.
fn arg_exists(args: &[Bytes], name: &str, offset: usize) -> usize {
    (offset..args.len()).find(|&i| args[i].eq_ignore_ascii_case(name.as_bytes())).unwrap_or(0)
}

/// `parseDialect`.
fn parse_dialect(args: &[Bytes]) -> Result<u32, QErr> {
    let mut ac = Ac::new(args);
    if ac.remaining() < 1 {
        return Err(QErr::new(Code::ParseArgs, "Need an argument for DIALECT"));
    }
    match ac.get_u32(GE1) {
        Ok(d) if d <= 4 => Ok(d),
        _ => Err(QErr::new(Code::ParseArgs, "DIALECT requires a non negative integer >=1 and <= 4")),
    }
}

/// `SpellCheck_IsTermExistsInTrie`: an exact (case-folded) match, and its score.
fn exists_in(t: &Trie, term: &[u8]) -> Option<f32> {
    let mut it = t.iterate_fuzzy(term, 0, MatchMode::EditDistance)?;
    it.next_entry().map(|e| e.score)
}

/// A dictionary's trie (`Trie_Sort_Lex`, as FT.DICTADD builds it).
fn dict_trie(terms: &std::collections::BTreeSet<Vec<u16>>) -> Trie {
    let mut t = Trie::new(SortMode::Lex);
    for r in terms {
        t.insert_runes(r, 1.0, true, None, 0);
    }
    t
}

struct Ctx<'a> {
    ix: &'a Index,
    include: Vec<Bytes>,
    exclude: Vec<Bytes>,
    dicts: &'a mut crate::search::rsdict::RsDict<std::collections::BTreeSet<Vec<u16>>>,
    distance: i32,
    full: bool,
    resp3: bool,
}

impl Ctx<'_> {
    fn open_dict(&mut self, name: &[u8]) -> Option<Trie> {
        self.dicts.find(cstr(name)).map(dict_trie)
    }

    /// `SpellCheck_GetScore`: the term's document count, 0 when it has no index, -1 when no
    /// record is in the term node's fields.
    fn score(&self, term: &[u8], mask: u128) -> f64 {
        let Some(ii) = self.ix.terms.get(term) else { return 0.0 };
        let filter = ii.flags() & crate::search::invidx::flags::STORE_FIELD_FLAGS != 0;
        let mut rd = ii.reader();
        while let Some(rec) = rd.next_record() {
            if !filter || rec.field_mask & mask != 0 {
                return ii.unique_docs() as f64;
            }
        }
        -1.0
    }

    /// `SpellCheck_FindSuggestions`.
    fn find(&self, t: &Trie, term: &[u8], mask: u128, s: &mut Trie, incr: bool) {
        let Some(mut it) = t.iterate_fuzzy(term, self.distance, MatchMode::EditDistance) else { return };
        while let Some(e) = it.next_entry() {
            let sugg = trie::runes_to_str(e.runes).unwrap_or_default();
            let score = self.score(&sugg, mask);
            if score != -1.0 {
                suggestions_add(s, &sugg, score, incr);
            }
        }
    }

    /// `SpellCheck_ReplyTermSuggestions`: the term's entry, `None` when it gets none.
    fn term(&mut self, term: &[u8], mask: u128) -> Option<(Reply, Reply)> {
        if exists_in(&self.ix.term_trie, term).is_some() {
            if !self.full {
                return None;
            }
            let t = Reply::Bulk(Bytes::copy_from_slice(term));
            if self.resp3 {
                return Some((t, Reply::Error(FOUND_IN_INDEX.into())));
            }
            return Some((Reply::Array(vec![Reply::Status("TERM".into()), t, Reply::Status(FOUND_IN_INDEX.into())]), Reply::Nil));
        }
        for name in self.exclude.clone() {
            let Some(t) = self.open_dict(&name) else { continue };
            if exists_in(&t, term).is_some() {
                return None;
            }
        }
        let mut s = Trie::new(SortMode::Score);
        self.find(&self.ix.term_trie, term, mask, &mut s, true);
        for name in self.include.clone() {
            let Some(t) = self.open_dict(&name) else { continue };
            self.find(&t, term, mask, &mut s, false);
        }
        let total = if self.full { 0 } else { self.ix.docs.size as u64 };
        Some(self.reply_term(term, &s, total))
    }

    /// `SpellCheck_SendReplyOnTerm`: the suggestions by score (musl's `qsort`), each score over
    /// the document count.
    fn reply_term(&self, term: &[u8], s: &Trie, total: u64) -> (Reply, Reply) {
        let total = total.max(1) as f64;
        let mut sugg: Vec<(Vec<u8>, f64)> = s.iterate_all().collect_all().into_iter().map(|(r, score, _, _)| (trie::runes_to_str(&r).unwrap_or_default(), score as f64)).collect();
        let mut order: Vec<usize> = (0..sugg.len()).collect();
        crate::tre::qsort::qsort(&mut order, |&a, &b| {
            let (x, y) = (sugg[a].1, sugg[b].1);
            if x > y {
                Ordering::Less
            } else if x < y {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        let list: Vec<(Vec<u8>, f64)> = order.into_iter().map(|i| std::mem::take(&mut sugg[i])).collect();
        let t = Reply::Bulk(Bytes::copy_from_slice(term));
        if self.resp3 {
            let items = list.into_iter().map(|(w, sc)| Reply::Map(vec![(Reply::Bulk(Bytes::from(w)), Reply::Double(sc / total))])).collect();
            return (t, Reply::Array(items));
        }
        let items = list.into_iter().map(|(w, sc)| Reply::Array(vec![Reply::Double(sc / total), Reply::Bulk(Bytes::from(w))])).collect();
        (Reply::Array(vec![Reply::Status("TERM".into()), t, Reply::Array(items)]), Reply::Nil)
    }

    /// `QueryNode_ForEach(root, forEachCallback, ctx, 1)`: the tree in pre-order.
    fn walk(&mut self, root: &Node, out: &mut Vec<(Reply, Reply)>) {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if let Kind::Token(t) = &n.kind
                && let Some(r) = self.term(&t.s, n.opts.field_mask)
            {
                out.push(r);
            }
            for c in n.children.iter().rev() {
                stack.push(c);
            }
        }
    }
}

/// `RS_SuggestionsAdd`: index terms add up (a zero score is replaced), dictionary terms are
/// only added when new.
fn suggestions_add(s: &mut Trie, term: &[u8], score: f64, incr: bool) {
    let cur = exists_in(s, term);
    if !incr {
        if cur.is_none() {
            s.insert(term, score, false, None, 0);
        }
        return;
    }
    if cur.is_some() && score == 0.0 {
        return;
    }
    let incr = cur.is_some_and(|c| c != 0.0);
    s.insert(term, score, incr, None, 0);
}

/// FT.SPELLCHECK index query [DISTANCE d] [TERMS INCLUDE|EXCLUDE dict]... [DIALECT d]
/// [PARAMS n name value ...].
pub(super) fn ft_spellcheck(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("FT.SPELLCHECK"));
    }
    verify_acl(cx, &args[1])?;
    let mut dialect = cx.eng.cfg.num("search-default-dialect") as u32;
    let di = arg_exists(args, "DIALECT", 3);
    if di > 0 {
        dialect = parse_dialect(&args[di + 1..])?;
    }
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(super::ft::not_found(&args[1]));
    };
    let mut params = None;
    let pi = arg_exists(args, "PARAMS", 3);
    if pi > 0 {
        let mut ac = Ac::new(&args[pi + 1..]);
        areq::parse_params(&mut params, &mut ac)?;
    }
    let search = &mut cx.eng.search;
    let ix = search.indexes.get(&name).expect("loaded");
    let parsed = qparse::parse(&args[2], dialect, &ix.spec, &ParseOpts { stopwords: None, params: params.as_ref() })?;
    let mut root = parsed.root.unwrap_or_else(|| Node::new(Kind::Null));
    if parsed.num_params > 0 {
        qast::eval_params(&mut root, params.as_ref(), dialect)?;
    }
    let mut distance = 1i64;
    let dp = arg_exists(args, "DISTANCE", 0);
    if dp != 0 {
        if dp + 1 >= args.len() {
            return Err(Error::new("DISTANCE arg is given but no DISTANCE comes after"));
        }
        match crate::util::parse_i64(&args[dp + 1]) {
            Some(d) if (1..=MAX_LEV_DISTANCE).contains(&d) => distance = d,
            _ => return Err(Error::new("bad distance given, distance must be a natural number between 1 to 4")),
        }
    }
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let mut next = 0;
    loop {
        next = arg_exists(args, "TERMS", next + 1);
        if next == 0 {
            break;
        }
        if next + 2 >= args.len() {
            return Err(Error::new("TERM arg is given but no TERM params comes after"));
        }
        let op = cstr(&args[next + 1]);
        let dict = Bytes::copy_from_slice(cstr(&args[next + 2]));
        if op.eq_ignore_ascii_case(b"INCLUDE") {
            include.push(dict);
        } else if op.eq_ignore_ascii_case(b"EXCLUDE") {
            exclude.push(dict);
        } else {
            return Err(Error::new("bad format, exclude/include operation was not given"));
        }
    }
    let full = arg_exists(args, "FULLSCOREINFO", 0) != 0;
    let resp3 = cx.client.resp >= 3;
    if let Some(ix) = search.indexes.get_mut(&name) {
        ix.used_dialects |= 1 << (dialect.clamp(1, 8) - 1);
    }
    let ix = search.indexes.get(&name).expect("loaded");
    // `SpellCheck_CheckTermDictsExistance`
    for d in include.iter().chain(exclude.iter()) {
        if search.dicts.find(d).is_none() {
            let mut msg = format!("Dict does not exist: {}", String::from_utf8_lossy(d)).into_bytes();
            msg.truncate(999);
            return Err(Error::msg(String::from_utf8_lossy(&msg).into_owned()));
        }
    }
    let total_docs = ix.docs.size as i64;
    let mut c = Ctx { ix, include, exclude, dicts: &mut search.dicts, distance: distance as i32, full, resp3 };
    let mut out = Vec::new();
    c.walk(&root, &mut out);
    if resp3 {
        let mut m = Vec::new();
        if full {
            m.push((Reply::Status("total_docs".into()), Reply::Int(total_docs)));
        }
        m.push((Reply::Status("results".into()), Reply::Map(out)));
        return Ok(Reply::Map(m));
    }
    let mut a = Vec::with_capacity(out.len() + 1);
    if full {
        a.push(Reply::Int(total_docs));
    }
    a.extend(out.into_iter().map(|(r, _)| r));
    Ok(Reply::Array(a))
}
