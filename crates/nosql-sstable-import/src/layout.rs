//! Finding a backup's SSTables and schema in a listing of its location.
//!
//! - **`nodetool snapshot`** (Cassandra and ScyllaDB): `…/<keyspace>/<table>-<id>/snapshots/
//!   <tag>/` with the SSTables, `schema.cql` and `manifest.json` (Cassandra: `files`,
//!   `created_at`; ScyllaDB: `snapshot.created_at`, `sstables[].toc_name`). One tag is used:
//!   `?tag=`, or the newest (automatic `dropped-*`, `pre-drop-*`, `truncated-*` snapshots
//!   are ignored). Copies of data directories without snapshots (`…/<keyspace>/<table>-<id>/`)
//!   are read as they are; incremental `backups/` and secondary index directories are not.
//! - **ScyllaDB Manager** (per its published backup specification): manifests at
//!   `backup/meta/cluster/<cluster>/dc/<dc>/node/<node>/task_<task>_tag_<tag>_manifest.json.gz`
//!   index the SSTables of each node at `backup/sst/cluster/<cluster>/dc/<dc>/node/<node>/
//!   keyspace/<ks>/table/<table>/<version>/`; the schema is `backup/schema/cluster/<cluster>/
//!   task_<task>_tag_<tag>_schema_with_internals.json.gz` (or the legacy `_schema.tar.gz`).
//!   Tags sort chronologically; the newest (or `?tag=`) is used.
//! - **Medusa** (per its design documentation): `<fqdn>/<backup>/meta/{manifest.json,
//!   schema.cql}`, data under `<fqdn>/<backup>/data/<ks>/<table>/` (full) or
//!   `<fqdn>/data/<ks>/<table>/` (differential, the manifest listing the files a backup
//!   uses). The newest backup name (or `?backup=` / `?tag=`) is used.
//!
//! Every node's SSTables of a table are imported together: replicas reconcile.

use crate::sstable::SstFiles;
use crate::tar;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Snapshot,
    DataDirectory,
    ScyllaManager,
    Medusa,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Snapshot => "nodetool snapshot",
            Kind::DataDirectory => "data directory",
            Kind::ScyllaManager => "ScyllaDB Manager backup",
            Kind::Medusa => "Medusa backup",
        }
    }
}

/// The SSTables of one table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableFiles {
    /// `None` when the path doesn't name it (resolved from schema.cql or Scylla.db).
    pub keyspace: Option<String>,
    pub table: Option<String>,
    /// The table id from the directory name (hex, no dashes).
    pub id: Option<String>,
    pub sstables: Vec<SstFiles>,
    /// `schema.cql` files next to the SSTables.
    pub schema_keys: Vec<String>,
    /// Distinct directories (nodes) the SSTables came from.
    pub sources: usize,
}

/// Schema text of a backup, to run in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaFile {
    /// CQL statements (`schema.cql`, Medusa's full schema).
    Cql(String),
    /// ScyllaDB Manager's `DESCRIBE SCHEMA WITH INTERNALS` rows (gzipped JSON).
    ManagerJson(String),
    /// ScyllaDB Manager's legacy per-keyspace CQL archive (`.tar.gz`).
    ManagerTar(String),
}

#[derive(Clone, Debug)]
pub struct Backup {
    pub kind: Kind,
    pub tag: Option<String>,
    pub tables: Vec<TableFiles>,
    /// Backup-wide schema files (Manager, Medusa); tables' own `schema.cql` are in
    /// [`TableFiles::schema_keys`].
    pub schema: Vec<SchemaFile>,
    pub warnings: Vec<String>,
}

/// Automatic snapshots both products take on their own.
fn automatic_tag(tag: &str) -> bool {
    ["dropped-", "pre-drop-", "truncated-", "pre-scrub-", "upgrade-", "pre-upgrade-"].iter().any(|p| tag.starts_with(p))
}

fn is_table_dir(name: &str) -> Option<(&str, &str)> {
    let (t, id) = name.rsplit_once('-')?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && !t.is_empty()).then_some((t, id))
}

