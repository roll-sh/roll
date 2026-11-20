//! `roll.toml` `[app]` on a service's settings. `apps_build::app` parses the file and checks
//! its shape (where `roll apps plan` and `roll up` read it too, before a push); here what it sets
//! goes on top of the service's settings, checked as the API checks them, at each build.
//!
//! A preview environment scales to zero unless `[app.previews.run]` sets `min_instances`: an
//! open pull request shouldn't keep an instance up because production does (Render's
//! `previews.numInstances`).
//!
//! What a push can't change is what it shouldn't: the volume, domains, access and variables
//! stay the service's settings.

pub use apps_build::app::{AppConfig, EnvScope};
use crate::model::{Build, Builder, Run};
use crate::{Error, Result};
use std::path::Path;

/// What a preview environment runs with when nothing says otherwise: a web service scales to
/// zero. A note when that changed something.
pub fn preview_defaults(mut run: Run, file: Option<&AppConfig>) -> (Run, Option<String>) {
    let set_by_file = file.and_then(|f| f.previews.as_ref()).and_then(|p| p.run.as_ref()).is_some_and(|r| r.contains_key("min_instances"));
    if set_by_file || run.kind != crate::model::RunType::Web || run.min_instances == 0 {
        return (run, None);
    }
    run.min_instances = 0;
    (run, Some("a preview environment scales to zero (roll.toml [app.previews.run] min_instances keeps instances up)".into()))
}

/// `[app]` of `<root>/roll.toml`, if there's one ([`apps_build::app::read`]).
pub fn read(root: &Path) -> Result<Option<AppConfig>> {
    apps_build::app::read(root).map_err(|e| match e {
        apps_build::app::ReadError::Invalid(m) => Error::BadRequest(m),
        apps_build::app::ReadError::Io(m) => Error::Internal(m),
    })
}

pub fn parse(text: &str) -> Result<Option<AppConfig>> {
    apps_build::app::parse(text).map_err(Error::BadRequest)
}

