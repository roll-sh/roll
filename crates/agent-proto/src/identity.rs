//! What makes projects and machines recognisable at a glance (docs/code/agents.md, "Identity";
//! docs/code/agents-design.md, "Identity"): a colour each, a project's logo or monogram, a
//! machine's kind.
//!
//! Ported from T3 Code: `ProjectIconColor`, the project icon overrides and `ProjectFaviconPath`
//! in `packages/contracts/src/project.ts`. roll's palette is its own: T3's has orange, amber and
//! yellow, which roll never uses, and blue, which on roll means busy.

use serde::{Deserialize, Serialize};

/// A categorical colour for a project or a machine. Each is a token pair (dark, light) in the
/// dashboard (`--id-<name>`). None of them is blue (the accent: busy, active), orange, amber or
/// yellow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityColor {
    Slate,
    Violet,
    Purple,
    Pink,
    Rose,
    Teal,
    Cyan,
    Green,
}

impl IdentityColor {
    pub const ALL: [IdentityColor; 8] = [
        IdentityColor::Slate,
        IdentityColor::Violet,
        IdentityColor::Purple,
        IdentityColor::Pink,
        IdentityColor::Rose,
        IdentityColor::Teal,
        IdentityColor::Cyan,
        IdentityColor::Green,
    ];
}

/// How a project shows itself. `project.update` sets it with the rest of its settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectIcon {
    /// The logo found in the project (`Project.logo`), else the monogram on its colour.
    #[default]
    Auto,
    /// The monogram on its colour even when the project has a logo; `text` replaces the letters
    /// made from the name (T3's monogram override).
    Monogram {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// An emoji on the project's colour.
    Emoji { emoji: String },
    /// A picture in the project, relative to it (T3's `faviconPath`). The daemon reads it into
    /// `Project.logo`; one it can't read shows the monogram.
    File { path: String },
}

impl ProjectIcon {
    /// Letters a monogram may have.
    pub const MAX_MONOGRAM: usize = 3;
    /// Bytes an emoji may have (a family or a flag with modifiers is a dozen code points).
    pub const MAX_EMOJI: usize = 32;
    /// Bytes a logo's path may have.
    pub const MAX_PATH: usize = 1024;
    /// What a logo may be, by extension (T3's list, less `.icns`, which browsers can't show).
    pub const EXTENSIONS: [&'static str; 8] = ["svg", "png", "ico", "gif", "webp", "jpg", "jpeg", "avif"];

    pub fn is_auto(&self) -> bool {
        self == &ProjectIcon::Auto
    }

    /// Trimmed, or why not (mirrored in `apps/dashboard/src/lib/agents-identity.ts`).
    pub fn check(self) -> Result<ProjectIcon, String> {
        let plain = |s: &str| !s.chars().any(|c| c.is_whitespace() || c.is_control());
        Ok(match self {
            ProjectIcon::Auto => ProjectIcon::Auto,
            ProjectIcon::Monogram { text } => {
                let text = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
                if let Some(t) = &text
                    && (t.chars().count() > Self::MAX_MONOGRAM || !plain(t) || !t.chars().next().is_some_and(char::is_alphanumeric))
                {
                    return Err(format!("a monogram is 1 to {} letters or digits", Self::MAX_MONOGRAM));
                }
                ProjectIcon::Monogram { text }
            }
            ProjectIcon::Emoji { emoji } => {
                let emoji = emoji.trim().to_string();
                if emoji.is_empty() || emoji.len() > Self::MAX_EMOJI || !plain(&emoji) || emoji.chars().any(|c| c.is_ascii()) {
                    return Err("an emoji is one emoji".into());
                }
                ProjectIcon::Emoji { emoji }
            }
            ProjectIcon::File { path } => {
                let path = path.trim().trim_start_matches("./").to_string();
                let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
                let inside = !path.starts_with('/') && !path.contains('\\') && path.split('/').all(|p| !p.is_empty() && p != "..");
                if path.is_empty() || path.len() > Self::MAX_PATH || !inside {
                    return Err("a logo's path is a file inside the project, like public/logo.svg".into());
                }
                if !ext.is_some_and(|e| Self::EXTENSIONS.contains(&e.as_str())) {
                    return Err(format!("a logo is a picture: {}", Self::EXTENSIONS.join(", ")));
                }
                ProjectIcon::File { path }
            }
        })
    }
}

/// A project's own logo, found by its daemon (T3's `ProjectFaviconResolver`): the picture a
/// `File` icon names, else the first of the well-known favicon and logo paths, else the icon an
/// `index.html` or root route links to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLogo {
    /// Where it is, relative to the project.
    pub path: String,
    /// The picture as a `data:` URL: an SVG as found, anything else as a PNG at most
    /// [`ProjectLogo::MAX_PX`] square. At most [`ProjectLogo::MAX_URL`] bytes. Clients show it
    /// only as an image (`<img>`), never inline, so an SVG's scripts never run.
    pub url: String,
}

impl ProjectLogo {
    pub const MAX_URL: usize = 48 << 10;
    pub const MAX_PX: u32 = 64;
}

/// What a machine is, for its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Laptop,
    Desktop,
    Server,
    /// A cloud machine (roll's sandboxes are).
    Cloud,
    Container,
}

/// How a machine shows itself, set by its owner on the relay with its name
/// (`PATCH …/machines/{machine}`), so an offline machine or a sleeping sandbox has it too.
/// Unset: a colour from the machine's id, and a kind from its OS (a sandbox is `cloud`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineAppearance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<IdentityColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MachineKind>,
}

impl MachineAppearance {
    pub fn is_empty(&self) -> bool {
        self == &MachineAppearance::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_checked() {
        let mono = |t: &str| ProjectIcon::Monogram { text: Some(t.into()) }.check();
        assert_eq!(mono(" rl ").unwrap(), ProjectIcon::Monogram { text: Some("rl".into()) });
        assert_eq!(mono("  ").unwrap(), ProjectIcon::Monogram { text: None });
        assert!(mono("ROLL").is_err());
        assert!(mono("a b").is_err());
        assert!(mono("-x").is_err());
        let emoji = |e: &str| ProjectIcon::Emoji { emoji: e.into() }.check();
        assert!(emoji("🦀").is_ok());
        assert!(emoji("👨‍👩‍👧").is_ok());
        assert!(emoji("x").is_err());
        assert!(emoji("").is_err());
        let file = |p: &str| ProjectIcon::File { path: p.into() }.check();
        assert_eq!(file("./public/logo.svg").unwrap(), ProjectIcon::File { path: "public/logo.svg".into() });
        assert!(file("../x.png").is_err());
        assert!(file("/etc/x.png").is_err());
        assert!(file("a//b.png").is_err());
        assert!(file("logo.txt").is_err());
        assert!(file("AppIcon.icns").is_err());

        // Unset identity fields stay off the wire.
        assert_eq!(serde_json::to_string(&MachineAppearance::default()).unwrap(), "{}");
        assert_eq!(serde_json::to_string(&ProjectIcon::Auto).unwrap(), r#"{"kind":"auto"}"#);
        let m: ProjectIcon = serde_json::from_str(r#"{"kind":"monogram"}"#).unwrap();
        assert_eq!(m, ProjectIcon::Monogram { text: None });
    }
}
