//! Keyspace replication: parsing `WITH replication = {…}`, validation, normalisation and the
//! warnings each reference emits.
//!
//! Cassandra follows `locator.AbstractReplicationStrategy.prepareReplicationStrategyOptions`,
//! `NetworkTopologyStrategy` / `SimpleStrategy` (`prepareOptions`, `validateExpectedOptions`,
//! `validateOptions`, `maybeWarnOnOptions`) and `ReplicationFactor` (Apache-2.0). ScyllaDB's rules
//! (tablets, the minimum-RF and RF-rack warnings, texts) come from the reference image.
//!
//! A database presents one node in one datacenter, so the only valid datacenter is the configured
//! one and data is never under-replicated: the options are validated and stored, nothing more.

use std::collections::BTreeMap;

use nosql_types::Dialect;

use crate::error::{Error, Result};

pub const LOCATOR: &str = "org.apache.cassandra.locator.";

/// A keyspace's replication: the strategy class (full name) and its options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replication {
    pub class: String,
    /// The options; a datacenter given a rack list has its rack count here.
    pub options: BTreeMap<String, String>,
    /// ScyllaDB's rack lists (`'dc': ['rack1']`), by datacenter (`replication_v2`).
    pub racks: BTreeMap<String, Vec<String>>,
}

impl Replication {
    pub fn new(class: &str, options: &[(&str, &str)]) -> Self {
        let class = if class.contains('.') {
            class.to_string()
        } else {
            format!("{LOCATOR}{class}")
        };
        Replication {
            class,
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            racks: BTreeMap::new(),
        }
    }

    pub fn local() -> Self {
        Replication::new("LocalStrategy", &[])
    }

    pub fn simple(rf: u32) -> Self {
        Replication::new("SimpleStrategy", &[("replication_factor", &rf.to_string())])
    }

    pub fn short_class(&self) -> &str {
        self.class.rsplit('.').next().unwrap_or(&self.class)
    }

    /// The `system_schema.keyspaces.replication` map (`class` included).
    pub fn as_map(&self) -> BTreeMap<String, String> {
        let mut m = self.options.clone();
        m.insert("class".into(), self.class.clone());
        m
    }

    pub fn from_map(mut m: BTreeMap<String, String>) -> Self {
        let class = m
            .remove("class")
            .unwrap_or_else(|| format!("{LOCATOR}NetworkTopologyStrategy"));
        Replication {
            class,
            options: m,
            racks: BTreeMap::new(),
        }
    }

