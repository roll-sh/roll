//! Filters that route tokens through other filters: `condition` (Lucene's
//! `ConditionalTokenFilter` with a script predicate), `predicate_token_filter` and `multiplexer`.
//!
//! A conditional filter runs a sub-chain over the runs of consecutive tokens its predicate
//! accepts: the sub-chain reads from a one-time source that ends at the first rejected token,
//! which is buffered and emitted after the sub-chain's `end()` (with the sub-chain's trailing
//! position increment added). The multiplexer stacks `n` such conditionals over a source that
//! repeats every token `n` times, each branch filtering one copy, then removes duplicates. The
//! sub-chains are ordinary boxed filters fed through [`Upstream`], so any filter works inside.

use std::sync::Arc;

use super::basic::RemoveDuplicates;
use crate::component::{
    AnalysisMode, ChainParts, FilterFactory, FilterResolver, NamedFilter, SynonymRole,
};
use crate::error::{AnalysisError, Result};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream, Upstream};
use crate::token::{Tok, attr};
use crate::{ScriptToken, TokenPredicate};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Reading,
    Prebuffering,
    Delegating,
}

/// What a conditional reads from: its input stream plus its predicate.
trait Below {
    fn next(&mut self, t: &mut Tok) -> bool;
    fn end(&mut self, t: &mut Tok);
    /// The conditional's `shouldFilter()` for the token in `t`.
    fn should(&mut self, t: &mut Tok) -> bool;
}

/// The state of a `ConditionalTokenFilter`, apart from its sub-chain.
#[derive(Default)]
struct CondState {
    state: Option<State>,
    last_filtered: bool,
    buffered: Tok,
    has_buffered: bool,
    exhausted: bool,
    adjust_position: bool,
    end_state: Tok,
    has_end_state: bool,
    end_offset: u32,
}

impl CondState {
    fn st(&self) -> State {
        self.state.unwrap_or(State::Reading)
    }
}

/// The one-time source the sub-chain reads from (`OneTimeWrapper`).
struct OneTime<'a, B: Below + ?Sized> {
    st: &'a mut CondState,
    below: &'a mut B,
}

impl<B: Below + ?Sized> TokenStream for OneTime<'_, B> {
    fn next(&mut self, t: &mut Tok) -> bool {
        if self.st.st() == State::Prebuffering {
            if t.pos_inc == 0 {
                self.st.adjust_position = true;
                t.pos_inc = 1;
            }
            self.st.state = Some(State::Delegating);
            return true;
        }
        if self.below.next(t) {
            if self.below.should(t) {
                return true;
            }
            self.st.end_offset = t.end;
            self.st.buffered.clone_from(t);
            self.st.has_buffered = true;
        } else {
            self.st.exhausted = true;
        }
        false
    }

    fn end(&mut self, t: &mut Tok) {
        if self.st.exhausted {
            if !self.st.has_end_state {
                t.clear_for_end();
                self.below.end(t);
                self.st.end_state.clone_from(t);
                self.st.has_end_state = true;
            }
            self.st.end_offset = self.st.end_state.end;
        }
        t.clear_for_end();
        t.start = self.st.end_offset;
        t.end = self.st.end_offset;
    }
}

/// A `ConditionalTokenFilter`: state plus the sub-chain.
struct Cond {
    st: CondState,
    delegate: Vec<Box<dyn Filter>>,
}

impl Cond {
    fn new(delegate: Vec<Box<dyn Filter>>) -> Cond {
        Cond {
            st: CondState::default(),
            delegate,
        }
    }

    fn reset(&mut self) {
        for f in &mut self.delegate {
            f.reset();
        }
        self.st.state = Some(State::Reading);
        self.st.last_filtered = false;
        self.st.has_buffered = false;
        self.st.exhausted = false;
        self.st.adjust_position = false;
        self.st.end_offset = 0;
        self.st.has_end_state = false;
    }

    fn delegate_next<B: Below + ?Sized>(&mut self, below: &mut B, t: &mut Tok) -> bool {
        let mut src = OneTime {
            st: &mut self.st,
            below,
        };
        Upstream {
            filters: &mut self.delegate,
            src: &mut src,
        }
        .next(t)
    }