fn is_sstable_file(name: &str) -> bool {
    // Pre-3.0 names too, so grouping refuses them instead of the import skipping their data.
    crate::descriptor::ComponentName::parse(name).is_some() || crate::descriptor::ComponentName::legacy(name)
}

/// What a listing holds, before reading any file.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub manager_manifests: Vec<ManagerManifest>,
    pub medusa: Vec<MedusaBackup>,
    /// Snapshot tag → its manifest.json keys (one per table directory and node; none for
    /// copies without manifests).
    pub snapshot_tags: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagerManifest {
    /// Path before `backup/`.
    pub root: String,
    pub cluster: String,
    pub dc: String,
    pub node: String,
    pub task: String,
    pub tag: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MedusaBackup {
    pub root: String,
    pub fqdn: String,
    pub name: String,
    pub manifest: String,
}

/// Classify a listing.
pub fn plan(files: &[(String, u64)]) -> Plan {
    let mut p = Plan::default();
    for (key, _) in files {
        let parts: Vec<&str> = key.split('/').collect();
        let n = parts.len();
        // backup/meta/cluster/<c>/dc/<dc>/node/<n>/task_<t>_tag_<tag>_manifest.json.gz
        if n >= 9 && parts[n - 9] == "backup" && parts[n - 8] == "meta" && parts[n - 7] == "cluster" && parts[n - 5] == "dc" && parts[n - 3] == "node" {
            if let Some(rest) = parts[n - 1].strip_prefix("task_").and_then(|r| r.strip_suffix("_manifest.json.gz"))
                && let Some((task, tag)) = rest.split_once("_tag_")
            {
                p.manager_manifests.push(ManagerManifest {
                    root: parts[..n - 9].iter().map(|s| format!("{s}/")).collect(),
                    cluster: parts[n - 6].into(),
                    dc: parts[n - 4].into(),
                    node: parts[n - 2].into(),
                    task: task.into(),
                    tag: tag.into(),
                    key: key.clone(),
                });
            }
            continue;
        }
        // <fqdn>/<backup>/meta/manifest.json
        if n >= 4 && parts[n - 1] == "manifest.json" && parts[n - 2] == "meta" {
            p.medusa.push(MedusaBackup { root: parts[..n - 4].iter().map(|s| format!("{s}/")).collect(), fqdn: parts[n - 4].into(), name: parts[n - 3].into(), manifest: key.clone() });
            continue;
        }
        // …/<table>-<id>/snapshots/<tag>/<file> (manifest.json, when there is one, tells when
        // the snapshot was taken and what it holds)
        if n >= 4 && parts[n - 3] == "snapshots" && is_table_dir(parts[n - 4]).is_some() {
            let manifests = p.snapshot_tags.entry(parts[n - 2].to_string()).or_default();
            if parts[n - 1] == "manifest.json" {
                manifests.push(key.clone());
            }
        }
    }
    p
}

/// Choose a ScyllaDB Manager snapshot: `tag` or the newest; `cluster` when there are several.
pub fn choose_manager<'a>(ms: &'a [ManagerManifest], tag: Option<&str>, cluster: Option<&str>) -> Result<Vec<&'a ManagerManifest>> {
    let clusters: BTreeSet<&str> = ms.iter().map(|m| m.cluster.as_str()).collect();
    let cluster = match cluster {
        Some(c) => clusters.get(c).copied().ok_or_else(|| anyhow::anyhow!("the backup has no cluster {c} (it has {})", clusters.iter().copied().collect::<Vec<_>>().join(", ")))?,
        None if clusters.len() > 1 => bail!("the backup holds several clusters ({}); choose one with ?cluster=", clusters.iter().copied().collect::<Vec<_>>().join(", ")),
        None => clusters.first().copied().unwrap_or_default(),
    };
    let of: Vec<&ManagerManifest> = ms.iter().filter(|m| m.cluster == cluster).collect();
    let tags: BTreeSet<&str> = of.iter().map(|m| m.tag.as_str()).collect();
    let tag = match tag {
        Some(t) => tags.get(t).copied().ok_or_else(|| anyhow::anyhow!("the backup has no snapshot tag {t} (it has {})", tags.iter().copied().collect::<Vec<_>>().join(", ")))?,
        None => tags.last().copied().ok_or_else(|| anyhow::anyhow!("the backup has no manifests"))?,
    };
    Ok(of.into_iter().filter(|m| m.tag == tag).collect())
}