    /// ScyllaDB's `system_schema.keyspaces.replication_v2`: the options with each rack list as
    /// `dc:<i>` entries; `None` without rack lists.
    pub fn v2_map(&self) -> Option<BTreeMap<String, String>> {
        if self.racks.is_empty() {
            return None;
        }
        let mut m = BTreeMap::from([("class".to_string(), self.class.clone())]);
        for (k, v) in &self.options {
            match self.racks.get(k) {
                Some(racks) => {
                    for (i, r) in racks.iter().enumerate() {
                        m.insert(format!("{k}:{i}"), r.clone());
                    }
                }
                None => {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        Some(m)
    }

    /// Takes the rack lists of a stored `replication_v2` map.
    pub fn with_v2(mut self, v2: &BTreeMap<String, String>) -> Self {
        let mut racks: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
        for (k, v) in v2 {
            if let Some((dc, i)) = k.rsplit_once(':')
                && let Ok(i) = i.parse::<usize>()
            {
                racks.entry(dc.to_string()).or_default().push((i, v.clone()));
            }
        }
        for (dc, mut rs) in racks {
            rs.sort();
            self.racks.insert(dc, rs.into_iter().map(|(_, r)| r).collect());
        }
        self
    }
}

/// The marker of a rack list among replication option values (`crate::schema::params::Properties::map`):
/// `LIST_MARK` then each rack followed by a NUL.
pub const LIST_MARK: char = '\u{1}';

/// The racks of a rack-list option value.
pub fn rack_list(v: &str) -> Option<Vec<String>> {
    v.strip_prefix(LIST_MARK)
        .map(|l| l.split_terminator('\0').map(str::to_string).collect())
}

/// `['rack1', 'rack2']` (as ScyllaDB prints a rack list).
pub fn rack_list_text(racks: &[String]) -> String {
    let items: Vec<String> = racks.iter().map(|r| format!("'{r}'")).collect();
    format!("[{}]", items.join(", "))
}

/// ScyllaDB's keyspace `STORAGE = {'type': …}`: `LOCAL` (no options) or `S3` (`bucket`,
/// `endpoint`; roll configures no S3 endpoint). A keyspace's storage can't change type;
/// `alter` is an ALTER (keyspaces are always `LOCAL`).
pub fn check_storage(m: Option<&BTreeMap<String, String>>, alter: bool) -> Result<()> {
    let Some(m) = m else {
        return Ok(());
    };
    let Some(ty) = m.get("type") else {
        return Ok(());
    };
    if ty != "LOCAL" && ty != "S3" {
        return Err(Error::invalid(format!("Unknown storage type: {ty}")));
    }
    if alter && ty != "LOCAL" {
        return Err(Error::invalid(format!(
            "Cannot alter storage options: LOCAL to {ty} is not supported"
        )));
    }
    let options: Vec<&str> = m.keys().map(String::as_str).filter(|k| *k != "type").collect();
    if ty == "LOCAL" {
        if !options.is_empty() {
            return Err(Error::invalid("Local storage does not accept any custom options"));
        }
        return Ok(());
    }
    for k in ["bucket", "endpoint"] {
        if !m.contains_key(k) {
            return Err(Error::invalid(format!("Missing S3 option: {k}")));
        }
    }
    if options.len() > 2 {
        return Err(Error::invalid(format!(
            "Extraneous options for S3: {}; allowed: bucket,endpoint",
            options.join(",")
        )));
    }
    Err(Error::config(format!("Endpoint {} not configured", m["endpoint"])))
}

/// Whether ScyllaDB knows the replication strategy class `written`.
pub fn known_class(written: &str) -> bool {
    class_of(written).is_some()
}

/// What a `CREATE` / `ALTER KEYSPACE` asked for.
pub struct Request<'a> {
    pub keyspace: &'a str,
    /// `replication = {…}`, `None` when absent.
    pub replication: Option<BTreeMap<String, String>>,
    /// The keyspace before an `ALTER`.
    pub previous: Option<&'a Replication>,
    /// The keyspace uses (or will use) tablets (ScyllaDB).
    pub tablets: bool,
    pub datacenter: &'a str,
    /// The rack of the presented node (rack lists may name only it).
    pub rack: &'a str,
    pub racks: u32,
    pub nodes: u32,
    /// ScyllaDB's replication guardrails.
    pub guard: Guard<'a>,
}

/// ScyllaDB's replication strategy and factor guardrails (`crate::guardrails`): the lists and
/// thresholds of the live settings, and the counters their violations go to.
#[derive(Clone, Debug)]
pub struct Guard<'a> {
    pub warn_list: Vec<String>,
    pub fail_list: Vec<String>,
    /// `restrict_replication_simplestrategy`: `(true, warn)`.
    pub restrict_simple: (bool, bool),
    pub min_warn: i64,
    pub min_fail: i64,
    pub max_warn: i64,
    pub max_fail: i64,
    pub metrics: Option<&'a crate::guardrails::Metrics>,
}

impl<'a> Guard<'a> {
    /// The guardrails of `s`, counting into `metrics`.
    pub fn of(s: &crate::Settings, metrics: Option<&'a crate::guardrails::Metrics>) -> Self {
        let g = &s.scylla;
        Guard {
            warn_list: g.replication_strategy_warn_list.clone(),
            fail_list: g.replication_strategy_fail_list.clone(),
            restrict_simple: crate::guardrails::config_table::restriction_mode(
                &g.restrict_replication_simplestrategy,
            ),
            min_warn: s.minimum_replication_factor_warn_threshold,
            min_fail: g.minimum_replication_factor_fail_threshold,
            max_warn: g.maximum_replication_factor_warn_threshold,
            max_fail: g.maximum_replication_factor_fail_threshold,
            metrics,
        }
    }

