//! `generate_normalized_query`: a statement's text with its constants replaced by `$n` (a
//! squashed list by `$n /*, ... */`), numbered after its highest external parameter.

use super::jumble::{Jumble, Loc};

/// `fill_in_constant_lengths`: each constant's token length, found by scanning the text
/// (a `-` before a number is part of it); duplicates get -1.
fn lengths(text: &str, query_loc: i32, locs: &mut [Loc]) {
    locs.sort_by_key(|l| l.location);
    let tokens = pg_query::scan(text).map(|r| r.tokens).unwrap_or_default();
    let mut ti = 0;
    for i in 0..locs.len() {
        if i > 0 && locs[i].location == locs[i - 1].location {
            locs[i].len = -1;
            continue;
        }
        if locs[i].squashed {
            continue;
        }
        let loc = locs[i].location - query_loc;
        while ti < tokens.len() && tokens[ti].start < loc {
            ti += 1;
        }
        let Some(t) = tokens.get(ti) else { break };
        let end = if text.as_bytes().get(loc as usize) == Some(&b'-') {
            match tokens.get(ti + 1) {
                Some(n) => n.end,
                None => break,
            }
        } else {
            t.end
        };
        locs[i].len = end - loc;
    }
}

/// The normalized text of `text` (which starts at byte `query_loc` of the string the
/// locations count in).
pub fn normalize(text: &str, query_loc: i32, j: &Jumble) -> String {
    let mut locs = j.locs.clone();
    lengths(text, query_loc, &mut locs);
    let mut out = String::with_capacity(text.len() + locs.len() * 10);
    let (mut quer_loc, mut last_off, mut last_len) = (0i32, 0i32, 0i32);
    let mut replaced = 0;
    for l in &locs {
        if l.extern_param && !j.squashed {
            continue;
        }
        if l.len < 0 {
            continue;
        }
        let off = l.location - query_loc;
        let to_write = off - last_off - last_len;
        if to_write < 0 || off as usize > text.len() {
            continue;
        }
        out.push_str(&text[quer_loc as usize..(quer_loc + to_write) as usize]);
        replaced += 1;
        out.push_str(&format!("${}{}", replaced + j.highest_extern_param, if l.squashed { " /*, ... */" } else { "" }));
        quer_loc = off + l.len;
        last_off = off;
        last_len = l.len;
    }
    if (quer_loc as usize) <= text.len() {
        out.push_str(&text[quer_loc as usize..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::jumble::jumble;

    fn columns(_: Option<&str>, name: &str) -> Option<Vec<(String, String)>> {
        let col = |n: &str, t: &str| (n.to_string(), t.to_string());
        match name {
            "t" => Some(vec![col("id", "int4"), col("x", "text")]),
            "f" => Some(vec![col("data", "float8")]),
            _ => None,
        }
    }

    /// The query id and normalized text of one statement.
    fn norm(sql: &str) -> (i64, String) {
        let parsed = pg_query::parse(sql, 0).unwrap();
        let node = parsed.protobuf.stmts[0].stmt.as_ref().unwrap().node.as_ref().unwrap();
        let (id, j) = jumble(node, &columns, &|_, _, _| None, &[]);
        (id, super::normalize(sql, 0, &j))
    }

    #[test]
    fn constants_become_parameters() {
        let (_, text) = norm("SELECT id FROM t WHERE x = 'a' AND id > -1 LIMIT 5");
        assert_eq!(text, "SELECT id FROM t WHERE x = $1 AND id > $2 LIMIT $3");
        // Numbering continues after the statement's own parameters.
        assert_eq!(norm("SELECT $1, 2").1, "SELECT $1, $2");
    }

    #[test]
    fn grouping_ignores_constants_and_aliases() {
        assert_eq!(norm("SELECT 1 AS a").0, norm("SELECT 2 AS b").0);
        assert_eq!(norm("SELECT id FROM t").0, norm("SELECT id FROM public.t").0);
        assert_ne!(norm("SELECT 1").0, norm("SELECT 1::text").0);
        assert_ne!(norm("SELECT id FROM t ORDER BY 1").0, norm("SELECT id FROM t ORDER BY 2").0);
    }

    #[test]
    fn lists_squash() {
        let (a, text) = norm("SELECT * FROM t WHERE id IN (1, 2, 3)");
        assert_eq!(text, "SELECT * FROM t WHERE id IN ($1 /*, ... */)");
        assert_eq!(a, norm("SELECT * FROM t WHERE id IN (1, 2, 3, 4, 5)").0);
        assert_eq!(a, norm("SELECT * FROM t WHERE id = ANY(ARRAY[1, 2])").0);
        // One value is a plain comparison; a column in the list is not squashed.
        assert_ne!(a, norm("SELECT * FROM t WHERE id IN (1)").0);
        assert_eq!(norm("SELECT * FROM t WHERE id IN (1, id)").1, "SELECT * FROM t WHERE id IN ($1, id)");
    }

    #[test]
    fn list_types_follow_the_column() {
        let float_in = norm("SELECT * FROM f WHERE data IN (1, 2)").0;
        assert_eq!(float_in, norm("SELECT * FROM f WHERE data IN ('1', 2.5)").0);
        assert_eq!(float_in, norm("SELECT * FROM f WHERE data = ANY(ARRAY[1::float8, 2::float8])").0);
        // An integer array compares to a float with a cross-type operator: another query.
        assert_ne!(float_in, norm("SELECT * FROM f WHERE data = ANY(ARRAY[1, 2])").0);
    }
}