    fn delegate_end<B: Below + ?Sized>(&mut self, below: &mut B, t: &mut Tok) {
        let mut src = OneTime {
            st: &mut self.st,
            below,
        };
        Upstream {
            filters: &mut self.delegate,
            src: &mut src,
        }
        .end(t)
    }

    fn end_delegating<B: Below + ?Sized>(&mut self, below: &mut B, t: &mut Tok) -> bool {
        if !self.st.has_buffered {
            return false;
        }
        t.clear_for_end();
        self.delegate_end(below, t);
        let pos_inc = t.pos_inc;
        t.clone_from(&self.st.buffered);
        t.pos_inc += pos_inc;
        if self.st.adjust_position {
            t.pos_inc = t.pos_inc.saturating_sub(1);
            self.st.adjust_position = false;
        }
        self.st.has_buffered = false;
        true
    }

    fn next<B: Below + ?Sized>(&mut self, below: &mut B, t: &mut Tok) -> bool {
        self.st.last_filtered = false;
        if t.error.is_some() {
            return false;
        }
        match self.st.st() {
            State::Reading => {
                if self.st.has_buffered {
                    t.clone_from(&self.st.buffered);
                    self.st.has_buffered = false;
                    self.st.last_filtered = false;
                    return true;
                }
                if self.st.exhausted {
                    return false;
                }
                if !below.next(t) {
                    self.st.exhausted = true;
                    return false;
                }
                if below.should(t) {
                    self.st.last_filtered = true;
                    self.st.state = Some(State::Prebuffering);
                    for f in &mut self.delegate {
                        f.reset();
                    }
                    if self.delegate_next(below, t) {
                        self.st.state = Some(State::Delegating);
                        if self.st.adjust_position {
                            t.pos_inc = t.pos_inc.saturating_sub(1);
                        }
                        self.st.adjust_position = false;
                    } else {
                        self.st.state = Some(State::Reading);
                        return self.end_delegating(below, t);
                    }
                    return true;
                }
                true
            }
            State::Delegating | State::Prebuffering => {
                self.st.last_filtered = true;
                if self.delegate_next(below, t) {
                    return true;
                }
                self.st.state = Some(State::Reading);
                self.end_delegating(below, t)
            }
        }
    }

    fn end<B: Below + ?Sized>(&mut self, below: &mut B, t: &mut Tok) {
        if !self.st.has_end_state {
            t.clear_for_end();
            below.end(t);
            self.st.end_state.clone_from(t);
            self.st.has_end_state = true;
        } else {
            t.clone_from(&self.st.end_state);
        }
        self.st.end_offset = t.end;
        if self.st.last_filtered {
            t.clear_for_end();
            self.delegate_end(below, t);
            self.st.end_state.clone_from(t);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Script predicates.

/// `AnalysisPredicateScript.Token` bookkeeping: the absolute position of the current token.
struct ScriptState {
    script: Arc<dyn TokenPredicate>,
    position: i64,
}

impl ScriptState {
    fn test(&mut self, t: &mut Tok) -> bool {
        self.position += t.pos_inc as i64;
        let tok = ScriptToken {
            term: &t.term,
            position: self.position,
            position_increment: t.pos_inc,
            position_length: t.pos_len,
            start_offset: t.start,
            end_offset: t.end,
            kind: t.kind,
            keyword: t.keyword,
        };
        match self.script.test(&tok) {
            Ok(b) => b,
            Err(e) => {
                t.error = Some(Box::new(e));
                false
            }
        }
    }
}

struct InputBelow<'a> {
    input: &'a mut dyn TokenStream,
    script: &'a mut ScriptState,
}

impl Below for InputBelow<'_> {
    fn next(&mut self, t: &mut Tok) -> bool {
        self.input.next(t)
    }
    fn end(&mut self, t: &mut Tok) {
        self.input.end(t)
    }
    fn should(&mut self, t: &mut Tok) -> bool {
        self.script.test(t)
    }
}

/// `condition`.
struct Condition {
    cond: Cond,
    script: ScriptState,
}

impl Filter for Condition {
    fn reset(&mut self) {
        self.cond.reset();
        self.script.position = -1;
    }
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let mut below = InputBelow {
            input,
            script: &mut self.script,
        };
        self.cond.next(&mut below, t)
    }
    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        let mut below = InputBelow {
            input,
            script: &mut self.script,
        };
        self.cond.end(&mut below, t)
    }
}

