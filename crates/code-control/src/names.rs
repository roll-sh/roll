//! Owner and repository names (docs/code/architecture.md, "Namespaces and public URLs").

/// Names roll.sh keeps for itself: owners share the top level of `roll.sh/<owner>` with the
/// app's own routes. Mirrors `apps/dashboard/src/lib/reserved-names.ts`; change both together.
pub const RESERVED: &[&str] = &[
    // Routes and files in apps/dashboard/src/app.
    "dashboard", "icon.svg", "favicon.ico", "_next",
    // Next.js and web conventions.
    "api", "robots.txt", "sitemap.xml", "manifest.json", "manifest.webmanifest", ".well-known", "opengraph-image",
    "twitter-image", "apple-icon", "static", "assets", "public",
    // Product and marketing pages.
    "about", "blog", "brand", "careers", "changelog", "community", "contact", "customers", "docs", "enterprise", "explore",
    "features", "help", "jobs", "legal", "pricing", "privacy", "security", "status", "support", "terms", "trust",
    // Accounts and auth.
    "account", "admin", "auth", "billing", "callback", "join", "login", "logout", "new", "oauth", "settings", "signin",
    "signout", "signup", "sso",
    // GitHub-style paths that must never be an owner.
    "code", "git", "raw", "orgs", "organizations", "users", "repos", "search", "notifications", "marketplace", "topics",
    "trending", "gist", "gists", "sponsors",
    // Products and dashboard sections.
    "databases", "postgres", "redis", "queues", "tasks", "streams", "kafka", "functions", "deployments", "edge", "logs",
    "observability", "previews",
    // roll itself.
    "roll", "rollsh", "roll-sh", "www", "mail", "app", "apps", "cdn", "v1", "health", "root", "system",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Invalid,
    Reserved,
}

impl NameError {
    pub fn reason(&self) -> &'static str {
        match self {
            NameError::Invalid => "invalid",
            NameError::Reserved => "reserved",
        }
    }
}

/// An owner name, lowercased: 1–39 of `[a-z0-9-]`, no leading, trailing or double `-`.
pub fn owner(name: &str) -> Result<String, (NameError, String)> {
    let n = name.trim().to_ascii_lowercase();
    let valid = (1..=39).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
        && !n.contains("--");
    if !valid {
        return Err((
            NameError::Invalid,
            "names are 1-39 characters of a-z, 0-9 and single hyphens, not at the start or end".into(),
        ));
    }
    if RESERVED.contains(&n.as_str()) {
        return Err((NameError::Reserved, format!("{n} is reserved")));
    }
    Ok(n)
}

/// A repository name, case kept: 1–100 of `[A-Za-z0-9._-]`, not `.`/`..`, not ending in `.git`.
pub fn repo(name: &str) -> Result<String, String> {
    let n = name.trim();
    let valid = (1..=100).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && n != "."
        && n != ".."
        && !n.to_ascii_lowercase().ends_with(".git");
    if !valid {
        return Err("repository names are 1-100 characters of A-Z, a-z, 0-9, `.`, `_` and `-`, and don't end in .git".into());
    }
    Ok(n.to_string())
}

/// A branch name for `default_branch`.
pub fn branch(name: &str) -> Result<String, String> {
    let n = name.trim().trim_start_matches("refs/heads/");
    let valid = !n.is_empty()
        && n.len() <= 200
        && !n.starts_with(['/', '-', '.'])
        && !n.ends_with(['/', '.'])
        && !n.contains("..")
        && !n.contains("//")
        && !n.ends_with(".lock")
        && n.bytes().all(|b| b > b' ' && !b"~^:?*[\\\x7f".contains(&b));
    if !valid {
        return Err(format!("invalid branch name {n:?}"));
    }
    Ok(n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners() {
        assert_eq!(owner("Roll-Labs").unwrap(), "roll-labs");
        assert_eq!(owner("a").unwrap(), "a");
        assert_eq!(owner(&"a".repeat(39)).unwrap().len(), 39);
        for bad in ["", "-a", "a-", "a--b", "a_b", "a.b", &"a".repeat(40)] {
            assert_eq!(owner(bad).unwrap_err().0, NameError::Invalid, "{bad}");
        }
        assert_eq!(owner("Dashboard").unwrap_err().0, NameError::Reserved);
    }

    #[test]
    fn repos() {
        assert_eq!(repo("My.Repo_1-x").unwrap(), "My.Repo_1-x");
        for bad in ["", ".", "..", "a/b", "x.git", "X.GIT", "a b", &"a".repeat(101)] {
            assert!(repo(bad).is_err(), "{bad}");
        }
    }

    /// The dashboard's list (apps/dashboard/src/lib/reserved-names.ts) and this one must agree.
    #[test]
    fn reserved_matches_the_dashboard() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/dashboard/src");
        let ts = std::fs::read_to_string(dir.join("lib/reserved-names.ts")).expect("reserved-names.ts");
        let start = ts.find("RESERVED_NAMES = [").expect("RESERVED_NAMES array") + "RESERVED_NAMES = [".len();
        let body = &ts[start..start + ts[start..].find(']').expect("end of array")];
        let mut theirs: Vec<&str> = body
            .lines()
            .map(|l| l.split("//").next().unwrap_or("").trim())
            .flat_map(|l| l.split(','))
            .filter_map(|item| item.trim().strip_prefix('"').and_then(|s| s.strip_suffix('"')))
            .collect();
        let mut ours = RESERVED.to_vec();
        theirs.sort_unstable();
        ours.sort_unstable();
        assert_eq!(ours, theirs, "crates/code-control/src/names.rs and apps/dashboard/src/lib/reserved-names.ts differ");

        // Every top-level route the Next app answers is reserved.
        for entry in std::fs::read_dir(dir.join("app")).expect("src/app") {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            let route = if entry.file_type().unwrap().is_dir() {
                !name.starts_with(['[', '(', '_', '@'])
            } else {
                let stem = name.split('.').next().unwrap_or("");
                ["icon", "apple-icon", "favicon", "opengraph-image", "twitter-image", "robots", "sitemap", "manifest"].contains(&stem)
            };
            if route {
                assert!(RESERVED.contains(&name.as_str()), "src/app/{name} is a top-level route but not a reserved name");
            }
        }
        for (i, n) in RESERVED.iter().enumerate() {
            assert_eq!(*n, n.to_ascii_lowercase(), "reserved names are lowercase");
            assert!(!RESERVED[..i].contains(n), "{n} is listed twice");
        }
    }

    #[test]
    fn branches() {
        assert_eq!(branch("refs/heads/main").unwrap(), "main");
        assert_eq!(branch("feature/x").unwrap(), "feature/x");
        for bad in ["", "a..b", "a.lock", "-x", "a b", "a~1", "x/"] {
            assert!(branch(bad).is_err(), "{bad}");
        }
    }
}
