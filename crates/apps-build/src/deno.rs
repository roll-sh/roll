//! Deno: its `start` task, or the entry module run with the permissions a server needs, on the
//! Deno image (Railpack's and Fly's Deno).

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const IMAGE: &str = "denoland/deno:2.1.4";
/// Where a Deno app's entry usually is, first found wins.
const ENTRIES: [&str; 5] = ["main.ts", "main.js", "server.ts", "mod.ts", "src/main.ts"];

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some((file, config)) = ["deno.json", "deno.jsonc"].into_iter().find_map(|f| text(tree, f).map(|t| (f, t))) else {
        return Ok(None);
    };
    let mut notes = vec![format!("found {file}: Deno")];
    // deno.jsonc's comments stripped (whole-line ones: what a config file has).
    let cleaned: String = config.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let json: serde_json::Value = serde_json::from_str(&cleaned).unwrap_or_default();
    let task = |name: &str| json["tasks"][name].as_str().is_some();
    let start = if task("start") {
        notes.push(format!("{file} has a start task: deno task start"));
        Some("deno task start".to_string())
    } else if let Some(entry) = ENTRIES.into_iter().find(|e| tree.exists(e)) {
        notes.push(format!("no start task, {entry} at the root: run with network, environment and file access"));
        Some(format!("deno run --allow-net --allow-env --allow-read {entry}"))
    } else if o.start.is_none() {
        return Err(Error::NoBinary(format!("{file} has no start task and there's no main.ts: add tasks.start or set start")));
    } else {
        None
    };
    let build = task("build").then(|| {
        notes.push(format!("{file} has a build task: deno task build"));
        "deno task build".to_string()
    });
    let manifests: Vec<String> = [file, "deno.lock"].into_iter().filter(|f| tree.exists(f)).map(String::from).collect();
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(IMAGE, None);
        df.line("WORKDIR /app");
        // The dependencies are the image's (DENO_DIR), not a cache mount: they run with it.
        df.source(Some(&manifests), &[], c);
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.serve(Some("deno"), c.start);
        df.finish()
    });
    Ok(Some(Draft { provider: Provider::Deno, framework: None, language_version: Some("2".into()), install: Some("deno install".into()), build, start, notes, render }))
}
