//! RediSearch 8.10.1's two query grammars as [`Grammar`] data: dialect 1
//! (`src/query_parser/v1/parser.y`) and dialects 2 to 4 (`src/query_parser/v2/parser.y`).
//!
//! Each grammar declares lemon's symbols in lemon's numbering (the generated `parser.h` for the
//! terminals, `parser.out` for the nonterminals, which lemon numbers by first appearance,
//! `%type` lines included), the precedence lines, `%fallback` and `%stack_size`, then the rules
//! in file order, so [`Grammar::build`] reproduces lemon's automaton state for state. Rule ids
//! are lemon's: the `v1rule`/`v2rule` constants name them, with each rule's text and its line in
//! `parser.y`, for attaching the semantic actions. Rules without code (v2's `star` and `as`)
//! are numbered after all the others, as lemon does.
//!
//! Both grammars have no `error` symbol, and RediSearch does not define `YYNOERRORRECOVERY`:
//! drive them with [`Parser::new`](super::lalr::Parser::new). Their `%syntax_error` reports the
//! offending token's offset; v2's `%stack_overflow` reports "Parser stack overflow" (v1 has no
//! overflow code, so an overflow there just empties the stack).

use std::sync::OnceLock;

use super::lalr::{Grammar, RuleId, Symbol, Tables};

/// Dialect 1 terminals, numbered as lemon numbers them (`v1/parser.h`).
pub mod v1tok {
    use super::Symbol;

    /// End of input.
    pub const EOF: Symbol = 0;
    pub const LOWEST: Symbol = 1;
    pub const TILDE: Symbol = 2;
    pub const TAGLIST: Symbol = 3;
    pub const QUOTE: Symbol = 4;
    pub const COLON: Symbol = 5;
    pub const MINUS: Symbol = 6;
    pub const NUMBER: Symbol = 7;
    pub const STOPWORD: Symbol = 8;
    pub const TERMLIST: Symbol = 9;
    pub const TERM: Symbol = 10;
    pub const PREFIX: Symbol = 11;
    pub const SUFFIX: Symbol = 12;
    pub const CONTAINS: Symbol = 13;
    pub const PERCENT: Symbol = 14;
    pub const ATTRIBUTE: Symbol = 15;
    pub const LP: Symbol = 16;
    pub const RP: Symbol = 17;
    pub const MODIFIER: Symbol = 18;
    pub const AND: Symbol = 19;
    pub const OR: Symbol = 20;
    pub const ORX: Symbol = 21;
    pub const ARROW: Symbol = 22;
    pub const STAR: Symbol = 23;
    pub const SEMICOLON: Symbol = 24;
    pub const LB: Symbol = 25;
    pub const RB: Symbol = 26;
    pub const LSQB: Symbol = 27;
    pub const RSQB: Symbol = 28;
}

/// Dialect 1 nonterminals, numbered as lemon numbers them (`parser.out`).
pub mod v1nt {
    use super::Symbol;

    pub const EXPR: Symbol = 29;
    pub const ATTRIBUTE: Symbol = 30;
    pub const ATTRIBUTE_LIST: Symbol = 31;
    pub const AFFIX: Symbol = 32;
    pub const TERMLIST: Symbol = 33;
    pub const UNION: Symbol = 34;
    pub const FUZZY: Symbol = 35;
    pub const TAG_LIST: Symbol = 36;
    pub const GEO_FILTER: Symbol = 37;
    pub const MODIFIERLIST: Symbol = 38;
    pub const NUM: Symbol = 39;
    pub const NUMERIC_RANGE: Symbol = 40;
    pub const QUERY: Symbol = 41;
    pub const MODIFIER: Symbol = 42;
    pub const TERM: Symbol = 43;
}

/// Dialect 1 rules by lemon number, with their text and `src/query_parser/v1/parser.y` line.
pub mod v1rule {
    use super::RuleId;

    /// `query ::= expr.` (line 165)
    pub const QUERY_EXPR: RuleId = 0;
    /// `query ::=.` (line 171)
    pub const QUERY_EMPTY: RuleId = 1;
    /// `query ::= STAR.` (line 175)
    pub const QUERY_STAR: RuleId = 2;
    /// `expr ::= expr expr. [AND]` (line 183)
    pub const EXPR_EXPR_EXPR: RuleId = 3;
    /// `expr ::= union. [ORX]` (line 206)
    pub const EXPR_UNION: RuleId = 4;
    /// `union ::= expr OR expr. [OR]` (line 210)
    pub const UNION_EXPR_OR_EXPR: RuleId = 5;
    /// `union ::= union OR expr. [ORX]` (line 229)
    pub const UNION_UNION_OR_EXPR: RuleId = 6;
    /// `expr ::= modifier COLON expr. [MODIFIER]` (line 252)
    pub const EXPR_MODIFIER_COLON_EXPR: RuleId = 7;
    /// `expr ::= modifierlist COLON expr. [MODIFIER]` (line 264)
    pub const EXPR_MODIFIERLIST_COLON_EXPR: RuleId = 8;
    /// `expr ::= LP expr RP.` (line 291)
    pub const EXPR_LP_EXPR_RP: RuleId = 9;
    /// `attribute ::= ATTRIBUTE COLON term.` (line 299)
    pub const ATTRIBUTE_ATTRIBUTE_COLON_TERM: RuleId = 10;
    /// `attribute_list ::= attribute.` (line 303)
    pub const ATTRIBUTE_LIST_ATTRIBUTE: RuleId = 11;
    /// `attribute_list ::= attribute_list SEMICOLON attribute.` (line 308)
    pub const ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE: RuleId = 12;
    /// `attribute_list ::= attribute_list SEMICOLON.` (line 313)
    pub const ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON: RuleId = 13;
    /// `attribute_list ::=.` (line 317)
    pub const ATTRIBUTE_LIST_EMPTY: RuleId = 14;
    /// `expr ::= expr ARROW LB attribute_list RB.` (line 321)
    pub const EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB: RuleId = 15;
    /// `expr ::= QUOTE termlist QUOTE. [TERMLIST]` (line 334)
    pub const EXPR_QUOTE_TERMLIST_QUOTE: RuleId = 16;
    /// `expr ::= QUOTE term QUOTE. [TERMLIST]` (line 341)
    pub const EXPR_QUOTE_TERM_QUOTE: RuleId = 17;
    /// `expr ::= term. [LOWEST]` (line 347)
    pub const EXPR_TERM: RuleId = 18;
    /// `expr ::= affix. [PREFIX]` (line 351)
    pub const EXPR_AFFIX: RuleId = 19;
    /// `expr ::= termlist. [TERMLIST]` (line 355)
    pub const EXPR_TERMLIST: RuleId = 20;
    /// `expr ::= STOPWORD. [STOPWORD]` (line 359)
    pub const EXPR_STOPWORD: RuleId = 21;
    /// `termlist ::= term term. [TERMLIST]` (line 363)
    pub const TERMLIST_TERM_TERM: RuleId = 22;
    /// `termlist ::= termlist term. [TERMLIST]` (line 369)
    pub const TERMLIST_TERMLIST_TERM: RuleId = 23;
    /// `termlist ::= termlist STOPWORD. [TERMLIST]` (line 374)
    pub const TERMLIST_TERMLIST_STOPWORD: RuleId = 24;
    /// `expr ::= MINUS expr.` (line 382)
    pub const EXPR_MINUS_EXPR: RuleId = 25;
    /// `expr ::= TILDE expr.` (line 390)
    pub const EXPR_TILDE_EXPR: RuleId = 26;
    /// `affix ::= PREFIX. [PREFIX]` (line 403)
    pub const AFFIX_PREFIX: RuleId = 27;
    /// `affix ::= SUFFIX. [PREFIX]` (line 407)
    pub const AFFIX_SUFFIX: RuleId = 28;
    /// `affix ::= CONTAINS. [PREFIX]` (line 411)
    pub const AFFIX_CONTAINS: RuleId = 29;
    /// `expr ::= PERCENT term PERCENT. [PREFIX]` (line 420)
    pub const EXPR_PERCENT_TERM_PERCENT: RuleId = 30;
    /// `expr ::= PERCENT PERCENT term PERCENT PERCENT. [PREFIX]` (line 425)
    pub const EXPR_PERCENT_PERCENT_TERM_PERCENT_PERCENT: RuleId = 31;
    /// `expr ::= PERCENT PERCENT PERCENT term PERCENT PERCENT PERCENT. [PREFIX]` (line 430)
    pub const EXPR_PERCENT_PERCENT_PERCENT_TERM_PERCENT_PERCENT_PERCENT: RuleId = 32;
    /// `expr ::= PERCENT STOPWORD PERCENT. [PREFIX]` (line 435)
    pub const EXPR_PERCENT_STOPWORD_PERCENT: RuleId = 33;
    /// `expr ::= PERCENT PERCENT STOPWORD PERCENT PERCENT. [PREFIX]` (line 440)
    pub const EXPR_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT: RuleId = 34;
    /// `expr ::= PERCENT PERCENT PERCENT STOPWORD PERCENT PERCENT PERCENT. [PREFIX]` (line 445)
    pub const EXPR_PERCENT_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT_PERCENT: RuleId = 35;
    /// `modifier ::= MODIFIER.` (line 454)
    pub const MODIFIER_MODIFIER: RuleId = 36;
    /// `modifierlist ::= modifier OR term.` (line 459)
    pub const MODIFIERLIST_MODIFIER_OR_TERM: RuleId = 37;
    /// `modifierlist ::= modifierlist OR term.` (line 467)
    pub const MODIFIERLIST_MODIFIERLIST_OR_TERM: RuleId = 38;
    /// `expr ::= modifier COLON tag_list.` (line 477)
    pub const EXPR_MODIFIER_COLON_TAG_LIST: RuleId = 39;
    /// `tag_list ::= LB term. [TAGLIST]` (line 500)
    pub const TAG_LIST_LB_TERM: RuleId = 40;
    /// `tag_list ::= LB STOPWORD. [TAGLIST]` (line 505)
    pub const TAG_LIST_LB_STOPWORD: RuleId = 41;
    /// `tag_list ::= LB affix. [TAGLIST]` (line 510)
    pub const TAG_LIST_LB_AFFIX: RuleId = 42;
    /// `tag_list ::= LB termlist. [TAGLIST]` (line 515)
    pub const TAG_LIST_LB_TERMLIST: RuleId = 43;
    /// `tag_list ::= tag_list OR term. [TAGLIST]` (line 520)
    pub const TAG_LIST_TAG_LIST_OR_TERM: RuleId = 44;
    /// `tag_list ::= tag_list OR STOPWORD. [TAGLIST]` (line 525)
    pub const TAG_LIST_TAG_LIST_OR_STOPWORD: RuleId = 45;
    /// `tag_list ::= tag_list OR affix. [TAGLIST]` (line 530)
    pub const TAG_LIST_TAG_LIST_OR_AFFIX: RuleId = 46;
    /// `tag_list ::= tag_list OR termlist. [TAGLIST]` (line 535)
    pub const TAG_LIST_TAG_LIST_OR_TERMLIST: RuleId = 47;
    /// `tag_list ::= tag_list RB. [TAGLIST]` (line 541)
    pub const TAG_LIST_TAG_LIST_RB: RuleId = 48;
    /// `expr ::= modifier COLON numeric_range.` (line 550)
    pub const EXPR_MODIFIER_COLON_NUMERIC_RANGE: RuleId = 49;
    /// `numeric_range ::= LSQB num num RSQB. [NUMBER]` (line 563)
    pub const NUMERIC_RANGE_LSQB_NUM_NUM_RSQB: RuleId = 50;
    /// `expr ::= modifier COLON geo_filter.` (line 573)
    pub const EXPR_MODIFIER_COLON_GEO_FILTER: RuleId = 51;
    /// `geo_filter ::= LSQB num num num TERM RSQB. [NUMBER]` (line 586)
    pub const GEO_FILTER_LSQB_NUM_NUM_NUM_TERM_RSQB: RuleId = 52;
    /// `num ::= NUMBER.` (line 598)
    pub const NUM_NUMBER: RuleId = 53;
    /// `num ::= LP num.` (line 603)
    pub const NUM_LP_NUM: RuleId = 54;
    /// `num ::= MINUS num.` (line 608)
    pub const NUM_MINUS_NUM: RuleId = 55;
    /// `term ::= TERM.` (line 613)
    pub const TERM_TERM: RuleId = 56;
    /// `term ::= NUMBER.` (line 617)
    pub const TERM_NUMBER: RuleId = 57;
}

/// `src/query_parser/v1/parser.y`.
pub fn v1() -> Grammar {
    use v1nt as n;
    use v1tok as t;
    let mut g = Grammar::new();
    for name in ["LOWEST", "TILDE", "TAGLIST", "QUOTE", "COLON", "MINUS", "NUMBER", "STOPWORD", "TERMLIST", "TERM", "PREFIX", "SUFFIX", "CONTAINS", "PERCENT", "ATTRIBUTE", "LP", "RP", "MODIFIER", "AND", "OR", "ORX", "ARROW", "STAR", "SEMICOLON", "LB", "RB", "LSQB", "RSQB"] {
        g.terminal(name);
    }
    g.left(&[t::LOWEST]);
    g.left(&[t::TILDE]);
    g.left(&[t::TAGLIST]);
    g.left(&[t::QUOTE]);
    g.left(&[t::COLON]);
    g.left(&[t::MINUS]);
    g.left(&[t::NUMBER]);
    g.left(&[t::STOPWORD]);
    g.left(&[t::TERMLIST]);
    g.left(&[t::TERM]);
    g.left(&[t::PREFIX, t::SUFFIX, t::CONTAINS]);
    g.left(&[t::PERCENT]);
    g.left(&[t::ATTRIBUTE]);
    g.right(&[t::LP]);
    g.left(&[t::RP]);
    g.left(&[t::MODIFIER]);
    g.left(&[t::AND]);
    g.left(&[t::OR]);
    g.left(&[t::ORX]);
    g.left(&[t::ARROW]);
    for name in ["expr", "attribute", "attribute_list", "affix", "termlist", "union", "fuzzy", "tag_list", "geo_filter", "modifierlist", "num", "numeric_range", "query", "modifier", "term"] {
        g.nonterminal(name);
    }
    g.rule(n::QUERY, &[n::EXPR], None); // v1rule::QUERY_EXPR (line 165)
    g.rule(n::QUERY, &[], None); // v1rule::QUERY_EMPTY (line 171)
    g.rule(n::QUERY, &[t::STAR], None); // v1rule::QUERY_STAR (line 175)
    g.rule(n::EXPR, &[n::EXPR, n::EXPR], Some(t::AND)); // v1rule::EXPR_EXPR_EXPR (line 183)
    g.rule(n::EXPR, &[n::UNION], Some(t::ORX)); // v1rule::EXPR_UNION (line 206)
    g.rule(n::UNION, &[n::EXPR, t::OR, n::EXPR], Some(t::OR)); // v1rule::UNION_EXPR_OR_EXPR (line 210)
    g.rule(n::UNION, &[n::UNION, t::OR, n::EXPR], Some(t::ORX)); // v1rule::UNION_UNION_OR_EXPR (line 229)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::EXPR], Some(t::MODIFIER)); // v1rule::EXPR_MODIFIER_COLON_EXPR (line 252)
    g.rule(n::EXPR, &[n::MODIFIERLIST, t::COLON, n::EXPR], Some(t::MODIFIER)); // v1rule::EXPR_MODIFIERLIST_COLON_EXPR (line 264)
    g.rule(n::EXPR, &[t::LP, n::EXPR, t::RP], None); // v1rule::EXPR_LP_EXPR_RP (line 291)
    g.rule(n::ATTRIBUTE, &[t::ATTRIBUTE, t::COLON, n::TERM], None); // v1rule::ATTRIBUTE_ATTRIBUTE_COLON_TERM (line 299)
    g.rule(n::ATTRIBUTE_LIST, &[n::ATTRIBUTE], None); // v1rule::ATTRIBUTE_LIST_ATTRIBUTE (line 303)
    g.rule(n::ATTRIBUTE_LIST, &[n::ATTRIBUTE_LIST, t::SEMICOLON, n::ATTRIBUTE], None); // v1rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE (line 308)
    g.rule(n::ATTRIBUTE_LIST, &[n::ATTRIBUTE_LIST, t::SEMICOLON], None); // v1rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON (line 313)
    g.rule(n::ATTRIBUTE_LIST, &[], None); // v1rule::ATTRIBUTE_LIST_EMPTY (line 317)
    g.rule(n::EXPR, &[n::EXPR, t::ARROW, t::LB, n::ATTRIBUTE_LIST, t::RB], None); // v1rule::EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB (line 321)
    g.rule(n::EXPR, &[t::QUOTE, n::TERMLIST, t::QUOTE], Some(t::TERMLIST)); // v1rule::EXPR_QUOTE_TERMLIST_QUOTE (line 334)
    g.rule(n::EXPR, &[t::QUOTE, n::TERM, t::QUOTE], Some(t::TERMLIST)); // v1rule::EXPR_QUOTE_TERM_QUOTE (line 341)
    g.rule(n::EXPR, &[n::TERM], Some(t::LOWEST)); // v1rule::EXPR_TERM (line 347)
    g.rule(n::EXPR, &[n::AFFIX], Some(t::PREFIX)); // v1rule::EXPR_AFFIX (line 351)
    g.rule(n::EXPR, &[n::TERMLIST], Some(t::TERMLIST)); // v1rule::EXPR_TERMLIST (line 355)
    g.rule(n::EXPR, &[t::STOPWORD], Some(t::STOPWORD)); // v1rule::EXPR_STOPWORD (line 359)
    g.rule(n::TERMLIST, &[n::TERM, n::TERM], Some(t::TERMLIST)); // v1rule::TERMLIST_TERM_TERM (line 363)
    g.rule(n::TERMLIST, &[n::TERMLIST, n::TERM], Some(t::TERMLIST)); // v1rule::TERMLIST_TERMLIST_TERM (line 369)
    g.rule(n::TERMLIST, &[n::TERMLIST, t::STOPWORD], Some(t::TERMLIST)); // v1rule::TERMLIST_TERMLIST_STOPWORD (line 374)
    g.rule(n::EXPR, &[t::MINUS, n::EXPR], None); // v1rule::EXPR_MINUS_EXPR (line 382)
    g.rule(n::EXPR, &[t::TILDE, n::EXPR], None); // v1rule::EXPR_TILDE_EXPR (line 390)
    g.rule(n::AFFIX, &[t::PREFIX], Some(t::PREFIX)); // v1rule::AFFIX_PREFIX (line 403)
    g.rule(n::AFFIX, &[t::SUFFIX], Some(t::PREFIX)); // v1rule::AFFIX_SUFFIX (line 407)
    g.rule(n::AFFIX, &[t::CONTAINS], Some(t::PREFIX)); // v1rule::AFFIX_CONTAINS (line 411)
    g.rule(n::EXPR, &[t::PERCENT, n::TERM, t::PERCENT], Some(t::PREFIX)); // v1rule::EXPR_PERCENT_TERM_PERCENT (line 420)
    g.rule(n::EXPR, &[t::PERCENT, t::PERCENT, n::TERM, t::PERCENT, t::PERCENT], Some(t::PREFIX)); // v1rule::EXPR_PERCENT_PERCENT_TERM_PERCENT_PERCENT (line 425)
    g.rule(n::EXPR, &[t::PERCENT, t::PERCENT, t::PERCENT, n::TERM, t::PERCENT, t::PERCENT, t::PERCENT], Some(t::PREFIX)); // v1rule::EXPR_PERCENT_PERCENT_PERCENT_TERM_PERCENT_PERCENT_PERCENT (line 430)
    g.rule(n::EXPR, &[t::PERCENT, t::STOPWORD, t::PERCENT], Some(t::PREFIX)); // v1rule::EXPR_PERCENT_STOPWORD_PERCENT (line 435)
    g.rule(n::EXPR, &[t::PERCENT, t::PERCENT, t::STOPWORD, t::PERCENT, t::PERCENT], Some(t::PREFIX)); // v1rule::EXPR_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT (line 440)
    g.rule(n::EXPR, &[t::PERCENT, t::PERCENT, t::PERCENT, t::STOPWORD, t::PERCENT, t::PERCENT, t::PERCENT], Some(t::PREFIX)); // v1rule::EXPR_PERCENT_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT_PERCENT (line 445)
    g.rule(n::MODIFIER, &[t::MODIFIER], None); // v1rule::MODIFIER_MODIFIER (line 454)
    g.rule(n::MODIFIERLIST, &[n::MODIFIER, t::OR, n::TERM], None); // v1rule::MODIFIERLIST_MODIFIER_OR_TERM (line 459)
    g.rule(n::MODIFIERLIST, &[n::MODIFIERLIST, t::OR, n::TERM], None); // v1rule::MODIFIERLIST_MODIFIERLIST_OR_TERM (line 467)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::TAG_LIST], None); // v1rule::EXPR_MODIFIER_COLON_TAG_LIST (line 477)
    g.rule(n::TAG_LIST, &[t::LB, n::TERM], Some(t::TAGLIST)); // v1rule::TAG_LIST_LB_TERM (line 500)
    g.rule(n::TAG_LIST, &[t::LB, t::STOPWORD], Some(t::TAGLIST)); // v1rule::TAG_LIST_LB_STOPWORD (line 505)
    g.rule(n::TAG_LIST, &[t::LB, n::AFFIX], Some(t::TAGLIST)); // v1rule::TAG_LIST_LB_AFFIX (line 510)
    g.rule(n::TAG_LIST, &[t::LB, n::TERMLIST], Some(t::TAGLIST)); // v1rule::TAG_LIST_LB_TERMLIST (line 515)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::TERM], Some(t::TAGLIST)); // v1rule::TAG_LIST_TAG_LIST_OR_TERM (line 520)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, t::STOPWORD], Some(t::TAGLIST)); // v1rule::TAG_LIST_TAG_LIST_OR_STOPWORD (line 525)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::AFFIX], Some(t::TAGLIST)); // v1rule::TAG_LIST_TAG_LIST_OR_AFFIX (line 530)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::TERMLIST], Some(t::TAGLIST)); // v1rule::TAG_LIST_TAG_LIST_OR_TERMLIST (line 535)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::RB], Some(t::TAGLIST)); // v1rule::TAG_LIST_TAG_LIST_RB (line 541)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::NUMERIC_RANGE], None); // v1rule::EXPR_MODIFIER_COLON_NUMERIC_RANGE (line 550)
    g.rule(n::NUMERIC_RANGE, &[t::LSQB, n::NUM, n::NUM, t::RSQB], Some(t::NUMBER)); // v1rule::NUMERIC_RANGE_LSQB_NUM_NUM_RSQB (line 563)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::GEO_FILTER], None); // v1rule::EXPR_MODIFIER_COLON_GEO_FILTER (line 573)
    g.rule(n::GEO_FILTER, &[t::LSQB, n::NUM, n::NUM, n::NUM, t::TERM, t::RSQB], Some(t::NUMBER)); // v1rule::GEO_FILTER_LSQB_NUM_NUM_NUM_TERM_RSQB (line 586)
    g.rule(n::NUM, &[t::NUMBER], None); // v1rule::NUM_NUMBER (line 598)
    g.rule(n::NUM, &[t::LP, n::NUM], None); // v1rule::NUM_LP_NUM (line 603)
    g.rule(n::NUM, &[t::MINUS, n::NUM], None); // v1rule::NUM_MINUS_NUM (line 608)
    g.rule(n::TERM, &[t::TERM], None); // v1rule::TERM_TERM (line 613)
    g.rule(n::TERM, &[t::NUMBER], None); // v1rule::TERM_NUMBER (line 617)
    g
}