/// One node's ScyllaDB Manager manifest (the fields import uses).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ManagerIndex {
    #[serde(default)]
    pub index: Vec<ManagerTable>,
    #[serde(default)]
    pub schema: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ManagerTable {
    pub keyspace: String,
    pub table: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub files: Vec<String>,
}

/// The tables of a ScyllaDB Manager snapshot, given each chosen node manifest's contents.
pub fn manager_tables(chosen: &[(&ManagerManifest, ManagerIndex)], listing: &HashMap<&str, u64>) -> Result<Vec<TableFiles>> {
    let mut by: BTreeMap<(String, String), TableFiles> = BTreeMap::new();
    for (m, idx) in chosen {
        for t in &idx.index {
            let dir = format!("{}backup/sst/cluster/{}/dc/{}/node/{}/keyspace/{}/table/{}/{}/", m.root, m.cluster, m.dc, m.node, t.keyspace, t.table, t.version);
            let mut files = Vec::new();
            for f in &t.files {
                let key = format!("{dir}{f}");
                let Some(size) = listing.get(key.as_str()) else { bail!("the backup is incomplete: {key} (listed in {}) is missing", m.key) };
                files.push((key, *size));
            }
            let e = by.entry((t.keyspace.clone(), t.table.clone())).or_insert_with(|| TableFiles { keyspace: Some(t.keyspace.clone()), table: Some(t.table.clone()), id: None, sstables: vec![], schema_keys: vec![], sources: 0 });
            e.sstables.extend(SstFiles::group(&files)?);
            e.sources += 1;
        }
    }
    Ok(by.into_values().collect())
}

/// The schema files of a ScyllaDB Manager snapshot.
pub fn manager_schema(chosen: &[&ManagerManifest], listing: &HashMap<&str, u64>) -> Vec<SchemaFile> {
    let Some(m) = chosen.first() else { return vec![] };
    let base = format!("{}backup/schema/cluster/{}/task_{}_tag_{}_schema", m.root, m.cluster, m.task, m.tag);
    let json = format!("{base}_with_internals.json.gz");
    let tar = format!("{base}.tar.gz");
    if listing.contains_key(json.as_str()) {
        vec![SchemaFile::ManagerJson(json)]
    } else if listing.contains_key(tar.as_str()) {
        vec![SchemaFile::ManagerTar(tar)]
    } else {
        vec![]
    }
}

/// CQL statements from ScyllaDB Manager's schema JSON: an array of rows, each with a
/// keyspace, a type, a name and the create statement (the field holding a `CREATE` statement
/// is taken, whatever its name).
pub fn manager_json_schema(raw: &[u8]) -> Result<Vec<String>> {
    let json = tar::gunzip(raw, 256 << 20)?;
    let v: serde_json::Value = serde_json::from_slice(&json).context("ScyllaDB Manager schema")?;
    let rows = match &v {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o.values().find_map(|x| x.as_array().cloned()).unwrap_or_default(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for r in rows {
        let stmt = match &r {
            serde_json::Value::Object(o) => ["cql_stmt", "create_statement", "statement", "cql"].iter().find_map(|k| o.get(*k).and_then(|s| s.as_str()).map(str::to_string)).or_else(|| o.values().filter_map(|s| s.as_str()).find(|s| s.trim_start().to_ascii_uppercase().starts_with("CREATE")).map(str::to_string)),
            serde_json::Value::String(s) => Some(s.rsplit('|').next().unwrap_or(s).to_string()),
            _ => None,
        };
        if let Some(s) = stmt {
            out.push(s);
        }
    }
    Ok(out)
}

/// CQL files of ScyllaDB Manager's legacy schema archive.
pub fn manager_tar_schema(raw: &[u8]) -> Result<Vec<String>> {
    let t = tar::gunzip(raw, 256 << 20)?;
    Ok(tar::entries(&t)?.into_iter().filter(|(n, _)| n.ends_with(".cql")).map(|(_, b)| String::from_utf8_lossy(&b).into_owned()).collect())
}

/// Choose a Medusa backup: `name` or the newest.
pub fn choose_medusa<'a>(bs: &'a [MedusaBackup], name: Option<&str>) -> Result<Vec<&'a MedusaBackup>> {
    let names: BTreeSet<&str> = bs.iter().map(|b| b.name.as_str()).collect();
    let name = match name {
        Some(n) => names.get(n).copied().ok_or_else(|| anyhow::anyhow!("the backup has no Medusa backup {n} (it has {})", names.iter().copied().collect::<Vec<_>>().join(", ")))?,
        None => names.last().copied().ok_or_else(|| anyhow::anyhow!("no Medusa backups"))?,
    };
    Ok(bs.iter().filter(|b| b.name == name).collect())
}

