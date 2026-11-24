//! A build's time by step (Vercel's and Netlify's build steps; docs/apps/README.md, "Build time by
//! step"): BuildKit's plain progress (`docker build --progress plain`, CodeBuild's too), read
//! from the build's log lines as they're written, so the log reads as it always did. Each of the
//! Dockerfile's steps (`[2/4] RUN npm ci`, `[builder 3/5] COPY . .`), sending the context and
//! exporting the image, with how long it took and whether its cache answered. A build step's own
//! output can't pose as BuildKit's: BuildKit prefixes every line of it with its step and a time.

use serde::{Deserialize, Serialize};

/// The most steps kept (a Dockerfile has a few dozen at most).
const MAX: usize = 200;
/// A step's name is cut to this.
const NAME_MAX: usize = 160;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildStep {
    pub name: String,
    /// How long it ran (ms), once BuildKit said `DONE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<i64>,
    /// Its cache answered: it didn't run.
    #[serde(default)]
    pub cached: bool,
    /// It's where the build failed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

/// BuildKit's vertices as their lines go past, by number.
#[derive(Default)]
pub struct BuildSteps {
    seen: Vec<(u32, bool, BuildStep)>,
}

impl BuildSteps {
    pub fn feed(&mut self, line: &str) {
        let Some((n, rest)) = line.strip_prefix('#').and_then(|l| l.split_once(' ')) else { return };
        let Ok(n) = n.parse::<u32>() else { return };
        let rest = rest.trim();
        if let Some((_, _, s)) = self.seen.iter_mut().find(|(m, _, _)| *m == n) {
            if rest == "CACHED" {
                s.cached = true;
            } else if let Some(t) = rest.strip_prefix("DONE ") {
                s.ms = t.strip_suffix('s').and_then(|t| t.parse::<f64>().ok()).filter(|t| t.is_finite() && *t >= 0.0).map(|t| (t * 1000.0).round() as i64);
            } else if rest.starts_with("ERROR") {
                s.failed = true;
            }
            return;
        }
        if self.seen.len() >= MAX * 2 {
            return;
        }
        // A vertex's first line names it.
        let kept = (rest.starts_with('[') && !rest.starts_with("[internal]") && !rest.starts_with("[auth]")) || rest == "[internal] load build context" || rest == "exporting to image";
        let mut name = match rest.split_once("@sha256:") {
            Some((image, _)) => image.to_string(),
            None => rest.to_string(),
        };
        if name.len() > NAME_MAX {
            let mut cut = NAME_MAX;
            while !name.is_char_boundary(cut) {
                cut -= 1;
            }
            name.truncate(cut);
            name.push('…');
        }
        self.seen.push((n, kept, BuildStep { name, ms: None, cached: false, failed: false }));
    }

    /// The steps worth showing, in the order they began.
    pub fn steps(&self) -> Vec<BuildStep> {
        self.seen.iter().filter(|(_, kept, _)| *kept).take(MAX).map(|(_, _, s)| s.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_progress_reads_as_steps_with_times_and_cache_hits() {
        let log = "#0 building with \"default\" instance using docker driver\n\n#1 [internal] load build definition from Dockerfile\n#1 transferring dockerfile: 105B done\n#1 DONE 0.1s\n\n#2 [internal] load metadata for docker.io/library/node:22\n#2 DONE 0.0s\n\n#3 [internal] load build context\n#3 transferring context: 30B done\n#3 DONE 0.4s\n\n#4 [1/4] FROM docker.io/library/node:22@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc\n#4 resolve docker.io/library/node:22@sha256:d9e8 0.0s done\n#4 DONE 0.2s\n\n#5 [2/4] COPY package.json ./\n#5 CACHED\n\n#6 [3/4] RUN npm ci\n#6 0.314 #9 DONE 99.0s\n#6 DONE 21.3s\n\n#7 [4/4] RUN npm run build\n#7 1.2 Error: boom\n#7 ERROR: process \"/bin/sh -c npm run build\" did not complete successfully: exit code: 1\n";
        let mut b = BuildSteps::default();
        log.lines().for_each(|l| b.feed(l));
        let s = b.steps();
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["[internal] load build context", "[1/4] FROM docker.io/library/node:22", "[2/4] COPY package.json ./", "[3/4] RUN npm ci", "[4/4] RUN npm run build"]);
        assert_eq!((s[0].ms, s[1].ms), (Some(400), Some(200)));
        assert!(s[2].cached && s[2].ms.is_none());
        // A step's own output saying `#9 DONE` is its output, not BuildKit's.
        assert_eq!((s[3].ms, s[3].cached), (Some(21_300), false));
        assert!(s[4].failed && s[4].ms.is_none());
    }
}