/// [`v1`]'s tables, built once.
pub fn v1_tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| v1().build().expect("the v1 grammar builds"))
}

/// Dialect 2 to 4 terminals, numbered as lemon numbers them (`v2/parser.h`).
pub mod v2tok {
    use super::Symbol;

    /// End of input.
    pub const EOF: Symbol = 0;
    pub const LOWEST: Symbol = 1;
    pub const TEXTEXPR: Symbol = 2;
    pub const ORX: Symbol = 3;
    pub const OR: Symbol = 4;
    pub const ISMISSING: Symbol = 5;
    pub const MODIFIER: Symbol = 6;
    pub const RP: Symbol = 7;
    pub const RB: Symbol = 8;
    pub const RSQB: Symbol = 9;
    pub const EXACT: Symbol = 10;
    pub const TERM: Symbol = 11;
    pub const QUOTE: Symbol = 12;
    pub const SQUOTE: Symbol = 13;
    pub const LP: Symbol = 14;
    pub const LB: Symbol = 15;
    pub const LSQB: Symbol = 16;
    pub const TILDE: Symbol = 17;
    pub const MINUS: Symbol = 18;
    pub const AND: Symbol = 19;
    pub const ARROW: Symbol = 20;
    pub const COLON: Symbol = 21;
    pub const NOT_EQUAL: Symbol = 22;
    pub const EQUALS: Symbol = 23;
    pub const GE: Symbol = 24;
    pub const GT: Symbol = 25;
    pub const LE: Symbol = 26;
    pub const LT: Symbol = 27;
    pub const NUMBER: Symbol = 28;
    pub const SIZE: Symbol = 29;
    pub const STAR: Symbol = 30;
    pub const TAGLIST: Symbol = 31;
    pub const TERMLIST: Symbol = 32;
    pub const PREFIX: Symbol = 33;
    pub const SUFFIX: Symbol = 34;
    pub const CONTAINS: Symbol = 35;
    pub const PERCENT: Symbol = 36;
    pub const ATTRIBUTE: Symbol = 37;
    pub const VERBATIM: Symbol = 38;
    pub const WILDCARD: Symbol = 39;
    pub const AS_T: Symbol = 40;
    pub const SEMICOLON: Symbol = 41;
}

/// Dialect 2 to 4 nonterminals, numbered as lemon numbers them (`parser.out`).
pub mod v2nt {
    use super::Symbol;

    pub const EXPR: Symbol = 42;
    pub const ATTRIBUTE: Symbol = 43;
    pub const ATTRIBUTE_LIST: Symbol = 44;
    pub const AFFIX: Symbol = 45;
    pub const SUFFIX: Symbol = 46;
    pub const CONTAINS: Symbol = 47;
    pub const VERBATIM: Symbol = 48;
    pub const TERMLIST: Symbol = 49;
    pub const UNION: Symbol = 50;
    pub const TEXT_UNION: Symbol = 51;
    pub const TEXT_EXPR: Symbol = 52;
    pub const FUZZY: Symbol = 53;
    pub const TAG_LIST: Symbol = 54;
    pub const GEO_FILTER: Symbol = 55;
    pub const GEOMETRY_QUERY: Symbol = 56;
    pub const VECTOR_QUERY: Symbol = 57;
    pub const VECTOR_COMMAND: Symbol = 58;
    pub const VECTOR_RANGE_COMMAND: Symbol = 59;
    pub const VECTOR_ATTRIBUTE: Symbol = 60;
    pub const VECTOR_ATTRIBUTE_LIST: Symbol = 61;
    pub const MODIFIER: Symbol = 62;
    pub const MODIFIERLIST: Symbol = 63;
    pub const NUM: Symbol = 64;
    pub const NUMERIC_RANGE: Symbol = 65;
    pub const QUERY: Symbol = 66;
    pub const STAR: Symbol = 67;
    pub const PARAM_TERM: Symbol = 68;
    pub const TERM: Symbol = 69;
    pub const PARAM_TERM_CASE: Symbol = 70;
    pub const PARAM_NUM: Symbol = 71;
    pub const EXCLUSIVE_PARAM_NUM: Symbol = 72;
    pub const VECTOR_SCORE_FIELD: Symbol = 73;
    pub const AS: Symbol = 74;
    pub const PARAM_SIZE: Symbol = 75;
}

/// Dialect 2 to 4 rules by lemon number, with their text and `src/query_parser/v2/parser.y` line.
pub mod v2rule {
    use super::RuleId;

