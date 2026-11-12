//! Writing Dockerfiles.

use crate::{Commands, DEFAULT_PORT};

/// A Dockerfile being written, BuildKit syntax first.
pub(crate) struct Df {
    text: String,
    /// How the build stage gets the source: the context, or a stage that built assets in it.
    copy_source: &'static str,
    /// Variables install and build steps see ([`crate::Overrides::build_env`]).
    env: Vec<String>,
}

impl Df {
    pub(crate) fn new(c: &Commands) -> Self {
        Df {
            text: String::from("# syntax=docker/dockerfile:1\n"),
            copy_source: "COPY . .",
            env: c.env.to_vec(),
        }
    }

    /// Starts with the `assets` stage (`node::assets`), and takes the source from it.
    pub(crate) fn with_assets(assets: Option<&crate::node::Assets>, c: &Commands) -> Self {
        let mut df = Df::new(c);
        if let Some(a) = assets {
            df.text.push_str(&a.render(c));
            df.copy_source = "COPY --from=assets /app ./";
        }
        df
    }

    /// Copies the whole source into the working directory.
    pub(crate) fn copy_source(&mut self) {
        self.line(self.copy_source);
    }

    pub(crate) fn from(&mut self, image: &str, stage: Option<&str>) {
        self.text.push('\n');
        match stage {
            Some(stage) => self.line(&format!("FROM {image} AS {stage}")),
            None => self.line(&format!("FROM {image}")),
        }
        if !self.env.is_empty() {
            self.line("ARG ROLL_BUILD_ENV");
        }
    }

    pub(crate) fn line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
    }

    pub(crate) fn env(&mut self, vars: &[(&str, &str)]) {
        if vars.is_empty() {
            return;
        }
        let pairs: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.line(&format!("ENV {}", pairs.join(" ")));
    }

    /// `RUN cmd` with a BuildKit cache mount on each of `caches`. A command over several lines
    /// (an override) goes in the exec form, so its newlines stay the shell's.
    pub(crate) fn run(&mut self, caches: &[&str], cmd: &str) {
        let mut mounts: String = caches
            .iter()
            .map(|c| format!("--mount=type=cache,target={c} "))
            .collect();
        for k in &self.env {
            mounts.push_str(&format!("--mount=type=secret,id={k},env={k} "));
        }
        if cmd.contains('\n') || cmd.trim_end().ends_with('\\') {
            self.line(&format!("RUN {mounts}[\"/bin/sh\", \"-c\", {}]", json(cmd)));
        } else {
            self.line(&format!("RUN {mounts}{cmd}"));
        }
    }

    /// Copies the source in and installs its dependencies. When `manifests` are all the install
    /// reads, they're copied and installed first, so the install's layer survives source edits.
    pub(crate) fn source(&mut self, manifests: Option<&[String]>, caches: &[&str], c: &Commands) {
        match (manifests, c.install) {
            (Some(files), Some(install)) if !c.custom_install && !files.is_empty() => {
                self.line(&format!("COPY {} ./", files.join(" ")));
                self.run(caches, install);
                self.copy_source();
            }
            (_, install) => {
                self.copy_source();
                if let Some(install) = install {
                    self.run(caches, install);
                }
            }
        }
    }

    /// The end of a runtime stage: `PORT`, `EXPOSE`, the user, and the start command through
    /// `sh -c` so `$PORT` expands.
    pub(crate) fn serve(&mut self, user: Option<&str>, start: Option<&str>) {
        self.line(&format!("ENV PORT={DEFAULT_PORT}"));
        self.line(&format!("EXPOSE {DEFAULT_PORT}"));
        if let Some(user) = user {
            self.line(&format!("USER {user}"));
        }
        if let Some(start) = start {
            self.line(&cmd(start));
        }
    }

    pub(crate) fn finish(self) -> String {
        self.text
    }
}

/// `CMD ["sh", "-c", "exec <start>"]`.
pub(crate) fn cmd(start: &str) -> String {
    format!("CMD [\"sh\", \"-c\", {}]", json(&exec(start)))
}

pub(crate) fn json(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

/// `start` with `exec` in front when it's one simple command, so the app replaces the shell and
/// gets the stop signal itself.
fn exec(start: &str) -> String {
    let simple = !start.contains(['\n', ';', '&', '|', '<', '>', '(', ')', '`'])
        && start
            .split_whitespace()
            .next()
            .is_some_and(|w| !w.contains('=') && !matches!(w, "exec" | "cd" | "export"));
    if simple {
        format!("exec {start}")
    } else {
        start.to_string()
    }
}

/// `apt-get install` of `packages` in one layer, without the package lists.
pub(crate) fn apt(packages: &[&str]) -> String {
    format!(
        "apt-get update && apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/*",
        packages.join(" ")
    )
}
