//! The variables a repo says it needs (Railway's suggested variables from `.env.example`,
//! Heroku's `app.json` and Render's `sync: false` asking before the first deploy): the names in
//! the root's `.env.example` (or `.env.sample`, `.env.template`, `.env.dist`, `example.env`), so
//! New service asks for them before the build rather than explaining the crash after. Names
//! only: what an example file holds beside them is never read into the plan.

use crate::Tree;

/// The files read, the first one there.
pub const FILES: [&str; 5] = [".env.example", ".env.sample", ".env.template", ".env.dist", "example.env"];
/// The most names kept.
const MAX: usize = 100;

/// The names the repo's example file declares, in its order, and which file it was.
pub fn expected(tree: &dyn Tree) -> Option<(&'static str, Vec<String>)> {
    let (file, text) = FILES.iter().find_map(|f| tree.read(f).and_then(|b| String::from_utf8(b).ok()).map(|t| (*f, t)))?;
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, _)) = line.split_once('=') else { continue };
        let name = name.trim();
        let ok = name.len() <= 128 && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if ok && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
            if names.len() == MAX {
                break;
            }
        }
    }
    (!names.is_empty()).then_some((file, names))
}

#[cfg(test)]
mod tests {
    use crate::tests::tree as files;

    #[test]
    fn an_example_file_names_what_the_app_needs() {
        let t = files(&[(".env.example", "# Stripe\nSTRIPE_KEY=sk_test_xxx\nexport DATABASE_URL=\n\nNEXT_PUBLIC_SITE=https://x\nnot a line\n1BAD=x\nSTRIPE_KEY=again\n")]);
        assert_eq!(super::expected(&t), Some((".env.example", vec!["STRIPE_KEY".to_string(), "DATABASE_URL".into(), "NEXT_PUBLIC_SITE".into()])));
        assert_eq!(super::expected(&files(&[("app.js", "")])), None);
    }
}