/// The tables of Medusa backups, given each node's `manifest.json` (an array of
/// `{keyspace, columnfamily, objects: [{path, size, MD5}]}`).
pub fn medusa_tables(chosen: &[(&MedusaBackup, serde_json::Value)], listing: &HashMap<&str, u64>) -> Result<Vec<TableFiles>> {
    let mut by: BTreeMap<(String, String), TableFiles> = BTreeMap::new();
    for (b, manifest) in chosen {
        let Some(entries) = manifest.as_array() else { bail!("{}: not a Medusa manifest", b.manifest) };
        for e in entries {
            let ks = e.get("keyspace").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let cf = e.get("columnfamily").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if ks.is_empty() || cf.is_empty() || ks.starts_with("system") {
                continue;
            }
            let (table, id) = match is_table_dir(&cf) {
                Some((t, id)) => (t.to_string(), Some(id.to_string())),
                None => (cf.clone(), None),
            };
            if table.starts_with('.') {
                continue; // secondary index
            }
            let mut files = Vec::new();
            for o in e.get("objects").and_then(|v| v.as_array()).into_iter().flatten() {
                let Some(path) = o.get("path").and_then(|v| v.as_str()) else { continue };
                let key = resolve(path, &b.root, listing).ok_or_else(|| anyhow::anyhow!("the backup is incomplete: {path} (listed in {}) is missing", b.manifest))?;
                files.push((key.to_string(), listing[key]));
            }
            let t = by.entry((ks.clone(), table.clone())).or_insert_with(|| TableFiles { keyspace: Some(ks.clone()), table: Some(table.clone()), id: id.clone(), sstables: vec![], schema_keys: vec![], sources: 0 });
            t.sstables.extend(SstFiles::group(&files)?);
            t.sources += 1;
        }
    }
    Ok(by.into_values().collect())
}

/// A manifest path as a listing key: as is, under the backup root, or with leading segments
/// (a bucket or prefix the manifest names) removed.
fn resolve<'a>(path: &str, root: &str, listing: &HashMap<&'a str, u64>) -> Option<&'a str> {
    let path = path.trim_start_matches('/');
    let mut candidates = vec![path.to_string(), format!("{root}{path}")];
    let mut rest = path;
    while let Some((_, r)) = rest.split_once('/') {
        candidates.push(r.to_string());
        candidates.push(format!("{root}{r}"));
        rest = r;
    }
    candidates.iter().find_map(|c| listing.get_key_value(c.as_str()).map(|(k, _)| *k))
}

/// Snapshot tags to consider (automatic ones excluded unless asked for), newest last given
/// each tag's creation time.
pub fn choose_snapshot_tag(plan: &Plan, tag: Option<&str>, created: &HashMap<String, i64>) -> Result<Option<String>> {
    if let Some(t) = tag {
        if !plan.snapshot_tags.contains_key(t) {
            bail!("the backup has no snapshot {t} (it has {})", plan.snapshot_tags.keys().cloned().collect::<Vec<_>>().join(", "));
        }
        return Ok(Some(t.to_string()));
    }
    let mut tags: Vec<&String> = plan.snapshot_tags.keys().filter(|t| !automatic_tag(t)).collect();
    tags.sort_by_key(|t| (created.get(*t).copied().unwrap_or(i64::MIN), (*t).clone()));
    Ok(tags.last().map(|t| (*t).clone()))
}

