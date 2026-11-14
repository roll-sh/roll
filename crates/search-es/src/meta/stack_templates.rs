//! The templates and ingest pipelines Elasticsearch installs on every cluster (x-pack's
//! `StackTemplateRegistry`): the `logs`, `metrics` and `synthetics` index templates for
//! `logs-*-*`, `metrics-*-*` and `synthetics-*-*` data streams, the component templates they
//! compose (`logs@mappings`, `ecs@mappings`, `logs@settings`, ...) with their deprecated
//! pre-8.12 names, `.kibana-reporting`, and the `logs@default-pipeline` /
//! `logs@json-pipeline` pipelines.
//!
//! Elasticsearch's registry re-adds a missing template on every cluster state change, so a
//! deleted built-in comes back. roll installs whatever is missing when a cluster opens, which
//! is the same as far as clients can tell (the test frameworks never delete them). The
//! definitions are what `GET /_component_template` etc. return on Elasticsearch 9.5.5, minus
//! the creation dates, in `stack_templates.json`. `stack.templates.enabled: false` (node
//! setting) turns the registry off, as in Elasticsearch.
//!
//! x-pack's otel-data plugin (`OTelIndexTemplateRegistry`) installs its templates the same way:
//! `logs-otel@template`, `metrics-otel@template`, `traces-otel@template` and the aggregated APM
//! metrics templates (`metrics-transaction.1m.otel@template`, ...), composed of `otel@mappings`,
//! `semconv-resource-to-ecs@mappings` and friends, in `otel_templates.json` (also what
//! Elasticsearch 9.5.5 returns). They compose the stack's components, so they're installed only
//! with them; `xpack.otel_data.registry.enabled: false` (node setting) leaves them out.

use std::sync::{Arc, OnceLock};

use super::Metadata;
use super::templates::{ComponentTemplate, ComposableTemplate};
use crate::json::{Map, Value};

/// The parsed definitions: component templates, index templates, pipelines.
struct Defs {
    components: Vec<(String, Arc<ComponentTemplate>)>,
    composables: Vec<(String, Arc<ComposableTemplate>)>,
    pipelines: Vec<(String, Map)>,
}

fn stack() -> &'static Defs {
    static D: OnceLock<Defs> = OnceLock::new();
    D.get_or_init(|| Defs::parse(include_bytes!("stack_templates.json")))
}

fn otel() -> &'static Defs {
    static D: OnceLock<Defs> = OnceLock::new();
    D.get_or_init(|| Defs::parse(include_bytes!("otel_templates.json")))
}

impl Defs {
    fn parse(bytes: &[u8]) -> Defs {
        let root = match crate::json::parse(bytes) {
            Ok(Some(Value::Obj(m))) => m,
            _ => Map::new(),
        };
        let section = |k: &str| root.get(k).and_then(Value::as_object).cloned().unwrap_or_default();
        // Dates are stamped at install time; parse with 0 and overwrite then.
        let components = section("component_templates")
            .into_iter()
            .filter_map(|(k, v)| ComponentTemplate::parse(&v, None, 0).ok().map(|t| (k, Arc::new(t))))
            .collect();
        let composables = section("index_templates")
            .into_iter()
            .filter_map(|(k, v)| ComposableTemplate::parse(&v, None, 0).ok().map(|t| (k, Arc::new(t))))
            .collect();
        let pipelines = section("pipelines").into_iter().filter_map(|(k, v)| v.as_object().cloned().map(|m| (k, m))).collect();
        Defs { components, composables, pipelines }
    }
}

/// Which registries a node runs, from its settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registries {
    pub stack: bool,
    pub otel: bool,
}

impl Registries {
    pub fn of(config: &crate::cluster::ClusterConfig) -> Registries {
        let on = |k: &str| config.node_setting(k) != Some("false");
        let stack = on("stack.templates.enabled");
        Registries { stack, otel: stack && on("xpack.otel_data.registry.enabled") }
    }
}

/// Installs every built-in of the registries `on` that `md` lacks, stamped with `now` (epoch
/// millis). Returns whether anything was added.
pub fn install_missing(md: &mut Metadata, on: Registries, now: i64) -> bool {
    let mut changed = false;
    if on.stack {
        changed |= install(stack(), md, now);
    }
    if on.otel {
        changed |= install(otel(), md, now);
    }
    changed
}

fn install(d: &Defs, md: &mut Metadata, now: i64) -> bool {
    let mut changed = false;
    for (name, t) in &d.components {
        if !md.component_templates.contains_key(name) {
            let mut t = ComponentTemplate::clone(t);
            t.created_date = now;
            t.modified_date = now;
            md.component_templates.insert(name.clone(), Arc::new(t));
            changed = true;
        }
    }
    for (name, t) in &d.composables {
        if !md.index_templates.contains_key(name) {
            let mut t = ComposableTemplate::clone(t);
            t.created_date = now;
            t.modified_date = now;
            md.index_templates.insert(name.clone(), Arc::new(t));
            changed = true;
        }
    }
    for (name, p) in &d.pipelines {
        if !md.pipelines.contains_key(name) {
            let mut p = p.clone();
            p.insert("created_date_millis".into(), now.into());
            p.insert("modified_date_millis".into(), now.into());
            md.pipelines.insert(name.clone(), Value::Obj(p));
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Registries = Registries { stack: true, otel: true };

    #[test]
    fn every_definition_parses() {
        let d = stack();
        assert_eq!(d.components.len(), 20);
        assert_eq!(d.composables.len(), 4);
        assert_eq!(d.pipelines.len(), 4);
        let logs = &d.composables.iter().find(|(n, _)| n == "logs").unwrap().1;
        assert_eq!(logs.index_patterns, ["logs-*-*"]);
        assert_eq!(logs.priority, Some(100));
        assert_eq!(logs.composed_of, ["logs@mappings", "logs@settings", "logs@custom", "ecs@mappings"]);
        let o = otel();
        assert_eq!((o.components.len(), o.composables.len()), (7, 15));
        // Every component an otel template needs (but its `@custom` ones) is built in.
        for (name, t) in &o.composables {
            for c in t.composed_of.iter().filter(|c| !t.ignore_missing_component_templates.as_ref().is_some_and(|i| i.contains(c))) {
                assert!(d.components.iter().chain(&o.components).any(|(n, _)| n == c), "{name}: {c}");
            }
        }
    }

    #[test]
    fn installs_only_what_is_missing() {
        let mut md = Metadata::default();
        assert!(install_missing(&mut md, ALL, 7));
        assert!(md.index_templates.contains_key("metrics"));
        assert!(md.component_templates.contains_key("ecs@mappings"));
        assert_eq!(md.pipelines["logs@default-pipeline"].get("created_date_millis").and_then(Value::as_i64), Some(7));
        md.index_templates.remove("logs");
        assert!(install_missing(&mut md, ALL, 8));
        assert_eq!(md.index_templates["logs"].created_date, 8);
        assert!(!install_missing(&mut md, ALL, 9));
        assert!(md.index_templates.contains_key("logs-otel@template"));
        let mut md = Metadata::default();
        install_missing(&mut md, Registries { stack: true, otel: false }, 7);
        assert!(md.index_templates.contains_key("logs") && !md.index_templates.contains_key("logs-otel@template"));
    }
}
