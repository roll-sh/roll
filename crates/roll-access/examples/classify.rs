//! How the catalogue classifies routes another branch defines: one `METHOD<TAB>PATTERN` per line
//! on stdin (any further tab-separated columns are echoed), one answer per line on stdout. Used to
//! check a product's routes against the catalogue before it merges (docs/permissions.md).

use roll_access::catalog::{self, EnvFrom, Tgt};
use std::io::BufRead;

fn main() {
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let cols: Vec<&str> = line.split('\t').collect();
        let (method, route) = match cols.as_slice() {
            [_, m, r, ..] if m.chars().all(|c| c.is_ascii_uppercase() || c == '?') => (*m, *r),
            [m, r, ..] => (*m, *r),
            _ => continue,
        };
        let Some(found) = catalog::find(method, route, route).or_else(|| catalog::by_path(method, route)) else {
            println!("NONE\t{method}\t{route}");
            continue;
        };
        let row = found.row;
        let view = catalog::view(row);
        // The row's resource and environment parameters must be the route's own.
        let mut missing = Vec::new();
        if let Tgt::Param(names, _) = row.target {
            missing.extend(names.iter().filter(|n| !route.contains(&format!("{{{n}}}"))).map(|n| n.to_string()));
        }
        if let EnvFrom::Param(p) = row.env
            && !route.contains(&format!("{{{p}}}"))
        {
            missing.push(format!("env:{p}"));
        }
        let warn = if missing.is_empty() { String::new() } else { format!("\tMISSING PARAMS {}", missing.join(",")) };
        println!("{}\t{}\t{method}\t{route}\t(row {} {}){warn}", view.product, view.action, row.method, row.route);
    }
}