/// The tables of a snapshot tag, or (without one) of data directory copies.
pub fn snapshot_tables(files: &[(String, u64)], tag: Option<&str>) -> Result<Vec<TableFiles>> {
    // Directory → ((keyspace, table, id) when the path names them, sstable files, schema.cql).
    type Dir = (Option<(String, String, String)>, Vec<(String, u64)>, Vec<String>);
    let mut by: BTreeMap<String, Dir> = BTreeMap::new();
    for (key, size) in files {
        let parts: Vec<&str> = key.split('/').collect();
        let n = parts.len();
        let name = parts[n - 1];
        let dir = &key[..key.len() - name.len()];
        let owner = match tag {
            Some(tag) => {
                // …/<ks>/<table>-<id>/snapshots/<tag>/<file>
                if n < 5 || parts[n - 2] != tag || parts[n - 3] != "snapshots" {
                    continue;
                }
                let Some((t, id)) = is_table_dir(parts[n - 4]) else { continue };
                Some((parts[n - 5].to_string(), t.to_string(), id.to_string()))
            }
            None => {
                if parts.iter().any(|p| *p == "snapshots" || *p == "backups" || p.starts_with('.')) {
                    continue;
                }
                (n >= 3).then(|| is_table_dir(parts[n - 2])).flatten().map(|(t, id)| (parts[n - 3].to_string(), t.to_string(), id.to_string()))
            }
        };
        let e = by.entry(dir.to_string()).or_insert_with(|| (owner, vec![], vec![]));
        if name == "schema.cql" {
            e.2.push(key.clone());
        } else if is_sstable_file(name) {
            e.1.push((key.clone(), *size));
        }
    }
    let mut tables: BTreeMap<(Option<String>, Option<String>, String), TableFiles> = BTreeMap::new();
    for (dir, (owner, files, schema)) in by {
        if files.is_empty() {
            continue;
        }
        let (ks, t, id) = match owner {
            Some((ks, t, id)) if !ks.starts_with("system") => (Some(ks), Some(t), Some(id)),
            Some(_) => continue,
            None => (None, None, Some(dir.clone())),
        };
        let e = tables.entry((ks.clone(), t.clone(), if ks.is_some() { String::new() } else { id.clone().unwrap_or_default() })).or_insert_with(|| TableFiles {
            keyspace: ks,
            table: t,
            id: id.filter(|i| i.len() == 32),
            sstables: vec![],
            schema_keys: vec![],
            sources: 0,
        });
        e.sstables.extend(SstFiles::group(&files)?);
        e.schema_keys.extend(schema);
        e.sources += 1;
    }
    Ok(tables.into_values().collect())
}

/// When a snapshot was taken, from its `manifest.json` (Cassandra: `created_at` as an ISO
/// time; ScyllaDB: `snapshot.created_at` in seconds). Seconds since the epoch.
pub fn snapshot_created(manifest: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(manifest).ok()?;
    if let Some(s) = v.pointer("/snapshot/created_at").and_then(|x| x.as_i64()) {
        return Some(s);
    }
    let iso = v.get("created_at")?.as_str()?;
    parse_iso8601(iso)
}

