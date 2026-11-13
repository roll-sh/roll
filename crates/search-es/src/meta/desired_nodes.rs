//! Desired nodes (`/_internal/desired_nodes`): the node topology an orchestrator says the
//! cluster should have, kept in the cluster metadata as Elasticsearch's `DesiredNodesMetadata`.
//!
//! A version is a `history_id` plus a monotonically increasing `version`; replacing the history
//! starts over. Each desired node is its settings (flattened, string values, as
//! `Settings.fromXContent` keeps them), a processor count or range, and memory and storage
//! sizes. Nodes are keyed by their external id (`node.external_id`, defaulting to `node.name`).
//! A desired node is `ACTUALIZED` once a node with its external id has joined the cluster.

use crate::json::{Map, Value};
use crate::meta::settings::{SettingValue, Settings};
use crate::obj;
use crate::units::ByteSize;

/// A processor count, rounded to five decimal places as `Processors` stores it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Processors(pub f64);

impl Processors {
    pub fn new(count: f64) -> Processors {
        if count == 0.0 {
            return Processors(0.0);
        }
        let rounded = format!("{count:.5}").parse::<f64>().unwrap_or(count);
        Processors(rounded.max(1e-5))
    }

    /// `Processors.equalsOrCloseTo`: equal, or equal as floats (an orchestrator sending `8.1`
    /// as a float and then as a double is not a new definition).
    pub fn close_to(a: Option<Processors>, b: Option<Processors>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a.0 == b.0 || (a.0 as f32 - b.0 as f32).abs() <= f32::EPSILON * (a.0 as f32).abs().max(1.0),
            _ => false,
        }
    }
}

/// One desired node.
#[derive(Clone, Debug, PartialEq)]
pub struct DesiredNode {
    pub settings: Settings,
    pub processors: Option<Processors>,
    pub processors_range: Option<(Processors, Option<Processors>)>,
    pub memory: ByteSize,
    pub storage: ByteSize,
    pub external_id: String,
    pub roles: Vec<String>,
    /// `ACTUALIZED` (a node with this external id joined) rather than `PENDING`.
    pub actualized: bool,
}

/// Node roles Elasticsearch knows (`DiscoveryNodeRole.roles()`).
pub const ROLES: &[&str] = &[
    "data", "data_cold", "data_content", "data_frozen", "data_hot", "data_warm", "index", "ingest", "master", "ml", "remote_cluster_client",
    "search", "transform", "voting_only",
];

impl DesiredNode {
    /// `node.roles` of the settings, or every role when unset; `Err` names the first unknown one.
    pub fn roles_of(settings: &Settings) -> Result<Vec<String>, String> {
        let list: Vec<String> = match settings.get_value("node.roles") {
            None => return Ok(ROLES.iter().filter(|r| !matches!(**r, "index" | "search" | "voting_only")).map(|r| r.to_string()).collect()),
            Some(SettingValue::List(l)) => l.clone(),
            Some(SettingValue::Str(s)) => s.split(',').map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect(),
            Some(SettingValue::Null) => Vec::new(),
        };
        for r in &list {
            if !ROLES.contains(&r.as_str()) {
                return Err(format!("unknown role [{r}]"));
            }
        }
        let mut sorted = list;
        sorted.sort();
        sorted.dedup();
        Ok(sorted)
    }

    pub fn has_master_role(&self) -> bool {
        self.roles.iter().any(|r| r == "master")
    }

    /// `equalsWithProcessorsCloseTo`.
    pub fn same_definition(&self, o: &DesiredNode) -> bool {
        self.settings == o.settings
            && self.memory == o.memory
            && self.storage == o.storage
            && self.external_id == o.external_id
            && self.roles == o.roles
            && Processors::close_to(self.processors, o.processors)
            && match (&self.processors_range, &o.processors_range) {
                (None, None) => true,
                (Some((a1, a2)), Some((b1, b2))) => Processors::close_to(Some(*a1), Some(*b1)) && Processors::close_to(*a2, *b2),
                _ => false,
            }
    }