/// `predicate_token_filter`: keeps tokens the script accepts.
struct Predicate {
    script: ScriptState,
    skipped: u32,
}

impl Filter for Predicate {
    fn reset(&mut self) {
        self.script.position = -1;
        self.skipped = 0;
    }
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        self.skipped = 0;
        while input.next(t) {
            if self.script.test(t) {
                t.pos_inc += self.skipped;
                return true;
            }
            if t.error.is_some() {
                return false;
            }
            self.skipped += t.pos_inc;
        }
        false
    }
    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        input.end(t);
        t.pos_inc += self.skipped;
    }
}

/// The `script` setting as JSON for the [`crate::ScriptHost`]; Elasticsearch's
/// `Script.parse(settings.getAsSettings("script"))` errors.
fn script_json(name: &str, s: &Settings) -> Result<serde_json::Value> {
    let sub = s.by_prefix("script");
    let mut m = serde_json::Map::new();
    let mut params = serde_json::Map::new();
    for k in sub.keys() {
        let v = match sub.get_list(k) {
            Some(l) if l.len() != 1 || sub.get(k).is_none() => {
                serde_json::Value::Array(l.into_iter().map(serde_json::Value::String).collect())
            }
            _ => sub
                .get(k)
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        };
        if let Some(p) = k.strip_prefix("params.") {
            params.insert(p.to_string(), v);
        } else {
            m.insert(k.to_string(), v);
        }
    }
    if !params.is_empty() {
        m.insert("params".into(), serde_json::Value::Object(params));
    }
    if m.contains_key("id") && !m.contains_key("source") {
        return Err(AnalysisError::illegal(format!(
            "Cannot use stored scripts in tokenfilter [{name}]"
        )));
    }
    if !m.contains_key("source") {
        return Err(AnalysisError::illegal(
            "must specify either [source] for an inline script or [id] for a stored script",
        ));
    }
    Ok(serde_json::Value::Object(m))
}

fn compile_script(
    ctx: &crate::registry::Ctx,
    name: &str,
    s: &Settings,
) -> Result<Arc<dyn TokenPredicate>> {
    let json = script_json(name, s)?;
    match &ctx.opts.scripts {
        Some(host) => host.compile_predicate(&json),
        None => Err(AnalysisError::illegal(format!(
            "scripts are not available to analysis component [{name}]"
        ))),
    }
}

struct ConditionFactory {
    name: Arc<str>,
    script: Arc<dyn TokenPredicate>,
    filters: Vec<String>,
}

struct BuiltCondition {
    script: Arc<dyn TokenPredicate>,
    filters: Vec<NamedFilter>,
    mode: AnalysisMode,
}

impl FilterFactory for BuiltCondition {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(Condition {
            cond: Cond::new(self.filters.iter().map(|f| f.factory.create()).collect()),
            script: ScriptState {
                script: self.script.clone(),
                position: -1,
            },
        })
    }
    fn check(&self) -> Result<()> {
        self.filters.iter().try_for_each(|f| f.factory.check())
    }
    fn attrs(&self) -> u32 {
        self.filters
            .iter()
            .fold(attr::KEYWORD, |a, f| a | f.factory.attrs())
    }
    fn mode(&self) -> AnalysisMode {
        self.mode
    }
}

/// Enters a referring filter, failing on a reference cycle (`ReferringFilterResolver.enter`).
fn enter(name: &Arc<str>, referring: &[Arc<str>]) -> Result<Vec<Arc<str>>> {
    if referring.iter().any(|r| r == name) {
        let path: Vec<&str> = referring.iter().map(|r| &**r).collect();
        return Err(AnalysisError::illegal(format!(
            "Token filter [{name}] refers to itself, either directly or via a cycle of filters [{}]",
            path.join(", ")
        )));
    }
    let mut v = referring.to_vec();
    v.push(name.clone());
    Ok(v)
}