/// The file on a service's settings.
pub trait Apply {
    /// The service's settings with this file's on top (then the environment's block, when
    /// `env` is given and has one), checked as the API checks them, and a line for each value
    /// the file set.
    fn apply(&self, build: &Build, run: &Run, env: Option<EnvScope<'_>>) -> Result<(Build, Run, Vec<String>)>;
}

impl Apply for AppConfig {
    fn apply(&self, build: &Build, run: &Run, env: Option<EnvScope<'_>>) -> Result<(Build, Run, Vec<String>)> {
        let (mut b, mut r, mut said) = (build.clone(), run.clone(), Vec::new());
        for (c, scope) in self.layers(env) {
            (b, r) = apply_one(c, &b, &r)?;
            said.extend(c.lines(&scope));
        }
        Ok((b, r, said))
    }
}

fn apply_one(c: &AppConfig, build: &Build, run: &Run) -> Result<(Build, Run)> {
    let (mut b, mut r) = (build.clone(), run.clone());
    if let Some(v) = c.builder {
        b.builder = match v {
            apps_build::Builder::Auto => Builder::Auto,
            apps_build::Builder::Dockerfile => Builder::Dockerfile,
            apps_build::Builder::Functions => Builder::Functions,
        };
    }
    for (from, to) in [(&c.dockerfile, &mut b.dockerfile), (&c.install, &mut b.install), (&c.build, &mut b.build), (&c.start, &mut b.start), (&c.pre_deploy, &mut r.pre_deploy), (&c.health_path, &mut r.health_path)] {
        if from.is_some() {
            to.clone_from(from);
        }
    }
    if c.watch.is_some() {
        b.watch.clone_from(&c.watch);
    }
    if c.spa.is_some() {
        b.spa = c.spa;
    }
    if c.packages.is_some() {
        b.packages.clone_from(&c.packages);
    }
    if let Some(t) = &c.run {
        let patch = serde_json::to_value(t).map_err(|e| Error::BadRequest(format!("roll.toml: {e}")))?;
        r = r.merged(Some(&patch)).map_err(|e| Error::BadRequest(format!("roll.toml [app.run]: {e}")))?;
    }
    b.check()?;
    let r = r.normalized()?;
    Ok((b, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Alerts, Restart, Rollout, RunType, Scale, Volume};

    #[test]
    fn the_files_run_keys_are_the_services_run_settings() {
        // Every field set, so every key serializes: a new field doesn't build until it's named here.
        let all = Run {
            kind: RunType::Cron,
            port: Some(1),
            start: Some("a".into()),
            pre_deploy: Some("a".into()),
            schedule: Some("* * * * *".into()),
            timezone: Some("UTC".into()),
            timeout_s: Some(1),
            retries: Some(1),
            min_instances: 1,
            max_instances: 1,
            concurrency: 1,
            cpu: 1.0,
            memory_mb: 1,
            health_path: Some("/".into()),
            health_timeout_s: 1,
            health_every_s: Some(2),
            health_failures: Some(1),
            health_command: Some("a".into()),
            suspend_after_s: 1,
            sleep_after_s: 1,
            volume: Some(Volume { mount: "/data".into(), size_gb: 1, backups: None }),
            alerts: Alerts::default(),
            rollout: Rollout::default(),
            h2c: true,
            scale: Some(Scale { stream: "s".into(), group: "g".into(), metric: String::new(), sum: false, per_instance: 1 }),
            public: false,
            drain_s: Some(1),
            cpu_target: Some(1),
            memory_target: Some(1),
            restart: Restart::Never,
            restart_tries: Some(1),
            affinity: true,
            tcp: true,
            rollback_on_crash: true,
            rebuild_on_base: true,
            skew_protection: true,
            queue_s: Some(5),
            latency_target_ms: Some(300),
            schedules: vec![crate::model::Scheduled { name: "a".into(), schedule: "@daily".into(), command: "x".into(), path: None, timezone: None, timeout_s: None }],
            push: vec![crate::model::Push { stream: "st_aaaaaaaaaaaa".into(), topic: "t".into(), group: "g".into(), path: "/e".into(), environment: None, dead_letter: None, max_attempts: None, timeout_s: None, max_in_flight: None }],
            sidecars: vec![crate::model::Sidecar { name: "a".into(), image: "x".into(), command: None, env: Default::default(), variables: vec![], cpu: 0.25, memory_mb: 128 }],
            block_on: Some(crate::model::Severity::Critical),
            slo: Some(crate::model::Slo { objective: 99.9, latency_ms: None, window_days: 28 }),
            metrics: Some(crate::model::Scrape { port: Some(9091), path: "/metrics".into() }),
            log_metrics: vec![crate::model::LogMetric { name: "payments_failed".into(), pattern: "payment failed".into() }],
            schedule_scale: vec![crate::model::ScaleWindow { windows: vec![crate::freeze::Window { from: "mon 08:00".into(), to: "fri 18:00".into() }], timezone: None, min_instances: 2 }],
        };
        let keys: std::collections::BTreeSet<String> = serde_json::to_value(&all).unwrap().as_object().unwrap().keys().filter(|k| *k != "volume" && *k != "push").cloned().collect();
        let file: std::collections::BTreeSet<String> = apps_build::app::RUN_KEYS.iter().map(|k| k.to_string()).collect();
        assert_eq!(keys, file);
        // The editors' schema gives each the type it serializes as.
        let schema: serde_json::Value = serde_json::from_str(include_str!("../../../docs/apps/roll.schema.json")).unwrap();
        for (k, v) in serde_json::to_value(&all).unwrap().as_object().unwrap().iter().filter(|(k, _)| *k != "volume" && *k != "push") {
            let kind = match v {
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(n) if n.is_f64() => "number",
                serde_json::Value::Number(_) => "integer",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Object(_) => "object",
                serde_json::Value::Array(_) => "array",
                _ => "other",
            };
            assert_eq!(schema["definitions"]["run"]["properties"][k]["type"], kind, "{k}");
        }
        let restart = schema["definitions"]["run"]["properties"]["restart"]["enum"].clone();
        for r in [Restart::Always, Restart::OnFailure, Restart::Never] {
            assert!(restart.as_array().unwrap().contains(&serde_json::to_value(r).unwrap()), "{restart}");
        }
    }

    #[test]
    fn app_is_read_beside_roll_codes_sections_and_wins_over_the_settings() {
        let text = "[checks.test]\nimage = \"node:22\"\nrun = \"npm test\"\n\n[app]\nbuild = \"pnpm build\"\npre_deploy = \"pnpm migrate\"\n";
        let app = parse(text).unwrap().unwrap();
        let build = Build { build: Some("npm run build".into()), ..Build::default() };
        let (b, r, said) = app.apply(&build, &Run::default(), None).unwrap();
        assert_eq!((b.build.as_deref(), r.pre_deploy.as_deref()), (Some("pnpm build"), Some("pnpm migrate")));
        assert_eq!(said, ["roll.toml sets build: pnpm build", "roll.toml sets pre_deploy: pnpm migrate"]);
        assert!(parse("[app]\nbuidl = \"x\"\n").is_err(), "a typo isn't ignored");
        assert!(parse("[app]\nhealth_path = \"healthz\"\n").unwrap().unwrap().apply(&Build::default(), &Run::default(), None).is_err(), "checked like the API");
        assert_eq!(parse("[preview]\nimage = \"x\"\n").unwrap(), None);
        // Sidecars as arrays of tables, checked as the API checks them.
        let side = parse("[app.run]\nport = 80\n[[app.run.sidecars]]\nname = \"proxy\"\nimage = \"envoy:1\"\nvariables = [\"DATABASE_URL\"]\n").unwrap().unwrap();
        let (_, r, _) = side.apply(&Build::default(), &Run::default(), None).unwrap();
        assert_eq!((r.sidecars[0].name.as_str(), r.sidecars[0].variables.as_slice(), r.sidecars[0].memory_mb), ("proxy", ["DATABASE_URL".to_string()].as_slice(), 128));
    }

    #[test]
    fn run_settings_and_an_environments_block_go_on_top() {
        let text = "[app]\nstart = \"node a.js\"\n[app.run]\nconcurrency = 20\n[app.environments.production]\nwatch = [\"web/**\"]\n[app.environments.production.run]\nmin_instances = 1\n";
        let app = parse(text).unwrap().unwrap();
        let (b, r, said) = app.apply(&Build::default(), &Run::default(), Some(EnvScope { name: "production", ephemeral: false })).unwrap();
        assert_eq!((b.start.as_deref(), r.concurrency, r.min_instances), (Some("node a.js"), 20, 1));
        assert!(said.contains(&"roll.toml sets run.min_instances: 1 (production)".to_string()), "{said:?}");
        let (_, staging, _) = app.apply(&Build::default(), &Run::default(), Some(EnvScope { name: "staging", ephemeral: false })).unwrap();
        assert_eq!(staging.min_instances, Run::default().min_instances, "another environment doesn't get production's block");
        assert_eq!(app.watch_for("production").as_deref(), Some(&["web/**".to_string()][..]));
        assert!(parse("[app.run]\nvolume = { mount = \"/data\", size_gb = 9 }\n").is_err(), "a push can't move a volume");
        assert!(parse("[app.run]\nmin_instances = \"lots\"\n").unwrap().unwrap().apply(&Build::default(), &Run::default(), None).is_err());
    }

    #[test]
    fn previews_scale_to_zero_unless_their_block_says_otherwise() {
        let up = Run { min_instances: 2, max_instances: 3, ..Run::default() };
        let (r, note) = preview_defaults(up.clone(), None);
        assert_eq!((r.min_instances, note.is_some()), (0, true));
        let file = parse("[app.previews.run]\nmin_instances = 1\nmemory_mb = 256\n").unwrap().unwrap();
        let (_, r, said) = file.apply(&Build::default(), &up, Some(EnvScope { name: "pv-abc", ephemeral: true })).unwrap();
        assert_eq!((r.min_instances, r.memory_mb), (1, 256), "{said:?}");
        assert_eq!(preview_defaults(r, Some(&file)).0.min_instances, 1, "the block's choice stands");
        let (_, prod, _) = file.apply(&Build::default(), &up, Some(EnvScope { name: "production", ephemeral: false })).unwrap();
        assert_eq!(prod.memory_mb, up.memory_mb, "production doesn't get the previews' block");
    }
}
