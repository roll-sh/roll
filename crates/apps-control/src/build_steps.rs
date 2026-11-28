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