    /// `query ::= expr.` (line 323)
    pub const QUERY_EXPR: RuleId = 0;
    /// `query ::=.` (line 328)
    pub const QUERY_EMPTY: RuleId = 1;
    /// `query ::= star.` (line 332)
    pub const QUERY_STAR: RuleId = 2;
    /// `expr ::= text_expr. [TEXTEXPR]` (line 343)
    pub const EXPR_TEXT_EXPR: RuleId = 3;
    /// `expr ::= expr expr. [AND]` (line 351)
    pub const EXPR_EXPR_EXPR: RuleId = 4;
    /// `expr ::= text_expr expr. [AND]` (line 357)
    pub const EXPR_TEXT_EXPR_EXPR: RuleId = 5;
    /// `expr ::= expr text_expr. [AND]` (line 361)
    pub const EXPR_EXPR_TEXT_EXPR: RuleId = 6;
    /// `text_expr ::= text_expr text_expr. [AND]` (line 367)
    pub const TEXT_EXPR_TEXT_EXPR_TEXT_EXPR: RuleId = 7;
    /// `expr ::= union. [ORX]` (line 375)
    pub const EXPR_UNION: RuleId = 8;
    /// `union ::= expr OR expr. [OR]` (line 379)
    pub const UNION_EXPR_OR_EXPR: RuleId = 9;
    /// `union ::= union OR expr. [OR]` (line 383)
    pub const UNION_UNION_OR_EXPR: RuleId = 10;
    /// `union ::= text_expr OR expr. [OR]` (line 389)
    pub const UNION_TEXT_EXPR_OR_EXPR: RuleId = 11;
    /// `union ::= expr OR text_expr. [OR]` (line 393)
    pub const UNION_EXPR_OR_TEXT_EXPR: RuleId = 12;
    /// `text_expr ::= text_union. [ORX]` (line 397)
    pub const TEXT_EXPR_TEXT_UNION: RuleId = 13;
    /// `text_union ::= text_expr OR text_expr. [OR]` (line 402)
    pub const TEXT_UNION_TEXT_EXPR_OR_TEXT_EXPR: RuleId = 14;
    /// `text_union ::= text_union OR text_expr. [OR]` (line 406)
    pub const TEXT_UNION_TEXT_UNION_OR_TEXT_EXPR: RuleId = 15;
    /// `expr ::= modifier COLON text_expr.` (line 414)
    pub const EXPR_MODIFIER_COLON_TEXT_EXPR: RuleId = 16;
    /// `expr ::= modifierlist COLON text_expr.` (line 429)
    pub const EXPR_MODIFIERLIST_COLON_TEXT_EXPR: RuleId = 17;
    /// `expr ::= LP expr RP.` (line 446)
    pub const EXPR_LP_EXPR_RP: RuleId = 18;
    /// `text_expr ::= LP text_expr RP.` (line 450)
    pub const TEXT_EXPR_LP_TEXT_EXPR_RP: RuleId = 19;
    /// `attribute ::= ATTRIBUTE COLON param_term.` (line 458)
    pub const ATTRIBUTE_ATTRIBUTE_COLON_PARAM_TERM: RuleId = 20;
    /// `attribute_list ::= attribute.` (line 473)
    pub const ATTRIBUTE_LIST_ATTRIBUTE: RuleId = 21;
    /// `attribute_list ::= attribute_list SEMICOLON attribute.` (line 478)
    pub const ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE: RuleId = 22;
    /// `attribute_list ::= attribute_list SEMICOLON.` (line 483)
    pub const ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON: RuleId = 23;
    /// `attribute_list ::=.` (line 487)
    pub const ATTRIBUTE_LIST_EMPTY: RuleId = 24;
    /// `expr ::= expr ARROW LB attribute_list RB.` (line 491)
    pub const EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB: RuleId = 25;
    /// `text_expr ::= text_expr ARROW LB attribute_list RB.` (line 499)
    pub const TEXT_EXPR_TEXT_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB: RuleId = 26;
    /// `text_expr ::= EXACT. [TERMLIST]` (line 511)
    pub const TEXT_EXPR_EXACT: RuleId = 27;
    /// `text_expr ::= QUOTE ATTRIBUTE QUOTE. [TERMLIST]` (line 532)
    pub const TEXT_EXPR_QUOTE_ATTRIBUTE_QUOTE: RuleId = 28;
    /// `text_expr ::= SQUOTE ATTRIBUTE SQUOTE. [TERMLIST]` (line 543)
    pub const TEXT_EXPR_SQUOTE_ATTRIBUTE_SQUOTE: RuleId = 29;
    /// `text_expr ::= param_term. [LOWEST]` (line 554)
    pub const TEXT_EXPR_PARAM_TERM: RuleId = 30;
    /// `text_expr ::= affix. [PREFIX]` (line 562)
    pub const TEXT_EXPR_AFFIX: RuleId = 31;
    /// `text_expr ::= verbatim. [VERBATIM]` (line 566)
    pub const TEXT_EXPR_VERBATIM: RuleId = 32;
    /// `termlist ::= param_term param_term. [TERMLIST]` (line 570)
    pub const TERMLIST_PARAM_TERM_PARAM_TERM: RuleId = 33;
    /// `termlist ::= termlist param_term. [TERMLIST]` (line 576)
    pub const TERMLIST_TERMLIST_PARAM_TERM: RuleId = 34;
    /// `expr ::= MINUS expr.` (line 585)
    pub const EXPR_MINUS_EXPR: RuleId = 35;
    /// `text_expr ::= MINUS text_expr.` (line 589)
    pub const TEXT_EXPR_MINUS_TEXT_EXPR: RuleId = 36;
    /// `expr ::= TILDE expr.` (line 597)
    pub const EXPR_TILDE_EXPR: RuleId = 37;
    /// `text_expr ::= TILDE text_expr.` (line 605)
    pub const TEXT_EXPR_TILDE_TEXT_EXPR: RuleId = 38;
    /// `affix ::= PREFIX.` (line 617)
    pub const AFFIX_PREFIX: RuleId = 39;
    /// `affix ::= SUFFIX.` (line 621)
    pub const AFFIX_SUFFIX: RuleId = 40;
    /// `affix ::= CONTAINS.` (line 625)
    pub const AFFIX_CONTAINS: RuleId = 41;
    /// `verbatim ::= WILDCARD.` (line 633)
    pub const VERBATIM_WILDCARD: RuleId = 42;
    /// `text_expr ::= PERCENT param_term PERCENT. [PREFIX]` (line 641)
    pub const TEXT_EXPR_PERCENT_PARAM_TERM_PERCENT: RuleId = 43;
    /// `text_expr ::= PERCENT PERCENT param_term PERCENT PERCENT. [PREFIX]` (line 645)
    pub const TEXT_EXPR_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT: RuleId = 44;
    /// `text_expr ::= PERCENT PERCENT PERCENT param_term PERCENT PERCENT PERCENT. [PREFIX]` (line 649)
    pub const TEXT_EXPR_PERCENT_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT_PERCENT: RuleId = 45;
    /// `modifier ::= MODIFIER.` (line 657)
    pub const MODIFIER_MODIFIER: RuleId = 46;
    /// `modifierlist ::= modifier OR term.` (line 668)
    pub const MODIFIERLIST_MODIFIER_OR_TERM: RuleId = 47;
    /// `modifierlist ::= modifierlist OR term.` (line 696)
    pub const MODIFIERLIST_MODIFIERLIST_OR_TERM: RuleId = 48;
    /// `expr ::= ISMISSING LP modifier RP.` (line 718)
    pub const EXPR_ISMISSING_LP_MODIFIER_RP: RuleId = 49;
    /// `expr ::= modifier COLON LB tag_list RB.` (line 731)
    pub const EXPR_MODIFIER_COLON_LB_TAG_LIST_RB: RuleId = 50;
    /// `tag_list ::= param_term_case. [TAGLIST]` (line 746)
    pub const TAG_LIST_PARAM_TERM_CASE: RuleId = 51;
    /// `tag_list ::= affix. [TAGLIST]` (line 751)
    pub const TAG_LIST_AFFIX: RuleId = 52;
    /// `tag_list ::= verbatim. [TAGLIST]` (line 756)
    pub const TAG_LIST_VERBATIM: RuleId = 53;
    /// `tag_list ::= termlist. [TAGLIST]` (line 761)
    pub const TAG_LIST_TERMLIST: RuleId = 54;
    /// `tag_list ::= tag_list OR param_term_case. [TAGLIST]` (line 766)
    pub const TAG_LIST_TAG_LIST_OR_PARAM_TERM_CASE: RuleId = 55;
    /// `tag_list ::= tag_list OR affix. [TAGLIST]` (line 771)
    pub const TAG_LIST_TAG_LIST_OR_AFFIX: RuleId = 56;
    /// `tag_list ::= tag_list OR verbatim. [TAGLIST]` (line 776)
    pub const TAG_LIST_TAG_LIST_OR_VERBATIM: RuleId = 57;
    /// `tag_list ::= tag_list OR termlist. [TAGLIST]` (line 781)
    pub const TAG_LIST_TAG_LIST_OR_TERMLIST: RuleId = 58;
    /// `expr ::= modifier COLON numeric_range.` (line 790)
    pub const EXPR_MODIFIER_COLON_NUMERIC_RANGE: RuleId = 59;
    /// `numeric_range ::= LSQB param_num param_num RSQB. [NUMBER]` (line 801)
    pub const NUMERIC_RANGE_LSQB_PARAM_NUM_PARAM_NUM_RSQB: RuleId = 60;
    /// `numeric_range ::= LSQB exclusive_param_num param_num RSQB. [NUMBER]` (line 811)
    pub const NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_PARAM_NUM_RSQB: RuleId = 61;
    /// `numeric_range ::= LSQB param_num exclusive_param_num RSQB. [NUMBER]` (line 821)
    pub const NUMERIC_RANGE_LSQB_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB: RuleId = 62;
    /// `numeric_range ::= LSQB exclusive_param_num exclusive_param_num RSQB. [NUMBER]` (line 831)
    pub const NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB: RuleId = 63;
    /// `numeric_range ::= LSQB param_num RSQB. [NUMBER]` (line 841)
    pub const NUMERIC_RANGE_LSQB_PARAM_NUM_RSQB: RuleId = 64;
    /// `expr ::= modifier NOT_EQUAL param_num.` (line 845)
    pub const EXPR_MODIFIER_NOT_EQUAL_PARAM_NUM: RuleId = 65;
    /// `expr ::= modifier EQUALS param_num.` (line 856)
    pub const EXPR_MODIFIER_EQUALS_PARAM_NUM: RuleId = 66;
    /// `expr ::= modifier GT param_num.` (line 866)
    pub const EXPR_MODIFIER_GT_PARAM_NUM: RuleId = 67;
    /// `expr ::= modifier GE param_num.` (line 876)
    pub const EXPR_MODIFIER_GE_PARAM_NUM: RuleId = 68;
    /// `expr ::= modifier LT param_num.` (line 886)
    pub const EXPR_MODIFIER_LT_PARAM_NUM: RuleId = 69;
    /// `expr ::= modifier LE param_num.` (line 896)
    pub const EXPR_MODIFIER_LE_PARAM_NUM: RuleId = 70;
    /// `expr ::= modifier COLON geo_filter.` (line 910)
    pub const EXPR_MODIFIER_COLON_GEO_FILTER: RuleId = 71;
    /// `geo_filter ::= LSQB param_num param_num param_num param_term RSQB. [NUMBER]` (line 922)
    pub const GEO_FILTER_LSQB_PARAM_NUM_PARAM_NUM_PARAM_NUM_PARAM_TERM_RSQB: RuleId = 72;
    /// `expr ::= modifier COLON geometry_query.` (line 937)
    pub const EXPR_MODIFIER_COLON_GEOMETRY_QUERY: RuleId = 73;
    /// `geometry_query ::= LSQB TERM ATTRIBUTE RSQB.` (line 950)
    pub const GEOMETRY_QUERY_LSQB_TERM_ATTRIBUTE_RSQB: RuleId = 74;
    /// `query ::= expr ARROW LSQB vector_query RSQB.` (line 974)
    pub const QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB: RuleId = 75;
    /// `query ::= text_expr ARROW LSQB vector_query RSQB.` (line 983)
    pub const QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB: RuleId = 76;
    /// `query ::= star ARROW LSQB vector_query RSQB.` (line 992)
    pub const QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB: RuleId = 77;
    /// `vector_query ::= vector_command vector_attribute_list vector_score_field.` (line 1001)
    pub const VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST_VECTOR_SCORE_FIELD: RuleId = 78;
    /// `vector_query ::= vector_command vector_score_field.` (line 1014)
    pub const VECTOR_QUERY_VECTOR_COMMAND_VECTOR_SCORE_FIELD: RuleId = 79;
    /// `vector_query ::= vector_command vector_attribute_list.` (line 1026)
    pub const VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST: RuleId = 80;
    /// `vector_query ::= vector_command.` (line 1032)
    pub const VECTOR_QUERY_VECTOR_COMMAND: RuleId = 81;
    /// `vector_score_field ::= as param_term_case.` (line 1038)
    pub const VECTOR_SCORE_FIELD_AS_PARAM_TERM_CASE: RuleId = 82;
    /// `query ::= expr ARROW LSQB vector_query RSQB ARROW LB attribute_list RB.` (line 1043)
    pub const QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB: RuleId = 83;
    /// `query ::= text_expr ARROW LSQB vector_query RSQB ARROW LB attribute_list RB.` (line 1057)
    pub const QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB: RuleId = 84;
    /// `query ::= star ARROW LSQB vector_query RSQB ARROW LB attribute_list RB.` (line 1071)
    pub const QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB: RuleId = 85;
    /// `vector_command ::= TERM param_size modifier ATTRIBUTE.` (line 1086)
    pub const VECTOR_COMMAND_TERM_PARAM_SIZE_MODIFIER_ATTRIBUTE: RuleId = 86;
    /// `vector_attribute ::= TERM param_term.` (line 1101)
    pub const VECTOR_ATTRIBUTE_TERM_PARAM_TERM: RuleId = 87;
    /// `vector_attribute_list ::= vector_attribute_list vector_attribute.` (line 1113)
    pub const VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE: RuleId = 88;
    /// `vector_attribute_list ::= vector_attribute.` (line 1120)
    pub const VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE: RuleId = 89;
    /// `expr ::= modifier COLON LSQB vector_range_command RSQB.` (line 1128)
    pub const EXPR_MODIFIER_COLON_LSQB_VECTOR_RANGE_COMMAND_RSQB: RuleId = 90;
    /// `vector_range_command ::= TERM param_num ATTRIBUTE.` (line 1139)
    pub const VECTOR_RANGE_COMMAND_TERM_PARAM_NUM_ATTRIBUTE: RuleId = 91;
    /// `num ::= SIZE.` (line 1153)
    pub const NUM_SIZE: RuleId = 92;
    /// `num ::= NUMBER.` (line 1157)
    pub const NUM_NUMBER: RuleId = 93;
    /// `num ::= MINUS num.` (line 1161)
    pub const NUM_MINUS_NUM: RuleId = 94;
    /// `term ::= TERM.` (line 1166)
    pub const TERM_TERM: RuleId = 95;
    /// `term ::= NUMBER.` (line 1171)
    pub const TERM_NUMBER: RuleId = 96;
    /// `term ::= SIZE.` (line 1176)
    pub const TERM_SIZE: RuleId = 97;
    /// `param_term ::= term.` (line 1185)
    pub const PARAM_TERM_TERM: RuleId = 98;
    /// `param_term ::= ATTRIBUTE.` (line 1189)
    pub const PARAM_TERM_ATTRIBUTE: RuleId = 99;
    /// `param_term_case ::= term.` (line 1194)
    pub const PARAM_TERM_CASE_TERM: RuleId = 100;
    /// `param_term_case ::= ATTRIBUTE.` (line 1199)
    pub const PARAM_TERM_CASE_ATTRIBUTE: RuleId = 101;
    /// `param_size ::= SIZE.` (line 1204)
    pub const PARAM_SIZE_SIZE: RuleId = 102;
    /// `param_size ::= ATTRIBUTE.` (line 1209)
    pub const PARAM_SIZE_ATTRIBUTE: RuleId = 103;
    /// `param_num ::= ATTRIBUTE.` (line 1214)
    pub const PARAM_NUM_ATTRIBUTE: RuleId = 104;
    /// `param_num ::= MINUS ATTRIBUTE.` (line 1219)
    pub const PARAM_NUM_MINUS_ATTRIBUTE: RuleId = 105;
    /// `param_num ::= num.` (line 1225)
    pub const PARAM_NUM_NUM: RuleId = 106;
    /// `exclusive_param_num ::= LP num.` (line 1230)
    pub const EXCLUSIVE_PARAM_NUM_LP_NUM: RuleId = 107;
    /// `exclusive_param_num ::= LP ATTRIBUTE.` (line 1235)
    pub const EXCLUSIVE_PARAM_NUM_LP_ATTRIBUTE: RuleId = 108;
    /// `exclusive_param_num ::= LP MINUS ATTRIBUTE.` (line 1240)
    pub const EXCLUSIVE_PARAM_NUM_LP_MINUS_ATTRIBUTE: RuleId = 109;
    /// `star ::= STAR.` (line 337, no action)
    pub const STAR_STAR: RuleId = 110;
    /// `star ::= LP star RP.` (line 339, no action)
    pub const STAR_LP_STAR_RP: RuleId = 111;
    /// `as ::= AS_T.` (line 1036, no action)
    pub const AS_AS_T: RuleId = 112;
}

