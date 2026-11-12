//! `@roll …` in a thread comment.
//!
//! ```text
//! @roll review [standard|deep|max]   review what changed since the last review
//! @roll full review [mode]           review the whole change again
//! @roll pause / @roll resume         stop / restart automatic reviews of this change
//! @roll resolve                      resolve roll's open threads on the change
//! @roll explain                      explain the thread's finding in more detail
//! @roll fix                          apply the thread's suggestion (or write one) as a new revision
//! @roll don't flag <what>            remember it for future reviews (a learning)
//! @roll <anything else>              a question, answered in the thread
//! ```

use crate::mode::Mode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Review { mode: Option<Mode>, full: bool },
    Pause,
    Resume,
    Resolve,
    Explain,
    Fix,
    /// "don't flag …", "stop flagging …", "ignore …", "remember …".
    Learn(String),
    Ask(String),
}

/// The command in a comment, if it mentions `@roll` (not `@rollbar`, not inside code).
pub fn parse(body: &str) -> Option<Command> {
    let mut in_code = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find("@roll") {
            let at = from + i;
            let end = at + "@roll".len();
            let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
            let after_ok = lower.as_bytes().get(end).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_'));
            if before_ok && after_ok {
                return Some(command(t[end..].trim_start_matches([',', ':', ' ']).trim()));
            }
            from = end;
        }
    }
    None
}

fn command(rest: &str) -> Command {
    let lower = rest.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let mode_in = |ws: &[&str]| ws.iter().find_map(|w| Mode::parse(w.trim_matches(|c: char| !c.is_ascii_alphabetic())));
    match words.as_slice() {
        ["full", "review", rest @ ..] | ["review", "full", rest @ ..] => Command::Review { mode: mode_in(rest), full: true },
        ["review", rest @ ..] => Command::Review { mode: mode_in(rest), full: false },
        ["pause", ..] => Command::Pause,
        ["resume", ..] => Command::Resume,
        ["resolve", ..] => Command::Resolve,
        ["explain", ..] => Command::Explain,
        ["fix", ..] => Command::Fix,
        _ => {
            for p in ["don't flag", "dont flag", "do not flag", "stop flagging", "never flag", "ignore", "remember"] {
                if let Some(x) = lower.strip_prefix(p) {
                    let text = rest[rest.len() - x.len()..].trim().trim_end_matches('.').trim();
                    if !text.is_empty() {
                        let lead = if p == "remember" { "" } else { "Don't flag " };
                        return Command::Learn(format!("{lead}{text}"));
                    }
                }
            }
            Command::Ask(rest.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse("@roll review"), Some(Command::Review { mode: None, full: false }));
        assert_eq!(parse("hey @roll review deep please"), Some(Command::Review { mode: Some(Mode::Deep), full: false }));
        assert_eq!(parse("@roll full review max"), Some(Command::Review { mode: Some(Mode::Max), full: true }));
        assert_eq!(parse("@roll review full"), Some(Command::Review { mode: None, full: true }));
        assert_eq!(parse("@Roll: pause"), Some(Command::Pause));
        assert_eq!(parse("@roll resume"), Some(Command::Resume));
        assert_eq!(parse("@roll resolve"), Some(Command::Resolve));
        assert_eq!(parse("@roll explain"), Some(Command::Explain));
        assert_eq!(parse("@roll fix"), Some(Command::Fix));
        assert_eq!(parse("@roll don't flag missing docs on private helpers."), Some(Command::Learn("Don't flag missing docs on private helpers".into())));
        assert_eq!(parse("@roll remember we use tabs in Go"), Some(Command::Learn("we use tabs in Go".into())));
        assert_eq!(parse("@roll why is this a bug?"), Some(Command::Ask("why is this a bug?".into())));
        assert_eq!(parse("cc @rollbar"), None);
        assert_eq!(parse("me@roll.sh"), None);
        assert_eq!(parse("```\n@roll review\n```"), None);
        assert_eq!(parse("no mention"), None);
    }
}