    fn count(&self, c: impl Fn(&crate::guardrails::Metrics) -> &std::sync::atomic::AtomicU64) {
        if let Some(m) = self.metrics {
            crate::guardrails::Metrics::inc(c(m));
        }
    }
}

impl Default for Guard<'_> {
    /// The reference image's defaults.
    fn default() -> Self {
        Guard::of(&crate::Settings::defaults(Dialect::Scylla), None)
    }
}

/// Validates and normalises a replication request; appends the reference's warnings.
pub fn prepare(d: Dialect, req: Request, warnings: &mut Vec<String>) -> Result<Replication> {
    match d {
        Dialect::Cassandra => prepare_cassandra(req, warnings),
        Dialect::Scylla => prepare_scylla(req, warnings),
    }
}

fn class_of(written: &str) -> Option<&'static str> {
    let short = written.strip_prefix(LOCATOR).unwrap_or(written);
    Some(match short {
        "SimpleStrategy" => "SimpleStrategy",
        "NetworkTopologyStrategy" => "NetworkTopologyStrategy",
        "LocalStrategy" => "LocalStrategy",
        "EverywhereStrategy" => "EverywhereStrategy",
        _ => return None,
    })
}

fn prepare_cassandra(req: Request, warnings: &mut Vec<String>) -> Result<Replication> {
    let ks = req.keyspace;
    let mut opts = match req.replication {
        Some(m) => m,
        None => {
            return req
                .previous
                .cloned()
                .ok_or_else(|| Error::config("Missing replication strategy class"));
        }
    };
    let Some(written) = opts.remove("class") else {
        return Err(Error::config("Missing replication strategy class"));
    };
    let full = if written.contains('.') {
        written.clone()
    } else {
        format!("{LOCATOR}{written}")
    };
    let class = match class_of(&full) {
        Some("EverywhereStrategy") | None => {
            return Err(Error::config(format!(
                "Unable to find replication strategy class '{full}'"
            )));
        }
        Some(c) => c,
    };
    if class == "LocalStrategy" {
        return Err(Error::invalid(
            "Unable to use given strategy class: LocalStrategy is reserved for internal use.",
        ));
    }
    let previous: BTreeMap<String, String> =
        req.previous.map(|p| p.options.clone()).unwrap_or_default();
    if class == "SimpleStrategy" {
        if !opts.contains_key("replication_factor") {
            let rf = previous
                .get("replication_factor")
                .cloned()
                .unwrap_or_else(|| "1".into());
            opts.insert("replication_factor".into(), rf);
        }
        for k in opts.keys() {
            if k != "replication_factor" {
                return Err(Error::config(format!(
                    "Unrecognized strategy option {{{k}}} passed to SimpleStrategy for keyspace {ks}"
                )));
            }
        }
        let rf = parse_cassandra_rf(&opts["replication_factor"])?;
        if rf > req.nodes && req.nodes != 0 {
            warnings.push(format!("Your replication factor {rf} for keyspace {ks} is higher than the number of nodes {}", req.nodes));
        }
        return Ok(Replication {
            class: format!("{LOCATOR}SimpleStrategy"),
            options: opts,
            racks: BTreeMap::new(),
        });
    }
    // NetworkTopologyStrategy.prepareOptions
    if opts.is_empty() {
        let rf = previous
            .get("replication_factor")
            .cloned()
            .unwrap_or_else(|| "1".into());
        opts.insert("replication_factor".into(), rf);
    }
    let mut replication = opts.remove("replication_factor");
    if replication.is_none() && opts.is_empty() {
        replication = previous.get("replication_factor").cloned();
    } else if replication.is_some() {
        for (k, v) in &previous {
            if k != "replication_factor" {
                opts.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    if let Some(rf) = replication {
        let n = parse_cassandra_rf(&rf)?;
        opts.entry(req.datacenter.to_string())
            .or_insert_with(|| n.to_string());
    }
    opts.retain(|_, v| v != "0");
    if opts.is_empty() {
        return Err(Error::config(
            "Configuration for at least one datacenter must be present",
        ));
    }
    for k in opts.keys() {
        if k != req.datacenter {
            return Err(Error::config(format!(
                "Unrecognized strategy option {{{k}}} passed to NetworkTopologyStrategy for keyspace {ks}"
            )));
        }
    }
    for v in opts.values() {
        parse_cassandra_rf(v)?;
    }
    for (dc, v) in &opts {
        let rf = parse_cassandra_rf(v)?;
        if rf > req.nodes && req.nodes != 0 {
            warnings.push(format!(
                "Your replication factor {rf} for keyspace {ks} is higher than the number of nodes {} for datacenter {dc}",
                req.nodes
            ));
        }
    }
    Ok(Replication {
        class: format!("{LOCATOR}NetworkTopologyStrategy"),
        options: opts,
        racks: BTreeMap::new(),
    })
}

/// `ReplicationFactor.fromString` / `validateReplicationFactor`.
fn parse_cassandra_rf(s: &str) -> Result<u32> {
    if s.contains('/') {
        return Err(Error::config(
            "Transient replication is not enabled on this node",
        ));
    }
    match s.trim().parse::<i64>() {
        Ok(n) if n < 0 => Err(Error::config(format!(
            "Replication factor must be non-negative, found {n}"
        ))),
        Ok(n) if n > i32::MAX as i64 => Err(Error::config(format!("For input string: \"{s}\""))),
        Ok(n) => Ok(n as u32),
        Err(_) => Err(Error::config(format!("For input string: \"{s}\""))),
    }
}

fn parse_scylla_rf(s: &str) -> Result<u32> {
    let t = s.trim();
    if !t.is_empty()
        && t.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = t.parse::<u32>()
    {
        return Ok(n);
    }
    Err(Error::config(format!(
        "Replication factor must be numeric and non-negative, found '{s}'"
    )))
}

fn prepare_scylla(req: Request, warnings: &mut Vec<String>) -> Result<Replication> {
    let ks = req.keyspace;
    let explicit = req.replication.is_some();
    let mut opts = match req.replication {
        Some(m) => m,
        None => match req.previous {
            Some(p) => return Ok(p.clone()),
            None => BTreeMap::from([("replication_factor".to_string(), "1".to_string())]),
        },
    };
    let explicit_class = opts.contains_key("class");
    let written = opts
        .remove("class")
        .unwrap_or_else(|| "NetworkTopologyStrategy".into());
    let Some(class) = class_of(&written) else {
        return Err(Error::config(format!("unable to find class '{written}'")));
    };
    let mut warn_rf: Vec<(String, u32)> = Vec::new();
    let mut racks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    match class {
        "SimpleStrategy" => {
            let Some(rf) = opts.get("replication_factor") else {
                return Err(Error::config(
                    "SimpleStrategy requires a replication_factor strategy option.",
                ));
            };
            if rack_list(rf).is_some() {
                return Err(Error::config("'replication_factor' option must be numeric."));
            }
            let n = parse_scylla_rf(rf)?;
            for k in opts.keys() {
                if k != "replication_factor" {
                    return Err(Error::config(format!(
                        "Unrecognized strategy option {{{k}}} passed to SimpleStrategy"
                    )));
                }
            }
            if req.tablets {
                return Err(Error::config(
                    "SimpleStrategy doesn't support tablet replication",
                ));
            }
            warn_rf.push(("replication_factor".into(), n));
        }
        "NetworkTopologyStrategy" => {
            let given = !opts.is_empty();
            for (k, v) in &opts {
                if k != "replication_factor" && k != req.datacenter {
                    return Err(Error::config(format!(
                        "Unrecognized strategy option {{{k}}} passed to NetworkTopologyStrategy"
                    )));
                }
                if let Some(list) = rack_list(v) {
                    if k == "replication_factor" {
                        return Err(Error::config(format!(
                            "Invalid replication factor: {}: must be a string holding a numerical value",
                            rack_list_text(&list)
                        )));
                    }
                    check_racks(k, &list, req.tablets, req.rack)?;
                    warn_rf.push((k.clone(), list.len() as u32));
                    continue;
                }
                let n = parse_scylla_rf(v)?;
                warn_rf.push((k.clone(), n));
            }
            if let Some(rf) = opts.remove("replication_factor") {
                opts.entry(req.datacenter.to_string()).or_insert(rf);
            }
            // A new keyspace without datacenters gets RF 1 in this one.
            if !given && req.previous.is_none() {
                opts.insert(req.datacenter.to_string(), "1".into());
            }
            let previous = req.previous.filter(|p| p.short_class() == "NetworkTopologyStrategy");
            if let Some(p) = previous {
                for (dc, rf) in &p.options {
                    if !opts.contains_key(dc) && rf.parse::<u32>().is_ok_and(|n| n > 0) {
                        return Err(Error::config(format!(
                            "Attempted to implicitly drop replicas in datacenter {dc}. If this is the desired behavior, set replication factor to 0 in {dc} explicitly."
                        )));
                    }
                }
            }
            for (k, v) in opts.iter_mut() {
                if let Some(list) = rack_list(v) {
                    *v = list.len().to_string();
                    if !list.is_empty() {
                        racks.insert(k.clone(), list);
                    }
                    continue;
                }
                let n = v.trim().parse::<u32>().ok();
                // A rack list stays when its count is kept.
                if let Some(old) = previous.and_then(|p| p.racks.get(k)) {
                    match n {
                        Some(n) if n as usize == old.len() => {
                            racks.insert(k.clone(), old.clone());
                        }
                        Some(0) => {}
                        _ => {
                            return Err(Error::config(format!(
                                "Cannot change replication factor for '{k}' from {} to '{v}' when the old value was a rack list",
                                rack_list_text(old)
                            )));
                        }
                    }
                }
                if let Some(n) = n {
                    *v = n.to_string();
                }
            }
            opts.retain(|_, v| v.trim().parse::<u32>().map(|n| n > 0).unwrap_or(true));
        }
        other => {
            if req.tablets {
                return Err(Error::config(format!(
                    "{other} doesn't support tablet replication"
                )));
            }
            // Any options are kept as written; numeric ones go through the RF guardrails.
            for (k, v) in &opts {
                if let Ok(n) = parse_scylla_rf(v) {
                    warn_rf.push((k.clone(), n));
                }
            }
        }
    }
    if explicit_class {
        strategy_guardrails(&req.guard, &written, class, ks, warnings)?;
    }
    if explicit {
        for (k, n) in &warn_rf {
            if *n > 0 && *n != req.racks && class == "NetworkTopologyStrategy" && req.tablets {
                warnings.push(format!(
                    "Keyspace '{ks}' is not RF-rack-valid: the replication factor doesn't match the rack count in at least one datacenter. A rack failure may reduce availability. For more context, see: https://docs.scylladb.com/manual/stable/reference/glossary.html#term-RF-rack-valid-keyspace."
                ));
                break;
            }
            let _ = k;
        }
        rf_guardrails(&req.guard, &warn_rf, warnings)?;
    }
    Ok(Replication {
        class: format!("{LOCATOR}{class}"),
        options: opts,
        racks,
    })
}

/// A rack list of datacenter `dc` (ScyllaDB): only for tablets keyspaces, without duplicates,
/// naming racks of the datacenter.
fn check_racks(dc: &str, list: &[String], tablets: bool, rack: &str) -> Result<()> {
    if !tablets {
        return Err(Error::config(format!(
            "Using rack list for '{dc}' is not allowed because the keyspace is not using tablets"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    if !list.iter().all(|r| seen.insert(r)) {
        return Err(Error::config(format!(
            "Rack list for '{dc}' contains duplicate entries"
        )));
    }
    if let Some(r) = list.iter().find(|r| r.as_str() != rack) {
        return Err(Error::config(format!(
            "Unrecognized rack name '{r}'. allowed_racks={{\"{rack}\"}}"
        )));
    }
    Ok(())
}

/// `replication_strategy_{fail,warn}_list` (and the legacy `restrict_replication_simplestrategy`):
/// a listed strategy fails the statement, or warns. `written` is the class as the statement
/// wrote it.
fn strategy_guardrails(
    g: &Guard,
    written: &str,
    class: &str,
    ks: &str,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let listed = |l: &[String]| l.iter().any(|c| c == class);
    let simple = class == "SimpleStrategy";
    if listed(&g.fail_list) || (simple && g.restrict_simple.0) {
        g.count(|m| &m.replication_strategy_fail_list_violations);
        return Err(Error::config(format!(
            "{written} replication class is not recommended, and forbidden by the current configuration, but was used for keyspace {ks}. You may override this restriction by modifying replication_strategy_fail_list configuration option to not list {written}."
        )));
    }
    if listed(&g.warn_list) || (simple && g.restrict_simple.1) {
        g.count(|m| &m.replication_strategy_warn_list_violations);
        warnings.push(format!(
            "{written} replication class is not recommended, but was used for keyspace {ks}. You may suppress this warning by delisting {written} from replication_strategy_warn_list configuration option, or make it into an error by listing this replication strategy on replication_strategy_fail_list."
        ));
    }
    Ok(())
}

/// `{minimum,maximum}_replication_factor_{fail,warn}_threshold` (-1 is off) for each written
/// replication factor; 0 ("not in this datacenter") never trips them.
fn rf_guardrails(g: &Guard, rfs: &[(String, u32)], warnings: &mut Vec<String>) -> Result<()> {
    for (k, n) in rfs {
        let rf = *n as i64;
        if rf == 0 {
            continue;
        }
        if g.min_fail >= 0 && rf < g.min_fail {
            g.count(|m| &m.minimum_replication_factor_fail_violations);
            return Err(Error::config(format!(
                "Replication Factor {k}={n} is forbidden by the current configuration setting of minimum_replication_factor_fail_threshold={}. Please increase replication factor, or lower minimum_replication_factor_fail_threshold set in the configuration.",
                g.min_fail
            )));
        }
        if g.max_fail >= 0 && rf > g.max_fail {
            g.count(|m| &m.maximum_replication_factor_fail_violations);
            return Err(Error::config(format!(
                "Replication Factor {k}={n} is forbidden by the current configuration setting of maximum_replication_factor_fail_threshold={}. Please decrease replication factor, or increase maximum_replication_factor_fail_threshold set in the configuration.",
                g.max_fail
            )));
        }
        if g.min_warn >= 0 && rf < g.min_warn {
            g.count(|m| &m.minimum_replication_factor_warn_violations);
            warnings.push(format!(
                "Using Replication Factor {k}={n} lower than the minimum_replication_factor_warn_threshold={} is not recommended.",
                g.min_warn
            ));
        } else if g.max_warn >= 0 && rf > g.max_warn {
            g.count(|m| &m.maximum_replication_factor_warn_violations);
            warnings.push(format!(
                "Using Replication Factor {k}={n} greater than the maximum_replication_factor_warn_threshold={} is not recommended.",
                g.max_warn
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(ks: &'a str, m: Option<&[(&str, &str)]>, tablets: bool) -> Request<'a> {
        Request {
            keyspace: ks,
            replication: m.map(|m| {
                m.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            }),
            previous: None,
            tablets,
            datacenter: "datacenter1",
            rack: "rack1",
            racks: 1,
            nodes: 1,
            guard: Guard::default(),
        }
    }

    fn run(d: Dialect, r: Request) -> (Result<Replication>, Vec<String>) {
        let mut w = Vec::new();
        (prepare(d, r, &mut w), w)
    }

    fn err(d: Dialect, r: Request) -> (crate::ErrorCode, String) {
        let e = run(d, r).0.unwrap_err();
        (e.code, e.message)
    }

    #[test]
    fn scylla_rules() {
        let (r, w) = run(Dialect::Scylla, req("k", None, true));
        assert_eq!(
            r.unwrap().as_map(),
            BTreeMap::from([
                ("class".into(), format!("{LOCATOR}NetworkTopologyStrategy")),
                ("datacenter1".into(), "1".into())
            ])
        );
        assert!(w.is_empty());
        let (r, w) = run(
            Dialect::Scylla,
            req("k", Some(&[("replication_factor", "1")]), true),
        );
        assert_eq!(r.unwrap().options["datacenter1"], "1");
        assert_eq!(
            w,
            [
                "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended."
            ]
        );
        let (_, w) = run(
            Dialect::Scylla,
            req(
                "x8",
                Some(&[("class", "NetworkTopologyStrategy"), ("datacenter1", "3")]),
                true,
            ),
        );
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("Keyspace 'x8' is not RF-rack-valid"));
        assert_eq!(
            err(
                Dialect::Scylla,
                req(
                    "k",
                    Some(&[("class", "NetworkTopologyStrategy"), ("nodc", "1")]),
                    true
                )
            )
            .1,
            "Unrecognized strategy option {nodc} passed to NetworkTopologyStrategy"
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                req("k", Some(&[("class", "SimpleStrategy")]), true)
            )
            .1,
            "SimpleStrategy requires a replication_factor strategy option."
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                req(
                    "k",
                    Some(&[("class", "SimpleStrategy"), ("replication_factor", "1")]),
                    true
                )
            )
            .1,
            "SimpleStrategy doesn't support tablet replication"
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                req("k", Some(&[("class", "FooStrategy")]), true)
            ),
            (
                crate::ErrorCode::Config,
                "unable to find class 'FooStrategy'".into()
            )
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                req("k", Some(&[("class", "LocalStrategy")]), true)
            )
            .1,
            "LocalStrategy doesn't support tablet replication"
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                req("k", Some(&[("class", "EverywhereStrategy")]), true)
            )
            .1,
            "EverywhereStrategy doesn't support tablet replication"
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                req(
                    "k",
                    Some(&[("class", "SimpleStrategy"), ("replication_factor", "abc")]),
                    true
                )
            )
            .1,
            "Replication factor must be numeric and non-negative, found 'abc'"
        );
        for bad in ["-1", "3/1"] {
            assert_eq!(
                err(
                    Dialect::Scylla,
                    req(
                        "k",
                        Some(&[("class", "NetworkTopologyStrategy"), ("datacenter1", bad)]),
                        true
                    )
                )
                .1,
                format!("Replication factor must be numeric and non-negative, found '{bad}'")
            );
        }
        let (r, w) = run(
            Dialect::Scylla,
            req(
                "k",
                Some(&[("class", "NetworkTopologyStrategy"), ("datacenter1", "0")]),
                true,
            ),
        );
        assert!(r.unwrap().options.is_empty());
        assert!(w.is_empty());
        let (r, w) = run(
            Dialect::Scylla,
            req(
                "w2",
                Some(&[("class", "SimpleStrategy"), ("replication_factor", "1")]),
                false,
            ),
        );
        assert_eq!(r.unwrap().short_class(), "SimpleStrategy");
        assert_eq!(w.len(), 2);
        assert!(w[0].starts_with(
            "SimpleStrategy replication class is not recommended, but was used for keyspace w2."
        ));
    }

    #[test]
    fn cassandra_rules() {
        let (r, w) = run(
            Dialect::Cassandra,
            req("k", Some(&[("class", "SimpleStrategy")]), false),
        );
        assert_eq!(r.unwrap().options["replication_factor"], "1");
        assert!(w.is_empty());
        let (r, w) = run(
            Dialect::Cassandra,
            req(
                "eng_k3",
                Some(&[
                    ("class", "NetworkTopologyStrategy"),
                    ("replication_factor", "2"),
                ]),
                false,
            ),
        );
        assert_eq!(
            r.unwrap().options,
            BTreeMap::from([("datacenter1".into(), "2".into())])
        );
        assert_eq!(
            w,
            [
                "Your replication factor 2 for keyspace eng_k3 is higher than the number of nodes 1 for datacenter datacenter1"
            ]
        );
        let (r, _) = run(
            Dialect::Cassandra,
            req(
                "k",
                Some(&[
                    ("class", "NetworkTopologyStrategy"),
                    ("datacenter1", "1"),
                    ("replication_factor", "2"),
                ]),
                false,
            ),
        );
        assert_eq!(r.unwrap().options["datacenter1"], "1");
        assert_eq!(
            err(
                Dialect::Cassandra,
                req(
                    "x1",
                    Some(&[("class", "NetworkTopologyStrategy"), ("nodc", "1")]),
                    false
                )
            )
            .1,
            "Unrecognized strategy option {nodc} passed to NetworkTopologyStrategy for keyspace x1"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req("k", Some(&[("class", "FooStrategy")]), false)
            )
            .1,
            "Unable to find replication strategy class 'org.apache.cassandra.locator.FooStrategy'"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req("k", Some(&[("class", "LocalStrategy")]), false)
            ),
            (
                crate::ErrorCode::Invalid,
                "Unable to use given strategy class: LocalStrategy is reserved for internal use."
                    .into()
            )
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req(
                    "k",
                    Some(&[("class", "SimpleStrategy"), ("replication_factor", "abc")]),
                    false
                )
            )
            .1,
            "For input string: \"abc\""
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req(
                    "k",
                    Some(&[("class", "NetworkTopologyStrategy"), ("datacenter1", "-1")]),
                    false
                )
            )
            .1,
            "Replication factor must be non-negative, found -1"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req(
                    "k",
                    Some(&[("class", "NetworkTopologyStrategy"), ("datacenter1", "0")]),
                    false
                )
            )
            .1,
            "Configuration for at least one datacenter must be present"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req(
                    "k",
                    Some(&[("class", "NetworkTopologyStrategy"), ("datacenter1", "3/1")]),
                    false
                )
            )
            .1,
            "Transient replication is not enabled on this node"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req("k", Some(&[("replication_factor", "1")]), false)
            )
            .1,
            "Missing replication strategy class"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                req(
                    "eng_k5",
                    Some(&[
                        ("class", "SimpleStrategy"),
                        ("replication_factor", "1"),
                        ("foo", "bar")
                    ]),
                    false
                )
            )
            .1,
            "Unrecognized strategy option {foo} passed to SimpleStrategy for keyspace eng_k5"
        );
        let (r, _) = run(
            Dialect::Cassandra,
            req(
                "k",
                Some(&[
                    ("class", "NetworkTopologyStrategy"),
                    ("datacenter1", "2"),
                    ("dc2", "0"),
                ]),
                false,
            ),
        );
        assert_eq!(r.unwrap().options.len(), 1);
    }
}