/// `src/query_parser/v2/parser.y`.
pub fn v2() -> Grammar {
    use v2nt as n;
    use v2tok as t;
    let mut g = Grammar::new();
    for name in ["LOWEST", "TEXTEXPR", "ORX", "OR", "ISMISSING", "MODIFIER", "RP", "RB", "RSQB", "EXACT", "TERM", "QUOTE", "SQUOTE", "LP", "LB", "LSQB", "TILDE", "MINUS", "AND", "ARROW", "COLON", "NOT_EQUAL", "EQUALS", "GE", "GT", "LE", "LT", "NUMBER", "SIZE", "STAR", "TAGLIST", "TERMLIST", "PREFIX", "SUFFIX", "CONTAINS", "PERCENT", "ATTRIBUTE", "VERBATIM", "WILDCARD", "AS_T", "SEMICOLON"] {
        g.terminal(name);
    }
    g.left(&[t::LOWEST]);
    g.left(&[t::TEXTEXPR]);
    g.left(&[t::ORX]);
    g.left(&[t::OR]);
    g.left(&[t::ISMISSING]);
    g.left(&[t::MODIFIER]);
    g.left(&[t::RP, t::RB, t::RSQB]);
    g.left(&[t::EXACT]);
    g.left(&[t::TERM]);
    g.left(&[t::QUOTE, t::SQUOTE]);
    g.left(&[t::LP, t::LB, t::LSQB]);
    g.left(&[t::TILDE, t::MINUS]);
    g.left(&[t::AND]);
    g.left(&[t::ARROW]);
    g.left(&[t::COLON]);
    g.left(&[t::NOT_EQUAL, t::EQUALS]);
    g.left(&[t::GE, t::GT, t::LE, t::LT]);
    g.left(&[t::NUMBER]);
    g.left(&[t::SIZE]);
    g.left(&[t::STAR]);
    g.left(&[t::TAGLIST]);
    g.left(&[t::TERMLIST]);
    g.left(&[t::PREFIX, t::SUFFIX, t::CONTAINS]);
    g.left(&[t::PERCENT]);
    g.left(&[t::ATTRIBUTE]);
    g.left(&[t::VERBATIM, t::WILDCARD]);
    g.fallback(t::TERM, &[t::EXACT, t::AS_T, t::ISMISSING]);
    g.stack_size(256);
    for name in ["expr", "attribute", "attribute_list", "affix", "suffix", "contains", "verbatim", "termlist", "union", "text_union", "text_expr", "fuzzy", "tag_list", "geo_filter", "geometry_query", "vector_query", "vector_command", "vector_range_command", "vector_attribute", "vector_attribute_list", "modifier", "modifierlist", "num", "numeric_range", "query", "star", "param_term", "term", "param_term_case", "param_num", "exclusive_param_num", "vector_score_field", "as", "param_size"] {
        g.nonterminal(name);
    }
    g.rule(n::QUERY, &[n::EXPR], None); // v2rule::QUERY_EXPR (line 323)
    g.rule(n::QUERY, &[], None); // v2rule::QUERY_EMPTY (line 328)
    g.rule(n::QUERY, &[n::STAR], None); // v2rule::QUERY_STAR (line 332)
    g.rule_without_action(n::STAR, &[t::STAR], None); // v2rule::STAR_STAR (line 337)
    g.rule_without_action(n::STAR, &[t::LP, n::STAR, t::RP], None); // v2rule::STAR_LP_STAR_RP (line 339)
    g.rule(n::EXPR, &[n::TEXT_EXPR], Some(t::TEXTEXPR)); // v2rule::EXPR_TEXT_EXPR (line 343)
    g.rule(n::EXPR, &[n::EXPR, n::EXPR], Some(t::AND)); // v2rule::EXPR_EXPR_EXPR (line 351)
    g.rule(n::EXPR, &[n::TEXT_EXPR, n::EXPR], Some(t::AND)); // v2rule::EXPR_TEXT_EXPR_EXPR (line 357)
    g.rule(n::EXPR, &[n::EXPR, n::TEXT_EXPR], Some(t::AND)); // v2rule::EXPR_EXPR_TEXT_EXPR (line 361)
    g.rule(n::TEXT_EXPR, &[n::TEXT_EXPR, n::TEXT_EXPR], Some(t::AND)); // v2rule::TEXT_EXPR_TEXT_EXPR_TEXT_EXPR (line 367)
    g.rule(n::EXPR, &[n::UNION], Some(t::ORX)); // v2rule::EXPR_UNION (line 375)
    g.rule(n::UNION, &[n::EXPR, t::OR, n::EXPR], Some(t::OR)); // v2rule::UNION_EXPR_OR_EXPR (line 379)
    g.rule(n::UNION, &[n::UNION, t::OR, n::EXPR], Some(t::OR)); // v2rule::UNION_UNION_OR_EXPR (line 383)
    g.rule(n::UNION, &[n::TEXT_EXPR, t::OR, n::EXPR], Some(t::OR)); // v2rule::UNION_TEXT_EXPR_OR_EXPR (line 389)
    g.rule(n::UNION, &[n::EXPR, t::OR, n::TEXT_EXPR], Some(t::OR)); // v2rule::UNION_EXPR_OR_TEXT_EXPR (line 393)
    g.rule(n::TEXT_EXPR, &[n::TEXT_UNION], Some(t::ORX)); // v2rule::TEXT_EXPR_TEXT_UNION (line 397)
    g.rule(n::TEXT_UNION, &[n::TEXT_EXPR, t::OR, n::TEXT_EXPR], Some(t::OR)); // v2rule::TEXT_UNION_TEXT_EXPR_OR_TEXT_EXPR (line 402)
    g.rule(n::TEXT_UNION, &[n::TEXT_UNION, t::OR, n::TEXT_EXPR], Some(t::OR)); // v2rule::TEXT_UNION_TEXT_UNION_OR_TEXT_EXPR (line 406)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::TEXT_EXPR], None); // v2rule::EXPR_MODIFIER_COLON_TEXT_EXPR (line 414)
    g.rule(n::EXPR, &[n::MODIFIERLIST, t::COLON, n::TEXT_EXPR], None); // v2rule::EXPR_MODIFIERLIST_COLON_TEXT_EXPR (line 429)
    g.rule(n::EXPR, &[t::LP, n::EXPR, t::RP], None); // v2rule::EXPR_LP_EXPR_RP (line 446)
    g.rule(n::TEXT_EXPR, &[t::LP, n::TEXT_EXPR, t::RP], None); // v2rule::TEXT_EXPR_LP_TEXT_EXPR_RP (line 450)
    g.rule(n::ATTRIBUTE, &[t::ATTRIBUTE, t::COLON, n::PARAM_TERM], None); // v2rule::ATTRIBUTE_ATTRIBUTE_COLON_PARAM_TERM (line 458)
    g.rule(n::ATTRIBUTE_LIST, &[n::ATTRIBUTE], None); // v2rule::ATTRIBUTE_LIST_ATTRIBUTE (line 473)
    g.rule(n::ATTRIBUTE_LIST, &[n::ATTRIBUTE_LIST, t::SEMICOLON, n::ATTRIBUTE], None); // v2rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE (line 478)
    g.rule(n::ATTRIBUTE_LIST, &[n::ATTRIBUTE_LIST, t::SEMICOLON], None); // v2rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON (line 483)
    g.rule(n::ATTRIBUTE_LIST, &[], None); // v2rule::ATTRIBUTE_LIST_EMPTY (line 487)
    g.rule(n::EXPR, &[n::EXPR, t::ARROW, t::LB, n::ATTRIBUTE_LIST, t::RB], None); // v2rule::EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB (line 491)
    g.rule(n::TEXT_EXPR, &[n::TEXT_EXPR, t::ARROW, t::LB, n::ATTRIBUTE_LIST, t::RB], None); // v2rule::TEXT_EXPR_TEXT_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB (line 499)
    g.rule(n::TEXT_EXPR, &[t::EXACT], Some(t::TERMLIST)); // v2rule::TEXT_EXPR_EXACT (line 511)
    g.rule(n::TEXT_EXPR, &[t::QUOTE, t::ATTRIBUTE, t::QUOTE], Some(t::TERMLIST)); // v2rule::TEXT_EXPR_QUOTE_ATTRIBUTE_QUOTE (line 532)
    g.rule(n::TEXT_EXPR, &[t::SQUOTE, t::ATTRIBUTE, t::SQUOTE], Some(t::TERMLIST)); // v2rule::TEXT_EXPR_SQUOTE_ATTRIBUTE_SQUOTE (line 543)
    g.rule(n::TEXT_EXPR, &[n::PARAM_TERM], Some(t::LOWEST)); // v2rule::TEXT_EXPR_PARAM_TERM (line 554)
    g.rule(n::TEXT_EXPR, &[n::AFFIX], Some(t::PREFIX)); // v2rule::TEXT_EXPR_AFFIX (line 562)
    g.rule(n::TEXT_EXPR, &[n::VERBATIM], Some(t::VERBATIM)); // v2rule::TEXT_EXPR_VERBATIM (line 566)
    g.rule(n::TERMLIST, &[n::PARAM_TERM, n::PARAM_TERM], Some(t::TERMLIST)); // v2rule::TERMLIST_PARAM_TERM_PARAM_TERM (line 570)
    g.rule(n::TERMLIST, &[n::TERMLIST, n::PARAM_TERM], Some(t::TERMLIST)); // v2rule::TERMLIST_TERMLIST_PARAM_TERM (line 576)
    g.rule(n::EXPR, &[t::MINUS, n::EXPR], None); // v2rule::EXPR_MINUS_EXPR (line 585)
    g.rule(n::TEXT_EXPR, &[t::MINUS, n::TEXT_EXPR], None); // v2rule::TEXT_EXPR_MINUS_TEXT_EXPR (line 589)
    g.rule(n::EXPR, &[t::TILDE, n::EXPR], None); // v2rule::EXPR_TILDE_EXPR (line 597)
    g.rule(n::TEXT_EXPR, &[t::TILDE, n::TEXT_EXPR], None); // v2rule::TEXT_EXPR_TILDE_TEXT_EXPR (line 605)
    g.rule(n::AFFIX, &[t::PREFIX], None); // v2rule::AFFIX_PREFIX (line 617)
    g.rule(n::AFFIX, &[t::SUFFIX], None); // v2rule::AFFIX_SUFFIX (line 621)
    g.rule(n::AFFIX, &[t::CONTAINS], None); // v2rule::AFFIX_CONTAINS (line 625)
    g.rule(n::VERBATIM, &[t::WILDCARD], None); // v2rule::VERBATIM_WILDCARD (line 633)
    g.rule(n::TEXT_EXPR, &[t::PERCENT, n::PARAM_TERM, t::PERCENT], Some(t::PREFIX)); // v2rule::TEXT_EXPR_PERCENT_PARAM_TERM_PERCENT (line 641)
    g.rule(n::TEXT_EXPR, &[t::PERCENT, t::PERCENT, n::PARAM_TERM, t::PERCENT, t::PERCENT], Some(t::PREFIX)); // v2rule::TEXT_EXPR_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT (line 645)
    g.rule(n::TEXT_EXPR, &[t::PERCENT, t::PERCENT, t::PERCENT, n::PARAM_TERM, t::PERCENT, t::PERCENT, t::PERCENT], Some(t::PREFIX)); // v2rule::TEXT_EXPR_PERCENT_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT_PERCENT (line 649)
    g.rule(n::MODIFIER, &[t::MODIFIER], None); // v2rule::MODIFIER_MODIFIER (line 657)
    g.rule(n::MODIFIERLIST, &[n::MODIFIER, t::OR, n::TERM], None); // v2rule::MODIFIERLIST_MODIFIER_OR_TERM (line 668)
    g.rule(n::MODIFIERLIST, &[n::MODIFIERLIST, t::OR, n::TERM], None); // v2rule::MODIFIERLIST_MODIFIERLIST_OR_TERM (line 696)
    g.rule(n::EXPR, &[t::ISMISSING, t::LP, n::MODIFIER, t::RP], None); // v2rule::EXPR_ISMISSING_LP_MODIFIER_RP (line 718)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, t::LB, n::TAG_LIST, t::RB], None); // v2rule::EXPR_MODIFIER_COLON_LB_TAG_LIST_RB (line 731)
    g.rule(n::TAG_LIST, &[n::PARAM_TERM_CASE], Some(t::TAGLIST)); // v2rule::TAG_LIST_PARAM_TERM_CASE (line 746)
    g.rule(n::TAG_LIST, &[n::AFFIX], Some(t::TAGLIST)); // v2rule::TAG_LIST_AFFIX (line 751)
    g.rule(n::TAG_LIST, &[n::VERBATIM], Some(t::TAGLIST)); // v2rule::TAG_LIST_VERBATIM (line 756)
    g.rule(n::TAG_LIST, &[n::TERMLIST], Some(t::TAGLIST)); // v2rule::TAG_LIST_TERMLIST (line 761)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::PARAM_TERM_CASE], Some(t::TAGLIST)); // v2rule::TAG_LIST_TAG_LIST_OR_PARAM_TERM_CASE (line 766)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::AFFIX], Some(t::TAGLIST)); // v2rule::TAG_LIST_TAG_LIST_OR_AFFIX (line 771)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::VERBATIM], Some(t::TAGLIST)); // v2rule::TAG_LIST_TAG_LIST_OR_VERBATIM (line 776)
    g.rule(n::TAG_LIST, &[n::TAG_LIST, t::OR, n::TERMLIST], Some(t::TAGLIST)); // v2rule::TAG_LIST_TAG_LIST_OR_TERMLIST (line 781)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::NUMERIC_RANGE], None); // v2rule::EXPR_MODIFIER_COLON_NUMERIC_RANGE (line 790)
    g.rule(n::NUMERIC_RANGE, &[t::LSQB, n::PARAM_NUM, n::PARAM_NUM, t::RSQB], Some(t::NUMBER)); // v2rule::NUMERIC_RANGE_LSQB_PARAM_NUM_PARAM_NUM_RSQB (line 801)
    g.rule(n::NUMERIC_RANGE, &[t::LSQB, n::EXCLUSIVE_PARAM_NUM, n::PARAM_NUM, t::RSQB], Some(t::NUMBER)); // v2rule::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_PARAM_NUM_RSQB (line 811)
    g.rule(n::NUMERIC_RANGE, &[t::LSQB, n::PARAM_NUM, n::EXCLUSIVE_PARAM_NUM, t::RSQB], Some(t::NUMBER)); // v2rule::NUMERIC_RANGE_LSQB_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB (line 821)
    g.rule(n::NUMERIC_RANGE, &[t::LSQB, n::EXCLUSIVE_PARAM_NUM, n::EXCLUSIVE_PARAM_NUM, t::RSQB], Some(t::NUMBER)); // v2rule::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB (line 831)
    g.rule(n::NUMERIC_RANGE, &[t::LSQB, n::PARAM_NUM, t::RSQB], Some(t::NUMBER)); // v2rule::NUMERIC_RANGE_LSQB_PARAM_NUM_RSQB (line 841)
    g.rule(n::EXPR, &[n::MODIFIER, t::NOT_EQUAL, n::PARAM_NUM], None); // v2rule::EXPR_MODIFIER_NOT_EQUAL_PARAM_NUM (line 845)
    g.rule(n::EXPR, &[n::MODIFIER, t::EQUALS, n::PARAM_NUM], None); // v2rule::EXPR_MODIFIER_EQUALS_PARAM_NUM (line 856)
    g.rule(n::EXPR, &[n::MODIFIER, t::GT, n::PARAM_NUM], None); // v2rule::EXPR_MODIFIER_GT_PARAM_NUM (line 866)
    g.rule(n::EXPR, &[n::MODIFIER, t::GE, n::PARAM_NUM], None); // v2rule::EXPR_MODIFIER_GE_PARAM_NUM (line 876)
    g.rule(n::EXPR, &[n::MODIFIER, t::LT, n::PARAM_NUM], None); // v2rule::EXPR_MODIFIER_LT_PARAM_NUM (line 886)
    g.rule(n::EXPR, &[n::MODIFIER, t::LE, n::PARAM_NUM], None); // v2rule::EXPR_MODIFIER_LE_PARAM_NUM (line 896)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::GEO_FILTER], None); // v2rule::EXPR_MODIFIER_COLON_GEO_FILTER (line 910)
    g.rule(n::GEO_FILTER, &[t::LSQB, n::PARAM_NUM, n::PARAM_NUM, n::PARAM_NUM, n::PARAM_TERM, t::RSQB], Some(t::NUMBER)); // v2rule::GEO_FILTER_LSQB_PARAM_NUM_PARAM_NUM_PARAM_NUM_PARAM_TERM_RSQB (line 922)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, n::GEOMETRY_QUERY], None); // v2rule::EXPR_MODIFIER_COLON_GEOMETRY_QUERY (line 937)
    g.rule(n::GEOMETRY_QUERY, &[t::LSQB, t::TERM, t::ATTRIBUTE, t::RSQB], None); // v2rule::GEOMETRY_QUERY_LSQB_TERM_ATTRIBUTE_RSQB (line 950)
    g.rule(n::QUERY, &[n::EXPR, t::ARROW, t::LSQB, n::VECTOR_QUERY, t::RSQB], None); // v2rule::QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB (line 974)
    g.rule(n::QUERY, &[n::TEXT_EXPR, t::ARROW, t::LSQB, n::VECTOR_QUERY, t::RSQB], None); // v2rule::QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB (line 983)
    g.rule(n::QUERY, &[n::STAR, t::ARROW, t::LSQB, n::VECTOR_QUERY, t::RSQB], None); // v2rule::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB (line 992)
    g.rule(n::VECTOR_QUERY, &[n::VECTOR_COMMAND, n::VECTOR_ATTRIBUTE_LIST, n::VECTOR_SCORE_FIELD], None); // v2rule::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST_VECTOR_SCORE_FIELD (line 1001)
    g.rule(n::VECTOR_QUERY, &[n::VECTOR_COMMAND, n::VECTOR_SCORE_FIELD], None); // v2rule::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_SCORE_FIELD (line 1014)
    g.rule(n::VECTOR_QUERY, &[n::VECTOR_COMMAND, n::VECTOR_ATTRIBUTE_LIST], None); // v2rule::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST (line 1026)
    g.rule(n::VECTOR_QUERY, &[n::VECTOR_COMMAND], None); // v2rule::VECTOR_QUERY_VECTOR_COMMAND (line 1032)
    g.rule_without_action(n::AS, &[t::AS_T], None); // v2rule::AS_AS_T (line 1036)
    g.rule(n::VECTOR_SCORE_FIELD, &[n::AS, n::PARAM_TERM_CASE], None); // v2rule::VECTOR_SCORE_FIELD_AS_PARAM_TERM_CASE (line 1038)
    g.rule(n::QUERY, &[n::EXPR, t::ARROW, t::LSQB, n::VECTOR_QUERY, t::RSQB, t::ARROW, t::LB, n::ATTRIBUTE_LIST, t::RB], None); // v2rule::QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB (line 1043)
    g.rule(n::QUERY, &[n::TEXT_EXPR, t::ARROW, t::LSQB, n::VECTOR_QUERY, t::RSQB, t::ARROW, t::LB, n::ATTRIBUTE_LIST, t::RB], None); // v2rule::QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB (line 1057)
    g.rule(n::QUERY, &[n::STAR, t::ARROW, t::LSQB, n::VECTOR_QUERY, t::RSQB, t::ARROW, t::LB, n::ATTRIBUTE_LIST, t::RB], None); // v2rule::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB (line 1071)
    g.rule(n::VECTOR_COMMAND, &[t::TERM, n::PARAM_SIZE, n::MODIFIER, t::ATTRIBUTE], None); // v2rule::VECTOR_COMMAND_TERM_PARAM_SIZE_MODIFIER_ATTRIBUTE (line 1086)
    g.rule(n::VECTOR_ATTRIBUTE, &[t::TERM, n::PARAM_TERM], None); // v2rule::VECTOR_ATTRIBUTE_TERM_PARAM_TERM (line 1101)
    g.rule(n::VECTOR_ATTRIBUTE_LIST, &[n::VECTOR_ATTRIBUTE_LIST, n::VECTOR_ATTRIBUTE], None); // v2rule::VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE (line 1113)
    g.rule(n::VECTOR_ATTRIBUTE_LIST, &[n::VECTOR_ATTRIBUTE], None); // v2rule::VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE (line 1120)
    g.rule(n::EXPR, &[n::MODIFIER, t::COLON, t::LSQB, n::VECTOR_RANGE_COMMAND, t::RSQB], None); // v2rule::EXPR_MODIFIER_COLON_LSQB_VECTOR_RANGE_COMMAND_RSQB (line 1128)
    g.rule(n::VECTOR_RANGE_COMMAND, &[t::TERM, n::PARAM_NUM, t::ATTRIBUTE], None); // v2rule::VECTOR_RANGE_COMMAND_TERM_PARAM_NUM_ATTRIBUTE (line 1139)
    g.rule(n::NUM, &[t::SIZE], None); // v2rule::NUM_SIZE (line 1153)
    g.rule(n::NUM, &[t::NUMBER], None); // v2rule::NUM_NUMBER (line 1157)
    g.rule(n::NUM, &[t::MINUS, n::NUM], None); // v2rule::NUM_MINUS_NUM (line 1161)
    g.rule(n::TERM, &[t::TERM], None); // v2rule::TERM_TERM (line 1166)
    g.rule(n::TERM, &[t::NUMBER], None); // v2rule::TERM_NUMBER (line 1171)
    g.rule(n::TERM, &[t::SIZE], None); // v2rule::TERM_SIZE (line 1176)
    g.rule(n::PARAM_TERM, &[n::TERM], None); // v2rule::PARAM_TERM_TERM (line 1185)
    g.rule(n::PARAM_TERM, &[t::ATTRIBUTE], None); // v2rule::PARAM_TERM_ATTRIBUTE (line 1189)
    g.rule(n::PARAM_TERM_CASE, &[n::TERM], None); // v2rule::PARAM_TERM_CASE_TERM (line 1194)
    g.rule(n::PARAM_TERM_CASE, &[t::ATTRIBUTE], None); // v2rule::PARAM_TERM_CASE_ATTRIBUTE (line 1199)
    g.rule(n::PARAM_SIZE, &[t::SIZE], None); // v2rule::PARAM_SIZE_SIZE (line 1204)
    g.rule(n::PARAM_SIZE, &[t::ATTRIBUTE], None); // v2rule::PARAM_SIZE_ATTRIBUTE (line 1209)
    g.rule(n::PARAM_NUM, &[t::ATTRIBUTE], None); // v2rule::PARAM_NUM_ATTRIBUTE (line 1214)
    g.rule(n::PARAM_NUM, &[t::MINUS, t::ATTRIBUTE], None); // v2rule::PARAM_NUM_MINUS_ATTRIBUTE (line 1219)
    g.rule(n::PARAM_NUM, &[n::NUM], None); // v2rule::PARAM_NUM_NUM (line 1225)
    g.rule(n::EXCLUSIVE_PARAM_NUM, &[t::LP, n::NUM], None); // v2rule::EXCLUSIVE_PARAM_NUM_LP_NUM (line 1230)
    g.rule(n::EXCLUSIVE_PARAM_NUM, &[t::LP, t::ATTRIBUTE], None); // v2rule::EXCLUSIVE_PARAM_NUM_LP_ATTRIBUTE (line 1235)
    g.rule(n::EXCLUSIVE_PARAM_NUM, &[t::LP, t::MINUS, t::ATTRIBUTE], None); // v2rule::EXCLUSIVE_PARAM_NUM_LP_MINUS_ATTRIBUTE (line 1240)
    g
}

/// [`v2`]'s tables, built once.
pub fn v2_tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| v2().build().expect("the v2 grammar builds"))
}
#[cfg(test)]
mod tests {
    //! Expectations generated once from lemon (`srcutil/lemon.c`, compiled with clang) in a
    //! scratch directory, not needed at run time:
    //!
    //! - `lemon -p -l -T srcutil/lempar.c parser.y` on each grammar gives `parser.out` with the
    //!   conflicts precedence resolved; its states were converted line for line into
    //!   [`Tables::report`]'s form (`V*_LEMON_STATES`), its rule list is `V*_LEMON_RULES`.
    //!   Lemon's `-s` statistics give the state and conflict counts; a copy of lemon.c counting
    //!   `resolve_conflict`'s precedence decisions and all LR(0) states gives the rest.
    //! - The traces come from a lemon build of each grammar with the action code replaced by
    //!   stubs (declarations and symbol order kept, the generated tables were checked to be
    //!   byte-identical to RediSearch's `parser.c`), compiled with `lempar.c` and `ParseTrace`, and
    //!   fed the token sequences below (values are 1-based token positions; end of input is
    //!   token 0). `r<n>` is a reduce by lemon rule n, `E<pos>` a `%syntax_error` on the token at
    //!   that position, `A` accept, `F` `%parse_failure`, `O` `%stack_overflow`.
    use super::*;
    use crate::search::lalr::{EOF, Handler, Parser};

    #[derive(Default)]
    struct Trace(Vec<String>);

    impl Handler<usize> for Trace {
        fn reduce(&mut self, rule: RuleId, _rhs: Vec<usize>) -> usize {
            self.0.push(format!("r{rule}"));
            0
        }
        fn syntax_error(&mut self, _token: Symbol, value: &usize) {
            self.0.push(format!("E{value}"));
        }
        fn stack_overflow(&mut self) {
            self.0.push("O".into());
        }
        fn parse_failure(&mut self) {
            self.0.push("F".into());
        }
        fn accept(&mut self, _value: usize) {
            self.0.push("A".into());
        }
    }

    /// Feeds token names (`NAME*N` repeats one) then end of input.
    fn run(t: &Tables, tokens: &str) -> String {
        let mut p = Parser::new(t);
        let mut h = Trace::default();
        let mut pos = 0;
        for word in tokens.split_whitespace() {
            let (name, n) = word.split_once('*').map_or((word, 1), |(a, b)| (a, b.parse().unwrap()));
            let tok = t.symbol(name).unwrap_or_else(|| panic!("no token {name}"));
            assert!((tok as usize) < t.n_terminals());
            for _ in 0..n {
                pos += 1;
                p.feed(tok, pos, &mut h);
            }
        }
        p.feed(EOF, pos + 1, &mut h);
        h.0.join(" ")
    }

    fn symbols_line(t: &Tables) -> String {
        (0..t.n_symbols() as Symbol).map(|s| t.symbol_name(s)).collect::<Vec<_>>().join(" ")
    }

    fn check_tables(t: &Tables, symbols: &str, rules: &str, states: &str) {
        assert!(t.errors().is_empty(), "{:?}", t.errors());
        assert_eq!(symbols_line(t), symbols);
        let report = t.rules_report();
        assert_eq!(report.lines().map(str::trim_start).collect::<Vec<_>>(), rules.lines().collect::<Vec<_>>());
        let ours = t.report();
        for (n, (a, b)) in ours.lines().zip(states.lines()).enumerate() {
            assert_eq!(a, b, "state {n}");
        }
        assert_eq!(ours.lines().count(), states.lines().count());
    }

    fn check_traces(t: &Tables, cases: &[(&str, &str)]) {
        for (tokens, want) in cases {
            assert_eq!(run(t, tokens), *want, "{tokens}");
        }
    }