/// `2026-10-21T04:32:56.999Z` → seconds (UTC).
fn parse_iso8601(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let t = time.trim_end_matches('Z');
    let mut hms = t.split(':');
    let (h, mi) = (hms.next()?.parse::<i64>().ok()?, hms.next()?.parse::<i64>().ok()?);
    let sec = hms.next()?.split('.').next()?.parse::<i64>().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// Files a snapshot's `manifest.json` lists that the listing lacks (incomplete copies).
pub fn snapshot_missing(manifest: &[u8], dir: &str, listing: &HashMap<&str, u64>) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest) else { return vec![] };
    let mut names: Vec<String> = v.get("files").and_then(|f| f.as_array()).into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
    names.extend(v.get("sstables").and_then(|f| f.as_array()).into_iter().flatten().filter_map(|s| s.get("toc_name").and_then(|t| t.as_str()).map(str::to_string)));
    names.into_iter().map(|n| format!("{dir}{n}")).filter(|k| !listing.contains_key(k.as_str())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(keys: &[&str]) -> Vec<(String, u64)> {
        keys.iter().map(|k| (k.to_string(), 10)).collect()
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn snapshots_and_tags() {
        let files = l(&[
            &format!("n1/data/ks/t-{ID}/snapshots/nightly/nb-1-big-Data.db"),
            &format!("n1/data/ks/t-{ID}/snapshots/nightly/nb-1-big-Statistics.db"),
            &format!("n1/data/ks/t-{ID}/snapshots/nightly/schema.cql"),
            &format!("n1/data/ks/t-{ID}/snapshots/nightly/manifest.json"),
            &format!("n2/data/ks/t-{ID}/snapshots/nightly/nb-7-big-Data.db"),
            &format!("n2/data/ks/t-{ID}/snapshots/nightly/manifest.json"),
            &format!("n1/data/ks/t-{ID}/snapshots/dropped-123-t/nb-1-big-Data.db"),
            &format!("n1/data/ks/t-{ID}/snapshots/dropped-123-t/manifest.json"),
            &format!("n1/data/ks/t-{ID}/snapshots/older/manifest.json"),
            &format!("n1/data/ks/t-{ID}/nb-9-big-Data.db"),
            &format!("n1/data/system/local-{ID}/nb-1-big-Data.db"),
        ]);
        let p = plan(&files);
        assert_eq!(p.snapshot_tags.len(), 3);
        let created: HashMap<String, i64> = [("nightly".to_string(), 200), ("older".to_string(), 100), ("dropped-123-t".to_string(), 300)].into();
        assert_eq!(choose_snapshot_tag(&p, None, &created).unwrap().as_deref(), Some("nightly"));
        assert!(choose_snapshot_tag(&p, Some("nope"), &created).is_err());
        let t = snapshot_tables(&files, Some("nightly")).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].keyspace.as_deref(), t[0].table.as_deref(), t[0].sstables.len(), t[0].sources, t[0].schema_keys.len()), (Some("ks"), Some("t"), 2, 2, 1));
        // Without snapshots: live data directories, system keyspaces skipped.
        let t = snapshot_tables(&files, None).unwrap();
        assert_eq!((t.len(), t[0].sstables[0].prefix.as_str()), (1, "nb-9-big"));
        // A bare directory of sstables.
        let t = snapshot_tables(&l(&["x/mt-3h4u_0cnc_5xh6o2d8zy2c2mzkrn-big-Data.db"]), None).unwrap();
        assert_eq!((t.len(), t[0].keyspace.clone()), (1, None));
    }

    #[test]
    fn manager_layout() {
        let m = |tag: &str, node: &str| format!("pfx/backup/meta/cluster/c1/dc/dc1/node/{node}/task_t1_tag_{tag}_manifest.json.gz");
        let files = l(&[
            &m("sm_20260101000000UTC", "n1"),
            &m("sm_20260102000000UTC", "n1"),
            &m("sm_20260102000000UTC", "n2"),
            "pfx/backup/sst/cluster/c1/dc/dc1/node/n1/keyspace/ks/table/t/v1/me-1-big-Data.db",
            "pfx/backup/sst/cluster/c1/dc/dc1/node/n2/keyspace/ks/table/t/v1/me-1-big-Data.db",
            "pfx/backup/schema/cluster/c1/task_t1_tag_sm_20260102000000UTC_schema_with_internals.json.gz",
        ]);
        let p = plan(&files);
        assert_eq!(p.manager_manifests.len(), 3);
        let chosen = choose_manager(&p.manager_manifests, None, None).unwrap();
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].tag, "sm_20260102000000UTC");
        let listing: HashMap<&str, u64> = files.iter().map(|(k, s)| (k.as_str(), *s)).collect();
        let idx = |node: &str| ManagerIndex { index: vec![ManagerTable { keyspace: "ks".into(), table: "t".into(), version: "v1".into(), files: vec!["me-1-big-Data.db".into()] }], schema: None }.clone_for(node);
        let with: Vec<(&ManagerManifest, ManagerIndex)> = chosen.iter().map(|m| (*m, idx(&m.node))).collect();
        let t = manager_tables(&with, &listing).unwrap();
        assert_eq!((t.len(), t[0].sstables.len(), t[0].sources), (1, 2, 2));
        assert_eq!(manager_schema(&chosen, &listing).len(), 1);
        let mut bad = with.clone();
        bad[0].1.index[0].files.push("me-2-big-Data.db".into());
        assert!(manager_tables(&bad, &listing).unwrap_err().to_string().contains("incomplete"));
    }

    impl ManagerIndex {
        fn clone_for(&self, _node: &str) -> ManagerIndex {
            self.clone()
        }
    }

    #[test]
    fn manager_schema_json() {
        let rows = serde_json::json!([
            {"keyspace": "ks", "type": "keyspace", "name": "ks", "cql_stmt": "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': 3};"},
            {"keyspace": "ks", "type": "table", "name": "t", "whatever": "CREATE TABLE ks.t (k int PRIMARY KEY);"},
        ]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, rows.to_string().as_bytes()).unwrap();
        let s = manager_json_schema(&gz.finish().unwrap()).unwrap();
        assert_eq!(s.len(), 2);
        assert!(s[1].starts_with("CREATE TABLE"));
        let t = crate::tar::tests::build(&[("ks.cql", b"CREATE KEYSPACE ks;"), ("README", b"x")]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &t).unwrap();
        assert_eq!(manager_tar_schema(&gz.finish().unwrap()).unwrap(), vec!["CREATE KEYSPACE ks;".to_string()]);
    }

    #[test]
    fn medusa_layout() {
        let files = l(&[
            "m/node1.example/2026102100/meta/manifest.json",
            "m/node1.example/2026102100/meta/schema.cql",
            "m/node1.example/2026102000/meta/manifest.json",
            &format!("m/node1.example/data/ks/t-{ID}/nb-1-big-Data.db"),
            &format!("m/node1.example/data/ks/t-{ID}/nb-2-big-Data.db"),
        ]);
        let p = plan(&files);
        assert_eq!(p.medusa.len(), 2);
        let chosen = choose_medusa(&p.medusa, None).unwrap();
        assert_eq!(chosen[0].name, "2026102100");
        let manifest = serde_json::json!([{"keyspace": "ks", "columnfamily": format!("t-{ID}"), "objects": [{"path": format!("bucket/m/node1.example/data/ks/t-{ID}/nb-2-big-Data.db"), "size": 10, "MD5": "x"}]}]);
        let listing: HashMap<&str, u64> = files.iter().map(|(k, s)| (k.as_str(), *s)).collect();
        let t = medusa_tables(&[(chosen[0], manifest)], &listing).unwrap();
        assert_eq!((t[0].table.as_deref(), t[0].sstables.len(), t[0].sstables[0].prefix.as_str()), (Some("t"), 1, "nb-2-big"));
    }

    #[test]
    fn snapshot_manifests() {
        let c = br#"{"files":["nb-1-big-Data.db","nb-2-big-Data.db"],"created_at":"2026-10-21T04:32:56.999Z","expires_at":null}"#;
        assert_eq!(snapshot_created(c), Some(1792557176));
        let s = br#"{"snapshot":{"name":"x","created_at":1792557193},"sstables":[{"toc_name":"mt-1-big-TOC.txt"}]}"#;
        assert_eq!(snapshot_created(s), Some(1792557193));
        let listing: HashMap<&str, u64> = [("d/nb-1-big-Data.db", 1u64)].into();
        assert_eq!(snapshot_missing(c, "d/", &listing), vec!["d/nb-2-big-Data.db".to_string()]);
        assert_eq!(snapshot_missing(s, "d/", &listing), vec!["d/mt-1-big-TOC.txt".to_string()]);
    }
}