    /// As the API prints it; the cluster state also has the `status` (0 pending, 1 actualized).
    pub fn to_value(&self, with_status: bool) -> Value {
        let mut m = Map::new();
        m.insert("settings".into(), self.settings.to_json(false));
        if let Some(p) = self.processors {
            m.insert("processors".into(), Value::from(p.0));
        }
        if let Some((min, max)) = self.processors_range {
            let mut r = Map::new();
            r.insert("min".into(), Value::from(min.0));
            if let Some(max) = max {
                r.insert("max".into(), Value::from(max.0));
            }
            m.insert("processors_range".into(), Value::Obj(r));
        }
        m.insert("memory".into(), Value::from(self.memory.human()));
        m.insert("storage".into(), Value::from(self.storage.human()));
        if with_status {
            m.insert("status".into(), Value::from(if self.actualized { 1 } else { 0 }));
        }
        Value::Obj(m)
    }

    fn from_stored(v: &Value) -> Option<DesiredNode> {
        let settings = Settings::from_json(v.get("settings")?).ok()?;
        let p = |v: Option<&Value>| v.and_then(Value::as_f64).map(Processors);
        let range = v.get("processors_range").map(|r| (p(r.get("min")).unwrap_or(Processors(1.0)), p(r.get("max"))));
        let size = |k: &str| v.get(k).and_then(Value::as_str).and_then(|s| ByteSize::parse(s, k).ok());
        Some(DesiredNode {
            external_id: external_id(&settings),
            roles: DesiredNode::roles_of(&settings).unwrap_or_default(),
            processors: p(v.get("processors")),
            processors_range: range,
            memory: size("memory")?,
            storage: size("storage")?,
            actualized: v.get("status").and_then(Value::as_i64) == Some(1),
            settings,
        })
    }
}

/// `node.external_id`, defaulting to `node.name`.
pub fn external_id(settings: &Settings) -> String {
    settings.get("node.external_id").or_else(|| settings.get("node.name")).unwrap_or_default().to_string()
}

/// The latest desired nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct DesiredNodes {
    pub history_id: String,
    pub version: i64,
    /// Sorted by external id (Elasticsearch keys them in a map).
    pub nodes: Vec<DesiredNode>,
}

impl DesiredNodes {
    /// `GET _internal/desired_nodes/_latest`, or the cluster state's `latest` with statuses.
    pub fn to_value(&self, with_status: bool) -> Value {
        obj! {
            "history_id" => self.history_id.clone(),
            "version" => self.version,
            "nodes" => Value::Arr(self.nodes.iter().map(|n| n.to_value(with_status)).collect()),
        }
    }

    pub fn from_stored(v: &Value) -> Option<DesiredNodes> {
        Some(DesiredNodes {
            history_id: v.get("history_id")?.as_str()?.to_string(),
            version: v.get("version")?.as_i64()?,
            nodes: v.get("nodes")?.as_array()?.iter().filter_map(DesiredNode::from_stored).collect(),
        })
    }

    /// `equalsWithProcessorsCloseTo`.
    pub fn same_definition(&self, o: &DesiredNodes) -> bool {
        self.version == o.version
            && self.history_id == o.history_id
            && self.nodes.len() == o.nodes.len()
            && self.nodes.iter().all(|n| o.nodes.iter().find(|m| m.external_id == n.external_id).is_some_and(|m| n.same_definition(m)))
    }

    /// Marks the desired nodes whose external id is a member of the cluster.
    pub fn actualize(&mut self, members: &[&str]) {
        for n in &mut self.nodes {
            if members.contains(&n.external_id.as_str()) {
                n.actualized = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processors_round_like_elasticsearch() {
        assert_eq!(Processors::new(8.123456789).0, 8.12346);
        assert_eq!(Processors::new(8.0).0, 8.0);
        assert_eq!(Processors::new(0.000001).0, 1e-5);
        assert!(Processors::close_to(Some(Processors(8.1)), Some(Processors(8.1f32 as f64))));
        assert!(!Processors::close_to(Some(Processors(8.0)), Some(Processors(64.0))));
    }

    #[test]
    fn roles() {
        let mut s = Settings::default();
        assert!(DesiredNode::roles_of(&s).unwrap().contains(&"master".to_string()));
        s.set("node.roles", "data_hot");
        assert_eq!(DesiredNode::roles_of(&s).unwrap(), vec!["data_hot".to_string()]);
        s.set("node.roles", "unknown,other");
        assert_eq!(DesiredNode::roles_of(&s).unwrap_err(), "unknown role [unknown]");
    }
}