    #[test]
    fn v1_matches_lemon() {
        let t = v1_tables();
        // lemon -s: 62 states, 0 conflicts; all states and precedence decisions from the
        // instrumented copy.
        assert_eq!((t.states().len(), t.n_states_total(), t.conflicts(), t.resolved_conflicts()), (62, 101, 0, 115));
        check_tables(t, V1_LEMON_SYMBOLS, V1_LEMON_RULES, V1_LEMON_STATES);
    }

    #[test]
    fn v1_rule_constants() {
        let t = v1_tables();
        let text = |id: RuleId| {
            let r = &t.rules()[id as usize];
            let rhs: String = r.rhs.iter().map(|&s| format!(" {}", t.symbol_name(s))).collect();
            format!("{} ::={rhs}.", t.symbol_name(r.lhs))
        };
        for (id, want) in [
            (v1rule::QUERY_EXPR, "query ::= expr."),
            (v1rule::QUERY_EMPTY, "query ::=."),
            (v1rule::QUERY_STAR, "query ::= STAR."),
            (v1rule::EXPR_EXPR_EXPR, "expr ::= expr expr."),
            (v1rule::EXPR_UNION, "expr ::= union."),
            (v1rule::UNION_EXPR_OR_EXPR, "union ::= expr OR expr."),
            (v1rule::UNION_UNION_OR_EXPR, "union ::= union OR expr."),
            (v1rule::EXPR_MODIFIER_COLON_EXPR, "expr ::= modifier COLON expr."),
            (v1rule::EXPR_MODIFIERLIST_COLON_EXPR, "expr ::= modifierlist COLON expr."),
            (v1rule::EXPR_LP_EXPR_RP, "expr ::= LP expr RP."),
            (v1rule::ATTRIBUTE_ATTRIBUTE_COLON_TERM, "attribute ::= ATTRIBUTE COLON term."),
            (v1rule::ATTRIBUTE_LIST_ATTRIBUTE, "attribute_list ::= attribute."),
            (v1rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE, "attribute_list ::= attribute_list SEMICOLON attribute."),
            (v1rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON, "attribute_list ::= attribute_list SEMICOLON."),
            (v1rule::ATTRIBUTE_LIST_EMPTY, "attribute_list ::=."),
            (v1rule::EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB, "expr ::= expr ARROW LB attribute_list RB."),
            (v1rule::EXPR_QUOTE_TERMLIST_QUOTE, "expr ::= QUOTE termlist QUOTE."),
            (v1rule::EXPR_QUOTE_TERM_QUOTE, "expr ::= QUOTE term QUOTE."),
            (v1rule::EXPR_TERM, "expr ::= term."),
            (v1rule::EXPR_AFFIX, "expr ::= affix."),
            (v1rule::EXPR_TERMLIST, "expr ::= termlist."),
            (v1rule::EXPR_STOPWORD, "expr ::= STOPWORD."),
            (v1rule::TERMLIST_TERM_TERM, "termlist ::= term term."),
            (v1rule::TERMLIST_TERMLIST_TERM, "termlist ::= termlist term."),
            (v1rule::TERMLIST_TERMLIST_STOPWORD, "termlist ::= termlist STOPWORD."),
            (v1rule::EXPR_MINUS_EXPR, "expr ::= MINUS expr."),
            (v1rule::EXPR_TILDE_EXPR, "expr ::= TILDE expr."),
            (v1rule::AFFIX_PREFIX, "affix ::= PREFIX."),
            (v1rule::AFFIX_SUFFIX, "affix ::= SUFFIX."),
            (v1rule::AFFIX_CONTAINS, "affix ::= CONTAINS."),
            (v1rule::EXPR_PERCENT_TERM_PERCENT, "expr ::= PERCENT term PERCENT."),
            (v1rule::EXPR_PERCENT_PERCENT_TERM_PERCENT_PERCENT, "expr ::= PERCENT PERCENT term PERCENT PERCENT."),
            (v1rule::EXPR_PERCENT_PERCENT_PERCENT_TERM_PERCENT_PERCENT_PERCENT, "expr ::= PERCENT PERCENT PERCENT term PERCENT PERCENT PERCENT."),
            (v1rule::EXPR_PERCENT_STOPWORD_PERCENT, "expr ::= PERCENT STOPWORD PERCENT."),
            (v1rule::EXPR_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT, "expr ::= PERCENT PERCENT STOPWORD PERCENT PERCENT."),
            (v1rule::EXPR_PERCENT_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT_PERCENT, "expr ::= PERCENT PERCENT PERCENT STOPWORD PERCENT PERCENT PERCENT."),
            (v1rule::MODIFIER_MODIFIER, "modifier ::= MODIFIER."),
            (v1rule::MODIFIERLIST_MODIFIER_OR_TERM, "modifierlist ::= modifier OR term."),
            (v1rule::MODIFIERLIST_MODIFIERLIST_OR_TERM, "modifierlist ::= modifierlist OR term."),
            (v1rule::EXPR_MODIFIER_COLON_TAG_LIST, "expr ::= modifier COLON tag_list."),
            (v1rule::TAG_LIST_LB_TERM, "tag_list ::= LB term."),
            (v1rule::TAG_LIST_LB_STOPWORD, "tag_list ::= LB STOPWORD."),
            (v1rule::TAG_LIST_LB_AFFIX, "tag_list ::= LB affix."),
            (v1rule::TAG_LIST_LB_TERMLIST, "tag_list ::= LB termlist."),
            (v1rule::TAG_LIST_TAG_LIST_OR_TERM, "tag_list ::= tag_list OR term."),
            (v1rule::TAG_LIST_TAG_LIST_OR_STOPWORD, "tag_list ::= tag_list OR STOPWORD."),
            (v1rule::TAG_LIST_TAG_LIST_OR_AFFIX, "tag_list ::= tag_list OR affix."),
            (v1rule::TAG_LIST_TAG_LIST_OR_TERMLIST, "tag_list ::= tag_list OR termlist."),
            (v1rule::TAG_LIST_TAG_LIST_RB, "tag_list ::= tag_list RB."),
            (v1rule::EXPR_MODIFIER_COLON_NUMERIC_RANGE, "expr ::= modifier COLON numeric_range."),
            (v1rule::NUMERIC_RANGE_LSQB_NUM_NUM_RSQB, "numeric_range ::= LSQB num num RSQB."),
            (v1rule::EXPR_MODIFIER_COLON_GEO_FILTER, "expr ::= modifier COLON geo_filter."),
            (v1rule::GEO_FILTER_LSQB_NUM_NUM_NUM_TERM_RSQB, "geo_filter ::= LSQB num num num TERM RSQB."),
            (v1rule::NUM_NUMBER, "num ::= NUMBER."),
            (v1rule::NUM_LP_NUM, "num ::= LP num."),
            (v1rule::NUM_MINUS_NUM, "num ::= MINUS num."),
            (v1rule::TERM_TERM, "term ::= TERM."),
            (v1rule::TERM_NUMBER, "term ::= NUMBER."),
        ] {
            assert_eq!(text(id), want);
        }
        assert_eq!(v1tok::LOWEST, t.symbol("LOWEST").unwrap());
    }

    #[test]
    fn v1_traces_match_lemon() {
        check_traces(
            v1_tables(),
            &[
                ("TERM", "r56 r18 r0 A"),
                ("TERM TERM", "r56 r56 r22 r20 r0 A"),
                ("TERM TERM TERM", "r56 r56 r22 r56 r23 r20 r0 A"),
                ("TERM OR TERM", "r56 r18 r56 r18 r5 r4 r0 A"),
                ("TERM OR TERM OR TERM", "r56 r18 r56 r18 r5 r4 r56 r18 r5 r4 r0 A"),
                ("TERM TERM OR TERM", "r56 r56 r22 r20 r56 r18 r5 r4 r0 A"),
                ("TERM OR TERM TERM", "r56 r18 r56 r56 r22 r20 r5 r4 r0 A"),
                ("NUMBER NUMBER", "r57 r57 r22 r20 r0 A"),
                ("STOPWORD TERM", "r21 r56 r18 r3 r0 A"),
                ("TERM STOPWORD TERM", "r56 r18 r21 r3 r56 r18 r3 r0 A"),
                ("MINUS TERM", "r56 r18 r25 r0 A"),
                ("MINUS TERM TERM", "r56 r56 r22 r20 r25 r0 A"),
                ("TILDE TERM TERM", "r56 r56 r22 r20 r26 r0 A"),
                ("MINUS LP TERM OR TERM RP", "r56 r18 r56 r18 r5 r4 r9 r25 r0 A"),
                ("LP TERM OR TERM RP TERM", "r56 r18 r56 r18 r5 r4 r9 r56 r18 r3 r0 A"),
                ("LP LP TERM RP RP", "r56 r18 r9 r9 r0 A"),
                ("MODIFIER COLON TERM", "r36 r56 r18 r7 r0 A"),
                ("MODIFIER COLON TERM TERM", "r36 r56 r56 r22 r20 r7 r0 A"),
                ("TERM MODIFIER COLON TERM", "r56 r18 r36 r56 r18 r7 r3 r0 A"),
                ("MODIFIER COLON LP TERM OR TERM RP", "r36 r56 r18 r56 r18 r5 r4 r9 r7 r0 A"),
                ("MODIFIER OR TERM COLON TERM", "r36 r56 r37 r56 r18 r8 r0 A"),
                ("MODIFIER OR TERM OR TERM COLON TERM TERM", "r36 r56 r37 r56 r38 r56 r56 r22 r20 r8 r0 A"),
                ("MODIFIER COLON LB TERM OR TERM RB", "r36 r56 r40 r56 r44 r48 r39 r0 A"),
                ("MODIFIER COLON LB TERM TERM OR STOPWORD OR PREFIX RB", "r36 r56 r56 r22 r43 r45 r27 r46 r48 r39 r0 A"),
                ("MODIFIER COLON LB PREFIX RB TERM", "r36 r27 r42 r48 r39 r56 r18 r3 r0 A"),
                ("MODIFIER COLON LSQB NUMBER NUMBER RSQB", "r36 r53 r53 r50 r49 r0 A"),
                ("MODIFIER COLON LSQB LP NUMBER MINUS NUMBER RSQB", "r36 r53 r54 r53 r55 r50 r49 r0 A"),
                ("MODIFIER COLON LSQB NUMBER NUMBER NUMBER TERM RSQB", "r36 r53 r53 r53 r52 r51 r0 A"),
                ("QUOTE TERM TERM QUOTE", "r56 r56 r22 r16 r0 A"),
                ("QUOTE TERM QUOTE", "r56 r17 r0 A"),
                ("QUOTE TERM STOPWORD TERM QUOTE", "r56 E3 r56 r22 r16 r0 A"),
                ("PREFIX", "r27 r19 r0 A"),
                ("PREFIX TERM SUFFIX CONTAINS", "r27 r19 r56 r18 r3 r28 r19 r3 r29 r19 r3 r0 A"),
                ("PERCENT TERM PERCENT", "r56 r30 r0 A"),
                ("PERCENT PERCENT TERM PERCENT PERCENT", "r56 r31 r0 A"),
                ("PERCENT PERCENT PERCENT STOPWORD PERCENT PERCENT PERCENT", "r35 r0 A"),
                ("TERM ARROW LB ATTRIBUTE COLON TERM SEMICOLON RB", "r56 r18 r56 r10 r11 r13 r15 r0 A"),
                ("TERM ARROW LB ATTRIBUTE COLON TERM SEMICOLON ATTRIBUTE COLON NUMBER RB TERM", "r56 r18 r56 r10 r11 r57 r10 r12 r15 r56 r18 r3 r0 A"),
                ("STAR", "r2 A"),
                ("", "r1 A"),
                ("TERM RP", "r56 r18 E2 r0 A"),
                ("LP TERM", "r56 r18 E3 F"),
                ("MODIFIER COLON", "r36 E3 F"),
                ("MODIFIER", "r36 E2 F"),
                ("OR TERM", "E1 r56 r18 r0 A"),
                ("TERM OR", "r56 r18 E3 F"),
                ("STAR TERM", "E2 r2 A"),
                ("MODIFIER COLON LSQB NUMBER RSQB", "r36 r53 E5 F"),
                ("QUOTE TERM", "r56 E3 F"),
                ("LB TERM", "E1 r56 r18 r0 A"),
                ("TERM COLON TERM", "r56 r18 E2 r56 r18 r3 r0 A"),
                ("TERM RP TERM TERM RP", "r56 r18 E2 r56 r56 r22 r20 r3 r0 A"),
                ("RP RP RP RP TERM", "E1 r56 r18 r0 A"),
                ("MODIFIER COLON LB TERM", "r36 r56 r40 r39 r0 A"),
                ("PERCENT TERM", "r56 E3 F"),
                ("TERM ARROW TERM", "r56 r18 E3 F"),
                ("LP*98 TERM RP*98", "r56 r18 O E101 r1 A"),
                ("LP*99 TERM RP*99", "O E101 r1 A"),
                ("LP*120 TERM RP*120", "O r56 r18 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 r9 E142 r0 A"),
                ("MINUS*110 TERM", "O r56 r18 r25 r25 r25 r25 r25 r25 r25 r25 r25 r25 r0 A"),
                ("TERM*150", "r56 r56 r22 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r56 r23 r20 r0 A"),
                ("TERM OR*50 TERM", "r56 r18 E3 r56 r18 r5 r4 r0 A"),
            ],
        );
    }

    #[test]
    fn v2_matches_lemon() {
        let t = v2_tables();
        // lemon -s: 136 states, 0 conflicts; all states and precedence decisions from the
        // instrumented copy.
        assert_eq!((t.states().len(), t.n_states_total(), t.conflicts(), t.resolved_conflicts()), (136, 215, 0, 552));
        check_tables(t, V2_LEMON_SYMBOLS, V2_LEMON_RULES, V2_LEMON_STATES);
    }