/// Resolves `names` as a sub-chain after `prev` (each made chain-aware).
fn resolve_chain(
    prev: &ChainParts<'_>,
    resolve: FilterResolver<'_>,
    path: &[Arc<str>],
    names: &[String],
    undefined: &dyn Fn(&str) -> AnalysisError,
) -> Result<Vec<NamedFilter>> {
    let mut existing: Vec<NamedFilter> = prev.filters.to_vec();
    let mut out = Vec::new();
    for n in names {
        let Some(f) = resolve(n)? else {
            return Err(undefined(n));
        };
        let parts = ChainParts {
            char_filters: prev.char_filters,
            tokenizer: prev.tokenizer,
            filters: &existing,
            referring: path,
        };
        let f = match f.factory.chain_aware(&parts, resolve)? {
            Some(factory) => NamedFilter {
                name: f.name,
                factory,
            },
            None => f,
        };
        existing.push(f.clone());
        out.push(f);
    }
    Ok(out)
}

fn merge_modes(filters: &[NamedFilter]) -> Result<AnalysisMode> {
    let mut mode = AnalysisMode::All;
    for f in filters {
        mode = mode.merge(f.factory.mode()).map_err(|_| {
            AnalysisError::other(
                "illegal_state_exception",
                500,
                "Cannot merge SEARCH_TIME and INDEX_TIME analysis mode.",
            )
        })?;
    }
    Ok(mode)
}

impl FilterFactory for ConditionFactory {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(Condition {
            cond: Cond::new(Vec::new()),
            script: ScriptState {
                script: self.script.clone(),
                position: -1,
            },
        })
    }
    fn attrs(&self) -> u32 {
        attr::KEYWORD
    }
    fn chain_aware(
        &self,
        prev: &ChainParts<'_>,
        resolve: FilterResolver<'_>,
    ) -> Result<Option<Arc<dyn FilterFactory>>> {
        let path = enter(&self.name, prev.referring)?;
        let name = self.name.clone();
        let filters = resolve_chain(prev, resolve, &path, &self.filters, &|f| {
            AnalysisError::illegal(format!(
                "ScriptedConditionTokenFilter [{name}] refers to undefined token filter [{f}]"
            ))
        })?;
        let mode = merge_modes(&filters)?;
        Ok(Some(Arc::new(BuiltCondition {
            script: self.script.clone(),
            filters,
            mode,
        })))
    }
}

/// `condition` (`ScriptedConditionTokenFilterFactory`).
pub(crate) fn condition_factory(
    ctx: &crate::registry::Ctx,
    name: &str,
    s: &Settings,
) -> Result<Arc<dyn FilterFactory>> {
    let script = compile_script(ctx, name, s)?;
    let filters = s.list_or_empty("filter");
    if filters.is_empty() {
        return Err(AnalysisError::illegal(format!(
            "Empty list of filters provided to tokenfilter [{name}]"
        )));
    }
    Ok(Arc::new(ConditionFactory {
        name: name.into(),
        script,
        filters,
    }))
}

struct PredicateFactory {
    script: Arc<dyn TokenPredicate>,
}

impl FilterFactory for PredicateFactory {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(Predicate {
            script: ScriptState {
                script: self.script.clone(),
                position: -1,
            },
            skipped: 0,
        })
    }
    fn attrs(&self) -> u32 {
        attr::KEYWORD
    }
}

/// `predicate_token_filter` (`PredicateTokenFilterScriptFactory`).
pub(crate) fn predicate_factory(
    ctx: &crate::registry::Ctx,
    name: &str,
    s: &Settings,
) -> Result<Arc<dyn FilterFactory>> {
    Ok(Arc::new(PredicateFactory {
        script: compile_script(ctx, name, s)?,
    }))
}

// ---------------------------------------------------------------------------------------------
// multiplexer

/// The multiplexer's source: each input token repeated once per branch.
struct MuxSource {
    n: usize,
    selector: usize,
    state: Tok,
}

