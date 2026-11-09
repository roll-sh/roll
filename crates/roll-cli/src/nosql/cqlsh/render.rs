//! Result layout: cqlsh's `print_formatted_result`, `print_formatted_result_vertically` and
//! Python's `cmd.Cmd.columnize` (Apache Cassandra `cqlshmain.py`, Apache-2.0; CPython's `cmd`
//! module, PSF licence).

use super::format::Fv;

fn width(w: isize) -> usize {
    w.max(0) as usize
}

/// A table. `rows: None` prints the header and a blank line (an empty page).
pub fn table(names: &[Fv], rows: Option<&[Vec<Fv>]>, with_header: bool, tty: bool, color: bool) -> String {
    let mut widths: Vec<usize> = names.iter().map(|n| width(n.width)).collect();
    for row in rows.unwrap_or_default() {
        for (i, c) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(width(c.width));
            }
        }
    }
    let mut out = String::new();
    if with_header {
        let header: Vec<String> = names.iter().zip(&widths).map(|(n, w)| n.ljust(*w, color)).collect();
        out.push(' ');
        out.push_str(header.join(" | ").trim_end());
        out.push('\n');
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        out.push_str(&format!("-{}-\n", rule.join("-+-")));
    }
    let Some(rows) = rows else {
        out.push('\n');
        return out;
    };
    for row in rows {
        let cells: Vec<String> = row.iter().zip(&widths).map(|(c, w)| c.rjust(*w, color)).collect();
        out.push(' ');
        out.push_str(&cells.join(" | "));
        out.push('\n');
    }
    if tty {
        out.push('\n');
    }
    out
}

/// EXPAND ON: one block per row, numbered from `offset + 1`.
pub fn vertical(names: &[Fv], rows: &[Vec<Fv>], offset: usize, color: bool) -> String {
    let col = names.iter().map(|n| width(n.width)).max().unwrap_or(0);
    let val = rows.iter().flatten().map(|v| width(v.width)).max().unwrap_or(0);
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        out.push_str(&format!("@ Row {}\n", offset + i + 1));
        out.push_str(&format!("-{}-+-{}-\n", "-".repeat(col), "-".repeat(val)));
        for (n, v) in names.iter().zip(row) {
            out.push_str(&format!(" {} | {}\n", n.ljust(col, color), v.ljust(width(v.width), color)));
        }
        out.push('\n');
    }
    out
}

/// `cmd.Cmd.columnize(list, displaywidth)`.
pub fn columnize(list: &[String], displaywidth: usize) -> String {
    if list.is_empty() {
        return "<empty>\n".into();
    }
    let size = list.len();
    if size == 1 {
        return format!("{}\n", list[0]);
    }
    let len = |s: &String| s.chars().count();
    let mut found = None;
    for nrows in 1..size {
        let ncols = size.div_ceil(nrows);
        let mut colwidths = vec![];
        let mut total: isize = -2;
        for col in 0..ncols {
            let w = (0..nrows).map(|row| row + nrows * col).take_while(|i| *i < size).map(|i| len(&list[i])).max().unwrap_or(0);
            colwidths.push(w);
            total += w as isize + 2;
            if total > displaywidth as isize {
                break;
            }
        }
        if total <= displaywidth as isize {
            found = Some((nrows, ncols, colwidths));
            break;
        }
    }
    let (nrows, ncols, colwidths) = found.unwrap_or((size, 1, vec![0]));
    let mut out = String::new();
    for row in 0..nrows {
        let mut texts: Vec<&str> = (0..ncols).map(|col| list.get(row + nrows * col).map(String::as_str).unwrap_or("")).collect();
        while texts.last().is_some_and(|t| t.is_empty()) {
            texts.pop();
        }
        let padded: Vec<String> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{t}{}", " ".repeat(colwidths.get(i).copied().unwrap_or(0).saturating_sub(t.chars().count()))))
            .collect();
        out.push_str(&padded.join("  "));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fv(s: &str) -> Fv {
        Fv::plain(s)
    }

    #[test]
    fn tables() {
        let names = [fv("key"), fv("cluster_name")];
        let rows = vec![vec![fv("local"), fv("")]];
        assert_eq!(
            table(&names, Some(&rows), true, false, false),
            " key   | cluster_name\n-------+--------------\n local |             \n"
        );
        assert_eq!(table(&[fv("k")], None, true, false, false), " k\n---\n\n");
        assert_eq!(table(&[fv("k")], Some(&[vec![fv("1")]]), false, true, false), " 1\n\n");
        // An empty collection counts as zero columns wide.
        let empty = Fv { text: "[]".into(), colored: "[]".into(), width: 0 };
        assert_eq!(table(&[fv("fl")], Some(&[vec![empty]]), true, false, false), " fl\n----\n   []\n");
    }

    #[test]
    fn expanded() {
        let names = [fv("k"), fv("li")];
        let rows = vec![vec![fv("1"), fv("[1, 2, 3]")], vec![fv("2"), fv("null")]];
        assert_eq!(
            vertical(&names, &rows, 0, false),
            "@ Row 1\n----+-----------\n k  | 1\n li | [1, 2, 3]\n\n@ Row 2\n----+-----------\n k  | 2\n li | null\n\n"
        );
    }

    #[test]
    fn columns() {
        let names: Vec<String> = ["cli_x", "eng_k2", "eng_k3", "eng_k4", "eng_k6", "eng_k7", "system", "system_auth", "system_distributed"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            columnize(&names, 80),
            "cli_x   eng_k3  eng_k6  system       system_distributed\neng_k2  eng_k4  eng_k7  system_auth\n"
        );
        assert_eq!(columnize(&[], 80), "<empty>\n");
        assert_eq!(columnize(&["t".into()], 80), "t\n");
        assert_eq!(columnize(&["t".into(), "\"t$paxos\"".into()], 80), "t  \"t$paxos\"\n");
    }
}