    #[test]
    fn v2_rule_constants() {
        let t = v2_tables();
        let text = |id: RuleId| {
            let r = &t.rules()[id as usize];
            let rhs: String = r.rhs.iter().map(|&s| format!(" {}", t.symbol_name(s))).collect();
            format!("{} ::={rhs}.", t.symbol_name(r.lhs))
        };
        for (id, want) in [
            (v2rule::QUERY_EXPR, "query ::= expr."),
            (v2rule::QUERY_EMPTY, "query ::=."),
            (v2rule::QUERY_STAR, "query ::= star."),
            (v2rule::EXPR_TEXT_EXPR, "expr ::= text_expr."),
            (v2rule::EXPR_EXPR_EXPR, "expr ::= expr expr."),
            (v2rule::EXPR_TEXT_EXPR_EXPR, "expr ::= text_expr expr."),
            (v2rule::EXPR_EXPR_TEXT_EXPR, "expr ::= expr text_expr."),
            (v2rule::TEXT_EXPR_TEXT_EXPR_TEXT_EXPR, "text_expr ::= text_expr text_expr."),
            (v2rule::EXPR_UNION, "expr ::= union."),
            (v2rule::UNION_EXPR_OR_EXPR, "union ::= expr OR expr."),
            (v2rule::UNION_UNION_OR_EXPR, "union ::= union OR expr."),
            (v2rule::UNION_TEXT_EXPR_OR_EXPR, "union ::= text_expr OR expr."),
            (v2rule::UNION_EXPR_OR_TEXT_EXPR, "union ::= expr OR text_expr."),
            (v2rule::TEXT_EXPR_TEXT_UNION, "text_expr ::= text_union."),
            (v2rule::TEXT_UNION_TEXT_EXPR_OR_TEXT_EXPR, "text_union ::= text_expr OR text_expr."),
            (v2rule::TEXT_UNION_TEXT_UNION_OR_TEXT_EXPR, "text_union ::= text_union OR text_expr."),
            (v2rule::EXPR_MODIFIER_COLON_TEXT_EXPR, "expr ::= modifier COLON text_expr."),
            (v2rule::EXPR_MODIFIERLIST_COLON_TEXT_EXPR, "expr ::= modifierlist COLON text_expr."),
            (v2rule::EXPR_LP_EXPR_RP, "expr ::= LP expr RP."),
            (v2rule::TEXT_EXPR_LP_TEXT_EXPR_RP, "text_expr ::= LP text_expr RP."),
            (v2rule::ATTRIBUTE_ATTRIBUTE_COLON_PARAM_TERM, "attribute ::= ATTRIBUTE COLON param_term."),
            (v2rule::ATTRIBUTE_LIST_ATTRIBUTE, "attribute_list ::= attribute."),
            (v2rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE, "attribute_list ::= attribute_list SEMICOLON attribute."),
            (v2rule::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON, "attribute_list ::= attribute_list SEMICOLON."),
            (v2rule::ATTRIBUTE_LIST_EMPTY, "attribute_list ::=."),
            (v2rule::EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB, "expr ::= expr ARROW LB attribute_list RB."),
            (v2rule::TEXT_EXPR_TEXT_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB, "text_expr ::= text_expr ARROW LB attribute_list RB."),
            (v2rule::TEXT_EXPR_EXACT, "text_expr ::= EXACT."),
            (v2rule::TEXT_EXPR_QUOTE_ATTRIBUTE_QUOTE, "text_expr ::= QUOTE ATTRIBUTE QUOTE."),
            (v2rule::TEXT_EXPR_SQUOTE_ATTRIBUTE_SQUOTE, "text_expr ::= SQUOTE ATTRIBUTE SQUOTE."),
            (v2rule::TEXT_EXPR_PARAM_TERM, "text_expr ::= param_term."),
            (v2rule::TEXT_EXPR_AFFIX, "text_expr ::= affix."),
            (v2rule::TEXT_EXPR_VERBATIM, "text_expr ::= verbatim."),
            (v2rule::TERMLIST_PARAM_TERM_PARAM_TERM, "termlist ::= param_term param_term."),
            (v2rule::TERMLIST_TERMLIST_PARAM_TERM, "termlist ::= termlist param_term."),
            (v2rule::EXPR_MINUS_EXPR, "expr ::= MINUS expr."),
            (v2rule::TEXT_EXPR_MINUS_TEXT_EXPR, "text_expr ::= MINUS text_expr."),
            (v2rule::EXPR_TILDE_EXPR, "expr ::= TILDE expr."),
            (v2rule::TEXT_EXPR_TILDE_TEXT_EXPR, "text_expr ::= TILDE text_expr."),
            (v2rule::AFFIX_PREFIX, "affix ::= PREFIX."),
            (v2rule::AFFIX_SUFFIX, "affix ::= SUFFIX."),
            (v2rule::AFFIX_CONTAINS, "affix ::= CONTAINS."),
            (v2rule::VERBATIM_WILDCARD, "verbatim ::= WILDCARD."),
            (v2rule::TEXT_EXPR_PERCENT_PARAM_TERM_PERCENT, "text_expr ::= PERCENT param_term PERCENT."),
            (v2rule::TEXT_EXPR_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT, "text_expr ::= PERCENT PERCENT param_term PERCENT PERCENT."),
            (v2rule::TEXT_EXPR_PERCENT_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT_PERCENT, "text_expr ::= PERCENT PERCENT PERCENT param_term PERCENT PERCENT PERCENT."),
            (v2rule::MODIFIER_MODIFIER, "modifier ::= MODIFIER."),
            (v2rule::MODIFIERLIST_MODIFIER_OR_TERM, "modifierlist ::= modifier OR term."),
            (v2rule::MODIFIERLIST_MODIFIERLIST_OR_TERM, "modifierlist ::= modifierlist OR term."),
            (v2rule::EXPR_ISMISSING_LP_MODIFIER_RP, "expr ::= ISMISSING LP modifier RP."),
            (v2rule::EXPR_MODIFIER_COLON_LB_TAG_LIST_RB, "expr ::= modifier COLON LB tag_list RB."),
            (v2rule::TAG_LIST_PARAM_TERM_CASE, "tag_list ::= param_term_case."),
            (v2rule::TAG_LIST_AFFIX, "tag_list ::= affix."),
            (v2rule::TAG_LIST_VERBATIM, "tag_list ::= verbatim."),
            (v2rule::TAG_LIST_TERMLIST, "tag_list ::= termlist."),
            (v2rule::TAG_LIST_TAG_LIST_OR_PARAM_TERM_CASE, "tag_list ::= tag_list OR param_term_case."),
            (v2rule::TAG_LIST_TAG_LIST_OR_AFFIX, "tag_list ::= tag_list OR affix."),
            (v2rule::TAG_LIST_TAG_LIST_OR_VERBATIM, "tag_list ::= tag_list OR verbatim."),
            (v2rule::TAG_LIST_TAG_LIST_OR_TERMLIST, "tag_list ::= tag_list OR termlist."),
            (v2rule::EXPR_MODIFIER_COLON_NUMERIC_RANGE, "expr ::= modifier COLON numeric_range."),
            (v2rule::NUMERIC_RANGE_LSQB_PARAM_NUM_PARAM_NUM_RSQB, "numeric_range ::= LSQB param_num param_num RSQB."),
            (v2rule::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_PARAM_NUM_RSQB, "numeric_range ::= LSQB exclusive_param_num param_num RSQB."),
            (v2rule::NUMERIC_RANGE_LSQB_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB, "numeric_range ::= LSQB param_num exclusive_param_num RSQB."),
            (v2rule::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB, "numeric_range ::= LSQB exclusive_param_num exclusive_param_num RSQB."),
            (v2rule::NUMERIC_RANGE_LSQB_PARAM_NUM_RSQB, "numeric_range ::= LSQB param_num RSQB."),
            (v2rule::EXPR_MODIFIER_NOT_EQUAL_PARAM_NUM, "expr ::= modifier NOT_EQUAL param_num."),
            (v2rule::EXPR_MODIFIER_EQUALS_PARAM_NUM, "expr ::= modifier EQUALS param_num."),
            (v2rule::EXPR_MODIFIER_GT_PARAM_NUM, "expr ::= modifier GT param_num."),
            (v2rule::EXPR_MODIFIER_GE_PARAM_NUM, "expr ::= modifier GE param_num."),
            (v2rule::EXPR_MODIFIER_LT_PARAM_NUM, "expr ::= modifier LT param_num."),
            (v2rule::EXPR_MODIFIER_LE_PARAM_NUM, "expr ::= modifier LE param_num."),
            (v2rule::EXPR_MODIFIER_COLON_GEO_FILTER, "expr ::= modifier COLON geo_filter."),
            (v2rule::GEO_FILTER_LSQB_PARAM_NUM_PARAM_NUM_PARAM_NUM_PARAM_TERM_RSQB, "geo_filter ::= LSQB param_num param_num param_num param_term RSQB."),
            (v2rule::EXPR_MODIFIER_COLON_GEOMETRY_QUERY, "expr ::= modifier COLON geometry_query."),
            (v2rule::GEOMETRY_QUERY_LSQB_TERM_ATTRIBUTE_RSQB, "geometry_query ::= LSQB TERM ATTRIBUTE RSQB."),
            (v2rule::QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB, "query ::= expr ARROW LSQB vector_query RSQB."),
            (v2rule::QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB, "query ::= text_expr ARROW LSQB vector_query RSQB."),
            (v2rule::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB, "query ::= star ARROW LSQB vector_query RSQB."),
            (v2rule::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST_VECTOR_SCORE_FIELD, "vector_query ::= vector_command vector_attribute_list vector_score_field."),
            (v2rule::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_SCORE_FIELD, "vector_query ::= vector_command vector_score_field."),
            (v2rule::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST, "vector_query ::= vector_command vector_attribute_list."),
            (v2rule::VECTOR_QUERY_VECTOR_COMMAND, "vector_query ::= vector_command."),
            (v2rule::VECTOR_SCORE_FIELD_AS_PARAM_TERM_CASE, "vector_score_field ::= as param_term_case."),
            (v2rule::QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB, "query ::= expr ARROW LSQB vector_query RSQB ARROW LB attribute_list RB."),
            (v2rule::QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB, "query ::= text_expr ARROW LSQB vector_query RSQB ARROW LB attribute_list RB."),
            (v2rule::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB, "query ::= star ARROW LSQB vector_query RSQB ARROW LB attribute_list RB."),
            (v2rule::VECTOR_COMMAND_TERM_PARAM_SIZE_MODIFIER_ATTRIBUTE, "vector_command ::= TERM param_size modifier ATTRIBUTE."),
            (v2rule::VECTOR_ATTRIBUTE_TERM_PARAM_TERM, "vector_attribute ::= TERM param_term."),
            (v2rule::VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE, "vector_attribute_list ::= vector_attribute_list vector_attribute."),
            (v2rule::VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE, "vector_attribute_list ::= vector_attribute."),
            (v2rule::EXPR_MODIFIER_COLON_LSQB_VECTOR_RANGE_COMMAND_RSQB, "expr ::= modifier COLON LSQB vector_range_command RSQB."),
            (v2rule::VECTOR_RANGE_COMMAND_TERM_PARAM_NUM_ATTRIBUTE, "vector_range_command ::= TERM param_num ATTRIBUTE."),
            (v2rule::NUM_SIZE, "num ::= SIZE."),
            (v2rule::NUM_NUMBER, "num ::= NUMBER."),
            (v2rule::NUM_MINUS_NUM, "num ::= MINUS num."),
            (v2rule::TERM_TERM, "term ::= TERM."),
            (v2rule::TERM_NUMBER, "term ::= NUMBER."),
            (v2rule::TERM_SIZE, "term ::= SIZE."),
            (v2rule::PARAM_TERM_TERM, "param_term ::= term."),
            (v2rule::PARAM_TERM_ATTRIBUTE, "param_term ::= ATTRIBUTE."),
            (v2rule::PARAM_TERM_CASE_TERM, "param_term_case ::= term."),
            (v2rule::PARAM_TERM_CASE_ATTRIBUTE, "param_term_case ::= ATTRIBUTE."),
            (v2rule::PARAM_SIZE_SIZE, "param_size ::= SIZE."),
            (v2rule::PARAM_SIZE_ATTRIBUTE, "param_size ::= ATTRIBUTE."),
            (v2rule::PARAM_NUM_ATTRIBUTE, "param_num ::= ATTRIBUTE."),
            (v2rule::PARAM_NUM_MINUS_ATTRIBUTE, "param_num ::= MINUS ATTRIBUTE."),
            (v2rule::PARAM_NUM_NUM, "param_num ::= num."),
            (v2rule::EXCLUSIVE_PARAM_NUM_LP_NUM, "exclusive_param_num ::= LP num."),
            (v2rule::EXCLUSIVE_PARAM_NUM_LP_ATTRIBUTE, "exclusive_param_num ::= LP ATTRIBUTE."),
            (v2rule::EXCLUSIVE_PARAM_NUM_LP_MINUS_ATTRIBUTE, "exclusive_param_num ::= LP MINUS ATTRIBUTE."),
            (v2rule::STAR_STAR, "star ::= STAR."),
            (v2rule::STAR_LP_STAR_RP, "star ::= LP star RP."),
            (v2rule::AS_AS_T, "as ::= AS_T."),
        ] {
            assert_eq!(text(id), want);
        }
        assert_eq!(v2tok::LOWEST, t.symbol("LOWEST").unwrap());
    }

    #[test]
    fn v2_traces_match_lemon() {
        check_traces(
            v2_tables(),
            &[
                ("TERM", "r95 r98 r30 r3 r0 A"),
                ("TERM TERM", "r95 r98 r30 r95 r98 r30 r7 r3 r0 A"),
                ("TERM TERM TERM", "r95 r98 r30 r95 r98 r30 r7 r95 r98 r30 r7 r3 r0 A"),
                ("TERM OR TERM", "r95 r98 r30 r95 r98 r30 r14 r13 r3 r0 A"),
                ("TERM TERM OR TERM TERM", "r95 r98 r30 r95 r98 r30 r7 r95 r98 r30 r95 r98 r30 r7 r14 r13 r3 r0 A"),
                ("TERM OR TERM OR TERM", "r95 r98 r30 r95 r98 r30 r14 r95 r98 r30 r15 r13 r3 r0 A"),
                ("NUMBER SIZE ATTRIBUTE", "r96 r98 r30 r97 r98 r30 r99 r30 r7 r7 r3 r0 A"),
                ("MINUS TERM", "r95 r98 r30 r36 r3 r0 A"),
                ("TILDE TERM TERM", "r95 r98 r30 r38 r95 r98 r30 r7 r3 r0 A"),
                ("MINUS MODIFIER COLON TERM", "r46 r95 r98 r30 r16 r35 r0 A"),
                ("LP TERM OR TERM RP TERM", "r95 r98 r30 r95 r98 r30 r14 r13 r19 r95 r98 r30 r7 r3 r0 A"),
                ("LP LP TERM RP RP", "r95 r98 r30 r19 r19 r3 r0 A"),
                ("MODIFIER COLON TERM", "r46 r95 r98 r30 r16 r0 A"),
                ("MODIFIER COLON TERM TERM", "r46 r95 r98 r30 r16 r95 r98 r30 r6 r0 A"),
                ("TERM MODIFIER COLON TERM", "r95 r98 r30 r46 r95 r98 r30 r16 r5 r0 A"),
                ("MODIFIER COLON LP TERM OR TERM RP", "r46 r95 r98 r30 r95 r98 r30 r14 r13 r19 r16 r0 A"),
                ("MODIFIER OR TERM COLON TERM", "r46 r95 r47 r95 r98 r30 r17 r0 A"),
                ("MODIFIER COLON LB TERM OR TERM RB", "r46 r95 r100 r51 r95 r100 r55 r50 r0 A"),
                ("MODIFIER COLON LB TERM TERM RB", "r46 r95 r98 r95 r98 r33 r54 r50 r0 A"),
                ("MODIFIER COLON LB PREFIX OR WILDCARD OR ATTRIBUTE RB", "r46 r39 r52 r42 r57 r101 r55 r50 r0 A"),
                ("MODIFIER COLON LSQB NUMBER NUMBER RSQB", "r46 r93 r106 r93 r106 r60 r59 r0 A"),
                ("MODIFIER COLON LSQB LP NUMBER LP SIZE RSQB", "r46 r93 r107 r92 r107 r63 r59 r0 A"),
                ("MODIFIER COLON LSQB NUMBER RSQB", "r46 r93 r106 r64 r59 r0 A"),
                ("MODIFIER COLON LSQB MINUS ATTRIBUTE ATTRIBUTE RSQB", "r46 r105 r104 r60 r59 r0 A"),
                ("MODIFIER COLON LSQB LP MINUS ATTRIBUTE MINUS NUMBER RSQB", "r46 r109 r93 r94 r106 r61 r59 r0 A"),
                ("MODIFIER GE NUMBER", "r46 r93 r106 r68 r0 A"),
                ("MODIFIER EQUALS ATTRIBUTE", "r46 r104 r66 r0 A"),
                ("MODIFIER NOT_EQUAL MINUS NUMBER", "r46 r93 r94 r106 r65 r0 A"),
                ("MODIFIER LT SIZE TERM", "r46 r92 r106 r69 r95 r98 r30 r6 r0 A"),
                ("MODIFIER COLON LSQB NUMBER NUMBER NUMBER TERM RSQB", "r46 r93 r106 r93 r106 r93 r106 r95 r98 r72 r71 r0 A"),
                ("MODIFIER COLON LSQB TERM ATTRIBUTE RSQB", "r46 r74 r73 r0 A"),
                ("STAR ARROW LSQB TERM SIZE MODIFIER ATTRIBUTE RSQB", "r110 r102 r46 r86 r81 r77 A"),
                ("STAR ARROW LSQB TERM ATTRIBUTE MODIFIER ATTRIBUTE TERM SIZE AS_T TERM RSQB", "r110 r103 r46 r86 r97 r98 r87 r89 r112 r95 r100 r82 r78 r77 A"),
                ("TERM ARROW LSQB TERM SIZE MODIFIER ATTRIBUTE AS_T TERM RSQB", "r95 r98 r30 r102 r46 r86 r112 r95 r100 r82 r79 r76 A"),
                ("TERM TERM ARROW LSQB TERM SIZE MODIFIER ATTRIBUTE TERM NUMBER TERM ATTRIBUTE RSQB", "r95 r98 r30 r95 r98 r30 E4 F"),
                ("MODIFIER COLON TERM ARROW LSQB TERM SIZE MODIFIER ATTRIBUTE RSQB ARROW LB ATTRIBUTE COLON TERM RB", "r46 r95 r98 r30 r16 r102 r46 r86 r81 r95 r98 r20 r21 r83 A"),
                ("LP STAR RP ARROW LSQB TERM SIZE MODIFIER ATTRIBUTE RSQB", "r110 r111 r102 r46 r86 r81 r77 A"),
                ("MODIFIER COLON LSQB TERM NUMBER ATTRIBUTE RSQB", "r46 r93 r106 r91 r90 r0 A"),
                ("MODIFIER COLON LSQB TERM ATTRIBUTE ATTRIBUTE RSQB TERM", "r46 r104 r91 r90 r95 r98 r30 r6 r0 A"),
                ("ISMISSING LP MODIFIER RP", "r46 r49 r0 A"),
                ("ISMISSING", "E2 F"),
                ("TERM ISMISSING TERM", "r95 r98 r30 E3 F"),
                ("EXACT", "r27 r3 r0 A"),
                ("EXACT TERM", "r27 r95 r98 r30 r7 r3 r0 A"),
                ("TERM AS_T TERM", "r95 r98 r30 r95 r98 r30 r7 r95 r98 r30 r7 r3 r0 A"),
                ("AS_T", "r95 r98 r30 r3 r0 A"),
                ("QUOTE ATTRIBUTE QUOTE", "r28 r3 r0 A"),
                ("SQUOTE ATTRIBUTE SQUOTE", "r29 r3 r0 A"),
                ("PREFIX SUFFIX CONTAINS WILDCARD", "r39 r31 r40 r31 r41 r31 r42 r32 r7 r7 r7 r3 r0 A"),
                ("PERCENT TERM PERCENT", "r95 r98 r43 r3 r0 A"),
                ("PERCENT PERCENT ATTRIBUTE PERCENT PERCENT", "r99 r44 r3 r0 A"),
                ("TERM ARROW LB ATTRIBUTE COLON TERM SEMICOLON RB", "r95 r98 r30 r95 r98 r20 r21 r23 r26 r3 r0 A"),
                ("STAR", "r110 r2 A"),
                ("LP STAR RP", "r110 r111 r2 A"),
                ("LP LP STAR RP RP", "r110 r111 r111 r2 A"),
                ("", "r1 A"),
                ("TERM RP", "r95 r98 r30 r3 E2 r0 A"),
                ("LP TERM", "r95 r98 r30 E3 F"),
                ("MODIFIER COLON", "r46 E3 F"),
                ("MODIFIER", "r46 E2 F"),
                ("OR", "E1 r1 A"),
                ("TERM OR", "r95 r98 r30 E3 F"),
                ("STAR TERM", "r110 E2 r2 A"),
                ("MODIFIER COLON LSQB NUMBER NUMBER", "r46 r93 r106 r93 r106 E6 F"),
                ("MODIFIER COLON LB RB", "r46 E4 F"),
                ("STAR ARROW LSQB TERM SIZE MODIFIER ATTRIBUTE RSQB TERM", "r110 r102 r46 r86 r81 E9 r77 A"),
                ("TERM ARROW LSQB TERM RSQB", "r95 r98 r30 E5 F"),
                ("ISMISSING LP TERM RP", "E3 F"),
                ("MODIFIER GE TERM", "r46 E3 F"),
                ("MODIFIER COLON LB AS_T RB", "r46 r95 r100 r51 r50 r0 A"),
                ("RP TERM RP TERM TERM TERM RP", "E1 r95 r98 r30 r3 r95 r98 r30 r6 r95 r98 r30 r6 r95 r98 r30 r6 E7 r0 A"),
                ("LP*250 TERM RP*250", "r95 r98 r30 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r3 r0 A"),
                ("LP*254 TERM RP*254", "r95 r98 r30 O E257 r1 A"),
                ("LP*300 TERM RP*300", "O r95 r98 r30 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r19 r3 E346 r0 A"),
                ("TERM*300", "r95 r98 r30 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r95 r98 r30 r7 r3 r0 A"),
                ("MINUS*260 TERM", "O r95 r98 r30 r36 r36 r36 r36 r3 r0 A"),
            ],
        );
    }

    /// `parser.out`'s symbols, by number.
    const V1_LEMON_SYMBOLS: &str = "$ LOWEST TILDE TAGLIST QUOTE COLON MINUS NUMBER STOPWORD TERMLIST TERM PREFIX SUFFIX CONTAINS PERCENT ATTRIBUTE LP RP MODIFIER AND OR ORX ARROW STAR SEMICOLON LB RB LSQB RSQB expr attribute attribute_list affix termlist union fuzzy tag_list geo_filter modifierlist num numeric_range query modifier term";