impl MuxSource {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.selector + 1 >= self.n {
            self.selector = 0;
            if !input.next(t) {
                return false;
            }
            self.state.clone_from(t);
            return true;
        }
        t.clone_from(&self.state);
        t.pos_inc = 0;
        self.selector += 1;
        true
    }
}

/// Branch conditionals below a given one, with the branch's slot.
struct MuxBelow<'a> {
    conds: &'a mut [Cond],
    src: &'a mut MuxSource,
    input: &'a mut dyn TokenStream,
    slot: usize,
}

impl Below for MuxBelow<'_> {
    fn next(&mut self, t: &mut Tok) -> bool {
        match self.conds.split_last_mut() {
            None => self.src.next(self.input, t),
            Some((last, rest)) => {
                let slot = rest.len();
                last.next(
                    &mut MuxBelow {
                        conds: rest,
                        src: self.src,
                        input: self.input,
                        slot,
                    },
                    t,
                )
            }
        }
    }
    fn end(&mut self, t: &mut Tok) {
        match self.conds.split_last_mut() {
            None => self.input.end(t),
            Some((last, rest)) => {
                let slot = rest.len();
                last.end(
                    &mut MuxBelow {
                        conds: rest,
                        src: self.src,
                        input: self.input,
                        slot,
                    },
                    t,
                )
            }
        }
    }
    fn should(&mut self, _t: &mut Tok) -> bool {
        self.slot == self.src.selector
    }
}

struct MuxCore {
    src: MuxSource,
    conds: Vec<Cond>,
}

struct Multiplexer {
    core: MuxCore,
    dedup: RemoveDuplicates,
}

/// The multiplexer below the duplicate removal, as a stream.
struct MuxStream<'a> {
    m: &'a mut MuxCore,
    input: &'a mut dyn TokenStream,
}

impl TokenStream for MuxStream<'_> {
    fn next(&mut self, t: &mut Tok) -> bool {
        let n = self.m.conds.len();
        let (last, rest) = self.m.conds.split_last_mut().expect("at least one branch");
        last.next(
            &mut MuxBelow {
                conds: rest,
                src: &mut self.m.src,
                input: self.input,
                slot: n - 1,
            },
            t,
        )
    }
    fn end(&mut self, t: &mut Tok) {
        let n = self.m.conds.len();
        let (last, rest) = self.m.conds.split_last_mut().expect("at least one branch");
        last.end(
            &mut MuxBelow {
                conds: rest,
                src: &mut self.m.src,
                input: self.input,
                slot: n - 1,
            },
            t,
        )
    }
}

impl Filter for Multiplexer {
    fn reset(&mut self) {
        for c in &mut self.core.conds {
            c.reset();
        }
        self.core.src.selector = self.core.src.n - 1;
        self.dedup.reset();
    }
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        self.dedup.next(
            &mut MuxStream {
                m: &mut self.core,
                input,
            },
            t,
        )
    }
    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        self.dedup.end(
            &mut MuxStream {
                m: &mut self.core,
                input,
            },
            t,
        )
    }
}

struct MultiplexerFactory {
    name: Arc<str>,
    filters: Vec<String>,
    preserve_original: bool,
}

/// A built multiplexer: its branches (empty = identity).
struct BuiltMultiplexer {
    branches: Vec<Vec<NamedFilter>>,
    mode: AnalysisMode,
}