    /// `parser.out`'s rules.
    const V1_LEMON_RULES: &str = "\
    0: query ::= expr.\n\
    1: query ::=.\n\
    2: query ::= STAR.\n\
    3: expr ::= expr expr. [AND precedence=17]\n\
    4: expr ::= union. [ORX precedence=19]\n\
    5: union ::= expr OR expr. [OR precedence=18]\n\
    6: union ::= union OR expr. [ORX precedence=19]\n\
    7: expr ::= modifier COLON expr. [MODIFIER precedence=16]\n\
    8: expr ::= modifierlist COLON expr. [MODIFIER precedence=16]\n\
    9: expr ::= LP expr RP. [LP precedence=14]\n\
    10: attribute ::= ATTRIBUTE COLON term. [ATTRIBUTE precedence=13]\n\
    11: attribute_list ::= attribute.\n\
    12: attribute_list ::= attribute_list SEMICOLON attribute.\n\
    13: attribute_list ::= attribute_list SEMICOLON.\n\
    14: attribute_list ::=.\n\
    15: expr ::= expr ARROW LB attribute_list RB. [ARROW precedence=20]\n\
    16: expr ::= QUOTE termlist QUOTE. [TERMLIST precedence=9]\n\
    17: expr ::= QUOTE term QUOTE. [TERMLIST precedence=9]\n\
    18: expr ::= term. [LOWEST precedence=1]\n\
    19: expr ::= affix. [PREFIX precedence=11]\n\
    20: expr ::= termlist. [TERMLIST precedence=9]\n\
    21: expr ::= STOPWORD. [STOPWORD precedence=8]\n\
    22: termlist ::= term term. [TERMLIST precedence=9]\n\
    23: termlist ::= termlist term. [TERMLIST precedence=9]\n\
    24: termlist ::= termlist STOPWORD. [TERMLIST precedence=9]\n\
    25: expr ::= MINUS expr. [MINUS precedence=6]\n\
    26: expr ::= TILDE expr. [TILDE precedence=2]\n\
    27: affix ::= PREFIX. [PREFIX precedence=11]\n\
    28: affix ::= SUFFIX. [PREFIX precedence=11]\n\
    29: affix ::= CONTAINS. [PREFIX precedence=11]\n\
    30: expr ::= PERCENT term PERCENT. [PREFIX precedence=11]\n\
    31: expr ::= PERCENT PERCENT term PERCENT PERCENT. [PREFIX precedence=11]\n\
    32: expr ::= PERCENT PERCENT PERCENT term PERCENT PERCENT PERCENT. [PREFIX precedence=11]\n\
    33: expr ::= PERCENT STOPWORD PERCENT. [PREFIX precedence=11]\n\
    34: expr ::= PERCENT PERCENT STOPWORD PERCENT PERCENT. [PREFIX precedence=11]\n\
    35: expr ::= PERCENT PERCENT PERCENT STOPWORD PERCENT PERCENT PERCENT. [PREFIX precedence=11]\n\
    36: modifier ::= MODIFIER. [MODIFIER precedence=16]\n\
    37: modifierlist ::= modifier OR term. [OR precedence=18]\n\
    38: modifierlist ::= modifierlist OR term. [OR precedence=18]\n\
    39: expr ::= modifier COLON tag_list. [COLON precedence=5]\n\
    40: tag_list ::= LB term. [TAGLIST precedence=3]\n\
    41: tag_list ::= LB STOPWORD. [TAGLIST precedence=3]\n\
    42: tag_list ::= LB affix. [TAGLIST precedence=3]\n\
    43: tag_list ::= LB termlist. [TAGLIST precedence=3]\n\
    44: tag_list ::= tag_list OR term. [TAGLIST precedence=3]\n\
    45: tag_list ::= tag_list OR STOPWORD. [TAGLIST precedence=3]\n\
    46: tag_list ::= tag_list OR affix. [TAGLIST precedence=3]\n\
    47: tag_list ::= tag_list OR termlist. [TAGLIST precedence=3]\n\
    48: tag_list ::= tag_list RB. [TAGLIST precedence=3]\n\
    49: expr ::= modifier COLON numeric_range. [COLON precedence=5]\n\
    50: numeric_range ::= LSQB num num RSQB. [NUMBER precedence=7]\n\
    51: expr ::= modifier COLON geo_filter. [COLON precedence=5]\n\
    52: geo_filter ::= LSQB num num num TERM RSQB. [NUMBER precedence=7]\n\
    53: num ::= NUMBER. [NUMBER precedence=7]\n\
    54: num ::= LP num. [LP precedence=14]\n\
    55: num ::= MINUS num. [MINUS precedence=6]\n\
    56: term ::= TERM. [TERM precedence=10]\n\
    57: term ::= NUMBER. [NUMBER precedence=7]\n\
    ";

    /// `parser.out`'s states (`lemon -p`), in [`Tables::report`]'s form.
    const V1_LEMON_STATES: &str = "\
    0:0r1 2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 23s45 29s3 32t19 33s39 34t4 38s43 41a 42s44 43s37\n\
    1:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 25s17 27s28 29s13 32t19 33s39 34t4 36s41 37t51 38s43 40t49 42s44 43s37\n\
    2:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 17t9 18t36 20s9 22s61 29s14 32t19 33s39 34t4 38s43 42s44 43s37\n\
    3:0r0 2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 20s9 22s61 29s14 32t19 33s39 34t4 38s43 42s44 43s37\n\
    4:2x5 4s19 4y26 6s6 6y26 7t57 7y26 8t21 8y26 10t56 10y26 11t27 11y26 12t28 12y26 13t29 13y26 14s23 14y26 16s7 16y26 18t36 18y26 20s9 20y26 22s61 22y26 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r26\n\
    5:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 29s4 32t19 33s39 34t4 38s43 42s44 43s37\n\
    6:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 29s11 32t19 33s39 34t4 38s43 42s44 43s37\n\
    7:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 29s2 32t19 33s39 34t4 38s43 42s44 43s37\n\
    8:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 29s12 32t19 33s39 34t4 38s43 42s44 43s37\n\
    9:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 29s15 32t19 33s39 34t4 38s43 42s44 43s37\n\
    10:2s5 4s19 6s6 7t57 8t21 10t56 11t27 12t28 13t29 14s23 16s7 18t36 29s16 32t19 33s39 34t4 38s43 42s44 43s37\n\
    11:2x5 4x19 6x6 7t57 7y25 8t21 8y25 10t56 10y25 11t27 11y25 12t28 12y25 13t29 13y25 14s23 14y25 16s7 16y25 18t36 18y25 20s9 20y25 22s61 22y25 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r25\n\
    12:2x5 4x19 6x6 7x96 8x86 10x97 11x85 12x84 13x83 14x23 16x7 18x76 20s9 20y8 22s61 22y8 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r8\n\
    13:2x5 4x19 6x6 7x96 8x86 10x97 11x85 12x84 13x83 14x23 16x7 18x76 20s9 20y7 22s61 22y7 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r7\n\
    14:2x5 4x19 6x6 7x96 8x86 10x97 11x85 12x84 13x83 14x23 16x7 18x76 20s9 20y3 22s61 22y3 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r3\n\
    15:2x5 4x19 6x6 7x96 8x86 10x97 11x85 12x84 13x83 14x23 16x7 18x76 20x9 22s61 22y5 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r5\n\
    16:2x5 4x19 6x6 7x96 8x86 10x97 11x85 12x84 13x83 14x23 16x7 18x76 20x9 22s61 22y6 29s14 32t19 33s39 34t4 38s43 42s44 43s37 *r6\n\
    17:7t57 8t41 10t56 11t27 12t28 13t29 32t42 33s29 43s34\n\
    18:7t57 8t45 10t56 11t27 12t28 13t29 32t46 33s30 43s35\n\
    19:7t57 10t56 33s24 43s32\n\
    20:15s60 30t11 31s42 *r14\n\
    21:6s25 7t53 16s26 28t50 39s47\n\
    22:7t57 8s50 10t56 14s31 43s58\n\
    23:7t57 8s48 10t56 14s22 43s59\n\
    24:4t16 7t57 8t24 10t56 43t23\n\
    25:6s25 7t53 16s26 39t55\n\
    26:6s25 7t53 16s26 39t54\n\
    27:6s25 7t53 16s26 39s21\n\
    28:6s25 7t53 16s26 39s27\n\
    29:7t57 7y43 8t24 8y43 10t56 10y43 43t23 *r43\n\
    30:7t57 7y47 8t24 8y47 10t56 10y47 43t23 *r47\n\
    31:7t57 8s53 10t56 43s56\n\
    32:4t17 7t57 10t56 43t22\n\
    33:7t57 10t56 43t37\n\
    34:7t57 7y40 10t56 10y40 43t22 *r40\n\
    35:7t57 7y44 10t56 10y44 43t22 *r44\n\
    36:7t57 10t56 43t38\n\
    37:7t57 7y18 10t56 10y18 43t22 *r18\n\
    38:7t57 10t56 43t10\n\
    39:7x96 8x90 10t56 10y20 43t23 *r20\n\
    40:15s60 30t12 *r13\n\
    41:20s18 20y39 26t48 *r39\n\
    42:24s40 26t15\n\
    43:5s8 20s36\n\
    44:5s1 20s33\n\
    45:0r2\n\
    46:28t52\n\
    47:10s46\n\
    48:14t33\n\
    49:14t34\n\
    50:14s49\n\
    51:14t35\n\
    52:14s51\n\
    53:14s52\n\
    54:14t32\n\
    55:14s54\n\
    56:14s55\n\
    57:14t31\n\
    58:14s57\n\
    59:14t30\n\
    60:5s38\n\
    61:25s20\n\
    ";

    /// `parser.out`'s symbols, by number.
    const V2_LEMON_SYMBOLS: &str = "$ LOWEST TEXTEXPR ORX OR ISMISSING MODIFIER RP RB RSQB EXACT TERM QUOTE SQUOTE LP LB LSQB TILDE MINUS AND ARROW COLON NOT_EQUAL EQUALS GE GT LE LT NUMBER SIZE STAR TAGLIST TERMLIST PREFIX SUFFIX CONTAINS PERCENT ATTRIBUTE VERBATIM WILDCARD AS_T SEMICOLON expr attribute attribute_list affix suffix contains verbatim termlist union text_union text_expr fuzzy tag_list geo_filter geometry_query vector_query vector_command vector_range_command vector_attribute vector_attribute_list modifier modifierlist num numeric_range query star param_term term param_term_case param_num exclusive_param_num vector_score_field as param_size";

    /// `parser.out`'s rules.
    const V2_LEMON_RULES: &str = "\
    0: query ::= expr.\n\
    1: query ::=.\n\
    2: query ::= star.\n\
    3: expr ::= text_expr. [TEXTEXPR precedence=2]\n\
    4: expr ::= expr expr. [AND precedence=13]\n\
    5: expr ::= text_expr expr. [AND precedence=13]\n\
    6: expr ::= expr text_expr. [AND precedence=13]\n\
    7: text_expr ::= text_expr text_expr. [AND precedence=13]\n\
    8: expr ::= union. [ORX precedence=3]\n\
    9: union ::= expr OR expr. [OR precedence=4]\n\
    10: union ::= union OR expr. [OR precedence=4]\n\
    11: union ::= text_expr OR expr. [OR precedence=4]\n\
    12: union ::= expr OR text_expr. [OR precedence=4]\n\
    13: text_expr ::= text_union. [ORX precedence=3]\n\
    14: text_union ::= text_expr OR text_expr. [OR precedence=4]\n\
    15: text_union ::= text_union OR text_expr. [OR precedence=4]\n\
    16: expr ::= modifier COLON text_expr. [COLON precedence=15]\n\
    17: expr ::= modifierlist COLON text_expr. [COLON precedence=15]\n\
    18: expr ::= LP expr RP. [LP precedence=11]\n\
    19: text_expr ::= LP text_expr RP. [LP precedence=11]\n\
    20: attribute ::= ATTRIBUTE COLON param_term. [ATTRIBUTE precedence=25]\n\
    21: attribute_list ::= attribute.\n\
    22: attribute_list ::= attribute_list SEMICOLON attribute.\n\
    23: attribute_list ::= attribute_list SEMICOLON.\n\
    24: attribute_list ::=.\n\
    25: expr ::= expr ARROW LB attribute_list RB. [ARROW precedence=14]\n\
    26: text_expr ::= text_expr ARROW LB attribute_list RB. [ARROW precedence=14]\n\
    27: text_expr ::= EXACT. [TERMLIST precedence=22]\n\
    28: text_expr ::= QUOTE ATTRIBUTE QUOTE. [TERMLIST precedence=22]\n\
    29: text_expr ::= SQUOTE ATTRIBUTE SQUOTE. [TERMLIST precedence=22]\n\
    30: text_expr ::= param_term. [LOWEST precedence=1]\n\
    31: text_expr ::= affix. [PREFIX precedence=23]\n\
    32: text_expr ::= verbatim. [VERBATIM precedence=26]\n\
    33: termlist ::= param_term param_term. [TERMLIST precedence=22]\n\
    34: termlist ::= termlist param_term. [TERMLIST precedence=22]\n\
    35: expr ::= MINUS expr. [MINUS precedence=12]\n\
    36: text_expr ::= MINUS text_expr. [MINUS precedence=12]\n\
    37: expr ::= TILDE expr. [TILDE precedence=12]\n\
    38: text_expr ::= TILDE text_expr. [TILDE precedence=12]\n\
    39: affix ::= PREFIX. [PREFIX precedence=23]\n\
    40: affix ::= SUFFIX. [SUFFIX precedence=23]\n\
    41: affix ::= CONTAINS. [CONTAINS precedence=23]\n\
    42: verbatim ::= WILDCARD. [WILDCARD precedence=26]\n\
    43: text_expr ::= PERCENT param_term PERCENT. [PREFIX precedence=23]\n\
    44: text_expr ::= PERCENT PERCENT param_term PERCENT PERCENT. [PREFIX precedence=23]\n\
    45: text_expr ::= PERCENT PERCENT PERCENT param_term PERCENT PERCENT PERCENT. [PREFIX precedence=23]\n\
    46: modifier ::= MODIFIER. [MODIFIER precedence=6]\n\
    47: modifierlist ::= modifier OR term. [OR precedence=4]\n\
    48: modifierlist ::= modifierlist OR term. [OR precedence=4]\n\
    49: expr ::= ISMISSING LP modifier RP. [ISMISSING precedence=5]\n\
    50: expr ::= modifier COLON LB tag_list RB. [COLON precedence=15]\n\
    51: tag_list ::= param_term_case. [TAGLIST precedence=21]\n\
    52: tag_list ::= affix. [TAGLIST precedence=21]\n\
    53: tag_list ::= verbatim. [TAGLIST precedence=21]\n\
    54: tag_list ::= termlist. [TAGLIST precedence=21]\n\
    55: tag_list ::= tag_list OR param_term_case. [TAGLIST precedence=21]\n\
    56: tag_list ::= tag_list OR affix. [TAGLIST precedence=21]\n\
    57: tag_list ::= tag_list OR verbatim. [TAGLIST precedence=21]\n\
    58: tag_list ::= tag_list OR termlist. [TAGLIST precedence=21]\n\
    59: expr ::= modifier COLON numeric_range. [COLON precedence=15]\n\
    60: numeric_range ::= LSQB param_num param_num RSQB. [NUMBER precedence=18]\n\
    61: numeric_range ::= LSQB exclusive_param_num param_num RSQB. [NUMBER precedence=18]\n\
    62: numeric_range ::= LSQB param_num exclusive_param_num RSQB. [NUMBER precedence=18]\n\
    63: numeric_range ::= LSQB exclusive_param_num exclusive_param_num RSQB. [NUMBER precedence=18]\n\
    64: numeric_range ::= LSQB param_num RSQB. [NUMBER precedence=18]\n\
    65: expr ::= modifier NOT_EQUAL param_num. [NOT_EQUAL precedence=16]\n\
    66: expr ::= modifier EQUALS param_num. [EQUALS precedence=16]\n\
    67: expr ::= modifier GT param_num. [GT precedence=17]\n\
    68: expr ::= modifier GE param_num. [GE precedence=17]\n\
    69: expr ::= modifier LT param_num. [LT precedence=17]\n\
    70: expr ::= modifier LE param_num. [LE precedence=17]\n\
    71: expr ::= modifier COLON geo_filter. [COLON precedence=15]\n\
    72: geo_filter ::= LSQB param_num param_num param_num param_term RSQB. [NUMBER precedence=18]\n\
    73: expr ::= modifier COLON geometry_query. [COLON precedence=15]\n\
    74: geometry_query ::= LSQB TERM ATTRIBUTE RSQB. [LSQB precedence=11]\n\
    75: query ::= expr ARROW LSQB vector_query RSQB. [ARROW precedence=14]\n\
    76: query ::= text_expr ARROW LSQB vector_query RSQB. [ARROW precedence=14]\n\
    77: query ::= star ARROW LSQB vector_query RSQB. [ARROW precedence=14]\n\
    78: vector_query ::= vector_command vector_attribute_list vector_score_field.\n\
    79: vector_query ::= vector_command vector_score_field.\n\
    80: vector_query ::= vector_command vector_attribute_list.\n\
    81: vector_query ::= vector_command.\n\
    82: vector_score_field ::= as param_term_case.\n\
    83: query ::= expr ARROW LSQB vector_query RSQB ARROW LB attribute_list RB. [ARROW precedence=14]\n\
    84: query ::= text_expr ARROW LSQB vector_query RSQB ARROW LB attribute_list RB. [ARROW precedence=14]\n\
    85: query ::= star ARROW LSQB vector_query RSQB ARROW LB attribute_list RB. [ARROW precedence=14]\n\
    86: vector_command ::= TERM param_size modifier ATTRIBUTE. [TERM precedence=9]\n\
    87: vector_attribute ::= TERM param_term. [TERM precedence=9]\n\
    88: vector_attribute_list ::= vector_attribute_list vector_attribute.\n\
    89: vector_attribute_list ::= vector_attribute.\n\
    90: expr ::= modifier COLON LSQB vector_range_command RSQB. [COLON precedence=15]\n\
    91: vector_range_command ::= TERM param_num ATTRIBUTE. [TERM precedence=9]\n\
    92: num ::= SIZE. [SIZE precedence=19]\n\
    93: num ::= NUMBER. [NUMBER precedence=18]\n\
    94: num ::= MINUS num. [MINUS precedence=12]\n\
    95: term ::= TERM. [TERM precedence=9]\n\
    96: term ::= NUMBER. [NUMBER precedence=18]\n\
    97: term ::= SIZE. [SIZE precedence=19]\n\
    98: param_term ::= term.\n\
    99: param_term ::= ATTRIBUTE. [ATTRIBUTE precedence=25]\n\
    100: param_term_case ::= term.\n\
    101: param_term_case ::= ATTRIBUTE. [ATTRIBUTE precedence=25]\n\
    102: param_size ::= SIZE. [SIZE precedence=19]\n\
    103: param_size ::= ATTRIBUTE. [ATTRIBUTE precedence=25]\n\
    104: param_num ::= ATTRIBUTE. [ATTRIBUTE precedence=25]\n\
    105: param_num ::= MINUS ATTRIBUTE. [MINUS precedence=12]\n\
    106: param_num ::= num.\n\
    107: exclusive_param_num ::= LP num. [LP precedence=11]\n\
    108: exclusive_param_num ::= LP ATTRIBUTE. [LP precedence=11]\n\
    109: exclusive_param_num ::= LP MINUS ATTRIBUTE. [LP precedence=11]\n\
    110: star ::= STAR. [STAR precedence=20]\n\
    111: star ::= LP star RP. [LP precedence=11]\n\
    112: as ::= AS_T.\n\
    ";

    /// `parser.out`'s states (`lemon -p`), in [`Tables::report`]'s form.
    const V2_LEMON_STATES: &str = "\
    0:0r1 5s114 6t46 10t27 11t95 12s131 13s129 14s1 17s13 18s14 28t96 29t97 30t110 33t39 34t40 35t41 36s50 37t99 39t42 42s4 45t31 48t32 50s135 51s134 52s5 62s84 63s95 66a 67s90 68t30 69t98\n\
    1:5s114 6t46 10t27 11t95 12s131 13s129 14s1 17s13 18s14 28t96 29t97 30t110 33t39 34t40 35t41 36s50 37t99 39t42 42s2 45t31 48t32 50s135 51s134 52s3 62s84 63s95 67s103 68t30 69t98\n\
    2:4s12 5s114 6t46 7t18 10t27 11t95 12s131 13s129 14s15 17s13 18s14 20s112 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98\n\
    3:4s16 4y3 5s114 5y3 6t46 6y3 7t19 7y3 10t27 10y3 11t95 11y3 12s131 12y3 13s129 13y3 14s15 14y3 17s13 17y3 18s14 18y3 20s133 20y3 28t96 28y3 29t97 29y3 33t39 33y3 34t40 34y3 35t41 35y3 36s50 36y3 37t99 37y3 39t42 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98\n\
    4:0r0 4s12 5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 20s93 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98\n\
    5:4s16 4y3 5s114 5y3 6t46 6y3 10t27 10y3 11t95 11y3 12s131 12y3 13s129 13y3 14s15 14y3 17s13 17y3 18s14 18y3 20s87 20y3 28t96 28y3 29t97 29y3 33t39 33y3 34t40 34y3 35t41 35y3 36s50 36y3 37t99 37y3 39t42 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r3\n\
    6:4s16 4y3 5s114 5y3 6t46 6y3 10t27 10y3 11t95 11y3 12s131 12y3 13s129 13y3 14s15 14y3 17s13 17y3 18s14 18y3 20s133 20y3 28t96 28y3 29t97 29y3 33t39 33y3 34t40 34y3 35t41 35y3 36s50 36y3 37t99 37y3 39t42 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r3\n\
    7:4x12 5s114 5y10 6t46 6y10 10t27 10y10 11t95 11y10 12s131 12y10 13s129 13y10 14s15 14y10 17s13 17y10 18s14 18y10 20s112 20y10 28t96 28y10 29t97 29y10 33t39 33y10 34t40 34y10 35t41 35y10 36s50 36y10 37t99 37y10 39t42 39y10 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r10\n\
    8:4x12 5s114 5y11 6t46 6y11 10t27 10y11 11t95 11y11 12s131 12y11 13s129 13y11 14s15 14y11 17s13 17y11 18s14 18y11 20s112 20y11 28t96 28y11 29t97 29y11 33t39 33y11 34t40 34y11 35t41 35y11 36s50 36y11 37t99 37y11 39t42 39y11 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r11\n\
    9:4x12 5s114 5y9 6t46 6y9 10t27 10y9 11t95 11y9 12s131 12y9 13s129 13y9 14s15 14y9 17s13 17y9 18s14 18y9 20s112 20y9 28t96 28y9 29t97 29y9 33t39 33y9 34t40 34y9 35t41 35y9 36s50 36y9 37t99 37y9 39t42 39y9 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r9\n\
    10:0y3 4x16 4y3 5s114 5y12 5y3 6t46 6y12 6y3 7y3 10t27 10y12 10y3 11t95 11y12 11y3 12s131 12y12 12y3 13s129 13y12 13y3 14s15 14y12 14y3 17s13 17y12 17y3 18s14 18y12 18y3 20s133 20y12 20y3 28t96 28y12 28y3 29t97 29y12 29y3 33t39 33y12 33y3 34t40 34y12 34y3 35t41 35y12 35y3 36s50 36y12 36y3 37t99 37y12 37y3 39t42 39y12 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r12\n\
    11:0y3 4x16 4y3 5s114 5y14 5y3 6t46 6y14 6y3 7y3 10t27 10y14 10y3 11t95 11y14 11y3 12s131 12y14 12y3 13s129 13y14 13y3 14s15 14y14 14y3 17s13 17y14 17y3 18s14 18y14 18y3 20s133 20y14 20y3 28t96 28y14 28y3 29t97 29y14 29y3 33t39 33y14 33y3 34t40 34y14 34y3 35t41 35y14 35y3 36s50 36y14 36y3 37t99 37y14 37y3 39t42 39y14 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r14\n\
    12:5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s9 45t31 48t32 50s135 51s134 52s10 62s84 63s95 68t30 69t98\n\
    13:5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s19 45t31 48t32 50s135 51s134 52s20 62s84 63s95 68t30 69t98\n\
    14:5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s18 45t31 48t32 50s135 51s134 52s21 62s84 63s95 68t30 69t98\n\
    15:5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s2 45t31 48t32 50s135 51s134 52s3 62s84 63s95 68t30 69t98\n\
    16:5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s8 45t31 48t32 50s135 51s134 52s11 62s84 63s95 68t30 69t98\n\
    17:5s114 6t46 10t27 11t95 12s131 13s129 14s15 17s13 18s14 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 42s7 45t31 48t32 50s135 51s134 52s6 62s84 63s95 68t30 69t98\n\
    18:4x12 5x114 6x153 10x204 11x212 12x131 13x129 14x15 17x13 18x14 20s112 20y35 28t96 28y35 29t97 29y35 33t39 33y35 34t40 34y35 35t41 35y35 36s50 36y35 37t99 37y35 39t42 39y35 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r35\n\
    19:4x12 5x114 6x153 10x204 11x212 12x131 13x129 14x15 17x13 18x14 20s112 20y37 28t96 28y37 29t97 29y37 33t39 33y37 34t40 34y37 35t41 35y37 36s50 36y37 37t99 37y37 39t42 39y37 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r37\n\
    20:0y3 4x16 4y3 5x114 5y3 6x153 6y3 7y3 10x204 10y3 11x212 11y3 12x131 12y3 13x129 13y3 14x15 14y3 17x13 17y3 18x14 18y3 20s133 20y38 20y3 28t96 28y38 28y3 29t97 29y38 29y3 33t39 33y38 33y3 34t40 34y38 34y3 35t41 35y38 35y3 36s50 36y38 36y3 37t99 37y38 37y3 39t42 39y38 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r38\n\
    21:0y3 4x16 4y3 5x114 5y3 6x153 6y3 7y3 10x204 10y3 11x212 11y3 12x131 12y3 13x129 13y3 14x15 14y3 17x13 17y3 18x14 18y3 20s133 20y36 20y3 28t96 28y36 28y3 29t97 29y36 29y3 33t39 33y36 33y3 34t40 34y36 34y3 35t41 35y36 35y3 36s50 36y36 36y3 37t99 37y36 37y3 39t42 39y36 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r36\n\
    22:4x12 5x114 6x153 10x204 11x212 12x131 13x129 14x15 17x13 18x14 20s112 20y4 28t96 28y4 29t97 29y4 33t39 33y4 34t40 34y4 35t41 35y4 36s50 36y4 37t99 37y4 39t42 39y4 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r4\n\
    23:4x12 5x114 6x153 10x204 11x212 12x131 13x129 14x15 17x13 18x14 20s112 20y5 28t96 28y5 29t97 29y5 33t39 33y5 34t40 34y5 35t41 35y5 36s50 36y5 37t99 37y5 39t42 39y5 42s22 45t31 48t32 50s135 51s134 52s25 62s84 63s95 68t30 69t98 *r5\n\
    24:0y3 4x16 4y3 5x114 5y3 6x153 6y3 7y3 10x204 10y3 11x212 11y3 12x131 12y3 13x129 13y3 14x15 14y3 17x13 17y3 18x14 18y3 20s133 20y7 20y3 28t96 28y7 28y3 29t97 29y7 29y3 33t39 33y7 33y3 34t40 34y7 34y3 35t41 35y7 35y3 36s50 36y7 36y3 37t99 37y7 37y3 39t42 39y7 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r7\n\
    25:0y3 4x16 4y3 5x114 5y3 6x153 6y3 7y3 10x204 10y3 11x212 11y3 12x131 12y3 13x129 13y3 14x15 14y3 17x13 17y3 18x14 18y3 20s133 20y6 20y3 28t96 28y6 28y3 29t97 29y6 29y3 33t39 33y6 33y3 34t40 34y6 34y3 35t41 35y6 35y3 36s50 36y6 36y3 37t99 37y6 37y3 39t42 39y6 39y3 42s23 45t31 48t32 50s135 51s134 52s24 62s84 63s95 68t30 69t98 *r6\n\
    26:10t27 11t95 12s131 13s129 14s34 15s27 16s43 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s42 55t71 56t73 65t59 68t30 69t98\n\
    27:11t95 28t96 29t97 33t39 34t40 35t41 37s96 39t42 45t52 48t53 49s62 54s98 68s63 69s97 70t51\n\
    28:4s35 7t19 10t27 11t95 12s131 13s129 14s34 17s32 18s33 20s133 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s39 68t30 69t98\n\
    29:4x35 10t27 10y14 11t95 11y14 12s131 12y14 13s129 13y14 14s34 14y14 17s32 17y14 18s33 18y14 20s133 20y14 28t96 28y14 29t97 29y14 33t39 33y14 34t40 34y14 35t41 35y14 36s50 36y14 37t99 37y14 39t42 39y14 45t31 48t32 51s134 52s39 68t30 69t98 *r14\n\
    30:4x35 10t27 10y15 11t95 11y15 12s131 12y15 13s129 13y15 14s34 14y15 17s32 17y15 18s33 18y15 20s133 20y15 28t96 28y15 29t97 29y15 33t39 33y15 34t40 34y15 35t41 35y15 36s50 36y15 37t99 37y15 39t42 39y15 45t31 48t32 51s134 52s39 68t30 69t98 *r15\n\
    31:10t27 11t95 12s131 13s129 14s34 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s40 68t30 69t98\n\
    32:10t27 11t95 12s131 13s129 14s34 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s37 68t30 69t98\n\
    33:10t27 11t95 12s131 13s129 14s34 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s38 68t30 69t98\n\
    34:10t27 11t95 12s131 13s129 14s34 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s28 68t30 69t98\n\
    35:10t27 11t95 12s131 13s129 14s34 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s29 68t30 69t98\n\
    36:10t27 11t95 12s131 13s129 14s34 17s32 18s33 28t96 29t97 33t39 34t40 35t41 36s50 37t99 39t42 45t31 48t32 51s134 52s30 68t30 69t98\n\
    37:4x35 10x204 11x212 12x131 13x129 14x34 17x32 18x33 20s133 20y38 28t96 28y38 29t97 29y38 33t39 33y38 34t40 34y38 35t41 35y38 36s50 36y38 37t99 37y38 39t42 39y38 45t31 48t32 51s134 52s39 68t30 69t98 *r38\n\
    38:4x35 10x204 11x212 12x131 13x129 14x34 17x32 18x33 20s133 20y36 28t96 28y36 29t97 29y36 33t39 33y36 34t40 34y36 35t41 35y36 36s50 36y36 37t99 37y36 39t42 39y36 45t31 48t32 51s134 52s39 68t30 69t98 *r36\n\
    39:4x35 10x204 11x212 12x131 13x129 14x34 17x32 18x33 20s133 20y7 28t96 28y7 29t97 29y7 33t39 33y7 34t40 34y7 35t41 35y7 36s50 36y7 37t99 37y7 39t42 39y7 45t31 48t32 51s134 52s39 68t30 69t98 *r7\n\
    40:4x35 10x204 11x212 12x131 13x129 14x34 17x32 18x33 20x133 28t96 28y17 29t97 29y17 33t39 33y17 34t40 34y17 35t41 35y17 36s50 36y17 37t99 37y17 39t42 39y17 45t31 48t32 51s134 52s39 68t30 69t98 *r17\n\
    41:11t95 28t96 29t97 33t39 34t40 35t41 37s96 39t42 45t56 48t57 49s61 68s63 69s97 70t55\n\
    42:4x35 10x204 11x212 12x131 13x129 14x34 17x32 18x33 20x133 28t96 28y16 29t97 29y16 33t39 33y16 34t40 34y16 35t41 35y16 36s50 36y16 37t99 37y16 39t42 39y16 45t31 48t32 51s134 52s39 68t30 69t98 *r16\n\
    43:11s59 14s75 18s76 28t93 29t92 37t104 59s115 64t106 71s45 72s46\n\
    44:11s51 40t112 60t89 61s47 73t79 74s52 *r81\n\
    45:9t64 14s75 18s76 28t93 29t92 37t104 64t106 71s48 72s120\n\
    46:14s75 18s76 28t93 29t92 37t104 64t106 71s119 72s118\n\
    47:11s51 40t112 60t88 73t78 74s52 *r80\n\
    48:9t60 18s76 28t93 29t92 37t104 64t106 71s60\n\
    49:11t95 28t96 29t97 36s64 37t99 68s126 69t98\n\
    50:11t95 28t96 29t97 36s49 37t99 68s127 69t98\n\
    51:11t95 28t96 29t97 37t99 68t87 69t98\n\
    52:11t95 28t96 29t97 37t101 69t100 70t82\n\
    53:18s76 28t93 29t92 37t104 64t106 71t70\n\
    54:18s76 28t93 29t92 37t104 64t106 71t69\n\
    55:18s76 28t93 29t92 37t104 64t106 71t68\n\
    56:18s76 28t93 29t92 37t104 64t106 71t67\n\
    57:18s76 28t93 29t92 37t104 64t106 71t66\n\
    58:18s76 28t93 29t92 37t104 64t106 71t65\n\
    59:18s76 28t93 29t92 37s117 64t106 71s116\n\
    60:11t95 28t96 29t97 37t99 68s121 69t98\n\
    61:11t95 28t96 29t97 37t99 68t34 69t98 *r58\n\
    62:11t95 28t96 29t97 37t99 68t34 69t98 *r54\n\
    63:11t95 28t96 29t97 37t99 68t33 69t98\n\
    64:11t95 28t96 29t97 37t99 68s124 69t98\n\
    65:11t95 28t96 29t97 37t99 68t20 69t98\n\
    66:37s132 43t21 44s85 *r24\n\
    67:11s80 57s102 58s44\n\
    68:37s132 43t21 44s88 *r24\n\
    69:11s80 57s106 58s44\n\
    70:37s132 43t21 44s91 *r24\n\
    71:11s80 57s111 58s44\n\
    72:37s132 43t21 44s94 *r24\n\
    73:37s132 43t21 44s99 *r24\n\
    74:18s79 28t93 29t92 37t109 64t94\n\
    75:18s74 28t93 29t92 37t108 64t107\n\
    76:18s79 28t93 29t92 37t105 64t94\n\
    77:11t95 28t96 29t97 69t48\n\
    78:11t95 28t96 29t97 69t47\n\
    79:18s79 28t93 29t92 64t94\n\
    80:29t102 37t103 75s81\n\
    81:6t46 62s108\n\
    82:6t46 62s113\n\
    83:37s132 43t22 *r23\n\
    84:4s78 21s26 22s58 23s57 24s55 25s56 26s53 27s54\n\
    85:8s100 41s83\n\
    86:0r76 20s101\n\
    87:15s73 16s67\n\
    88:8s104 41s83\n\
    89:0r77 20s105\n\
    90:0r2 20s107\n\
    91:8s109 41s83\n\
    92:0r75 20s110\n\
    93:15s72 16s71\n\
    94:8t25 41s83\n\
    95:4s77 21s31\n\
    96:4r101 8r101 *r99\n\
    97:4r100 8r100 *r98\n\
    98:4s41 8t50\n\
    99:8t26 41s83\n\
    100:0r84\n\
    101:15s66\n\
    102:9s86\n\
    103:7t111\n\
    104:0r85\n\
    105:15s68\n\
    106:9s89\n\
    107:16s69\n\
    108:37t86\n\
    109:0r83\n\
    110:15s70\n\
    111:9s92\n\
    112:15s72\n\
    113:7t49\n\
    114:14s82\n\
    115:9t90\n\
    116:37t91\n\
    117:9t74 *r104\n\
    118:9t63\n\
    119:9t61\n\
    120:9t62\n\
    121:9t72\n\
    122:36t45\n\
    123:36s122\n\
    124:36s123\n\
    125:36t44\n\
    126:36s125\n\
    127:36t43\n\
    128:13t29\n\
    129:37s128\n\
    130:12t28\n\
    131:37s130\n\
    132:21s65\n\
    133:15s73\n\
    134:4s36 4y13 *r13\n\
    135:4s17 4y8 *r8\n\
    ";
}