impl FilterFactory for BuiltMultiplexer {
    fn create(&self) -> Box<dyn Filter> {
        let conds: Vec<Cond> = self
            .branches
            .iter()
            .map(|b| Cond::new(b.iter().map(|f| f.factory.create()).collect()))
            .collect();
        let n = conds.len();
        Box::new(Multiplexer {
            core: MuxCore {
                src: MuxSource {
                    n,
                    selector: n - 1,
                    state: Tok::default(),
                },
                conds,
            },
            dedup: RemoveDuplicates::new(),
        })
    }
    fn check(&self) -> Result<()> {
        self.branches
            .iter()
            .flatten()
            .try_for_each(|f| f.factory.check())
    }
    fn attrs(&self) -> u32 {
        self.branches
            .iter()
            .flatten()
            .fold(0, |a, f| a | f.factory.attrs())
    }
    fn mode(&self) -> AnalysisMode {
        self.mode
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
}

impl FilterFactory for MultiplexerFactory {
    fn create(&self) -> Box<dyn Filter> {
        // Only reachable outside an analyzer; a multiplexer with no branches passes tokens on.
        BuiltMultiplexer {
            branches: vec![Vec::new()],
            mode: AnalysisMode::All,
        }
        .create()
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
    fn chain_aware(
        &self,
        prev: &ChainParts<'_>,
        resolve: FilterResolver<'_>,
    ) -> Result<Option<Arc<dyn FilterFactory>>> {
        let path = enter(&self.name, prev.referring)?;
        let name = self.name.clone();
        let undefined = |f: &str| {
            AnalysisError::illegal(format!(
                "Multiplexing filter [{name}] refers to undefined tokenfilter [{f}]"
            ))
        };
        let mut branches: Vec<Vec<NamedFilter>> = Vec::new();
        if self.preserve_original {
            branches.push(Vec::new());
        }
        let mut all = Vec::new();
        for spec in &self.filters {
            // `Strings.tokenizeToStringArray(filter, ",")`: trimmed, empty parts dropped.
            let parts: Vec<String> = spec
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            // A comma-separated branch is a chain, each filter seeing the ones before it.
            let chain = resolve_chain(prev, resolve, &path, &parts, &undefined)?;
            all.extend(chain.iter().cloned());
            branches.push(chain);
        }
        let mode = merge_modes(&all)?;
        if branches.is_empty() {
            branches.push(Vec::new());
        }
        Ok(Some(Arc::new(BuiltMultiplexer { branches, mode })))
    }
}

/// `multiplexer` (`MultiplexerTokenFilterFactory`).
pub(crate) fn multiplexer_factory(name: &str, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    Ok(Arc::new(MultiplexerFactory {
        name: name.into(),
        filters: s.list_or_empty("filters"),
        preserve_original: s.get_bool("preserve_original", true)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::case::{Lower, Upper};
    use crate::filter::tests::run_filters;

    fn mux(branches: Vec<Vec<Box<dyn Filter>>>) -> Box<dyn Filter> {
        let conds: Vec<Cond> = branches.into_iter().map(Cond::new).collect();
        let n = conds.len();
        Box::new(Multiplexer {
            core: MuxCore {
                src: MuxSource {
                    n,
                    selector: n - 1,
                    state: Tok::default(),
                },
                conds,
            },
            dedup: RemoveDuplicates::new(),
        })
    }

    #[test]
    fn multiplexes_and_dedups() {
        let f = mux(vec![
            Vec::new(),
            vec![Box::new(Lower::default())],
            vec![Box::new(Upper::default())],
        ]);
        let out = run_filters(vec![f], "Ab cd");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![("Ab", 1), ("ab", 0), ("AB", 0), ("cd", 1), ("CD", 0)]
        );
    }

    struct Long;
    impl TokenPredicate for Long {
        fn test(&self, t: &ScriptToken<'_>) -> Result<bool> {
            Ok(t.term.len() > 2)
        }
    }

    #[test]
    fn condition_runs_sub_chain_on_matching_runs() {
        let f = Condition {
            cond: Cond::new(vec![Box::new(Upper::default())]),
            script: ScriptState {
                script: Arc::new(Long),
                position: -1,
            },
        };
        let out = run_filters(vec![Box::new(f)], "a abc abcd b xyz");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![("a", 1), ("ABC", 1), ("ABCD", 1), ("b", 1), ("XYZ", 1)]
        );
        let stop = Arc::new(crate::filter::WordSet::new(["abc"], false));
        let f = Condition {
            cond: Cond::new(vec![Box::new(crate::filter::basic::stop(stop))]),
            script: ScriptState {
                script: Arc::new(Long),
                position: -1,
            },
        };
        let out = run_filters(vec![Box::new(f)], "a abc abcd b");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(terms, vec![("a", 1), ("abcd", 2), ("b", 1)]);
    }
}
