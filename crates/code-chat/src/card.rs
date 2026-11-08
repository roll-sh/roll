//! Platform-neutral views of an agent thread. roll builds these; [`crate::slack::blocks`] and
//! [`crate::discord`] render them, applying each platform's escaping and limits. All text here is
//! plain: renderers escape it, so callers never pre-escape.

use serde::{Deserialize, Serialize};

/// How many of a status card's lines a renderer shows (the newest).
pub const MAX_STATUS_LINES: usize = 8;

/// The characters each status line keeps before it is cut with `…`.
pub const MAX_LINE_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Running,
    /// Waiting on a person (an approval or an answer).
    Waiting,
    Done,
    Failed,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub label: String,
    pub url: String,
}

/// An agent thread's live status message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCard {
    pub title: String,
    pub tone: Tone,
    /// `Running · 3 steps`.
    pub status: String,
    /// The step summary, oldest first. Renderers show the newest [`MAX_STATUS_LINES`], each cut
    /// to [`MAX_LINE_CHARS`].
    pub lines: Vec<String>,
    /// "Open in roll", the pull request.
    pub links: Vec<Link>,
    pub footer: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceStyle {
    #[default]
    Default,
    Primary,
    Danger,
}

/// A button or select option.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    /// Opaque and ours: it comes back on the click. Platforms cap it, and a renderer cuts a value
    /// that doesn't fit (it then won't match anything on the way back), so keep it short:
    ///
    /// - Slack: a button value ≤ 2000 bytes, a select option value ≤ 150.
    /// - Discord: a button's `custom_id` *is* the value, ≤ 100; a select option value ≤ 100.
    ///
    /// ≤ 100 ASCII bytes fits everywhere.
    pub value: String,
    #[serde(default)]
    pub style: ChoiceStyle,
}

/// One question an agent asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionView {
    /// Short, ASCII, no `:` (it ends Discord custom ids, see [`crate::discord::question_id`]).
    pub id: String,
    pub header: String,
    pub text: String,
    /// Empty: a free-text question.
    pub options: Vec<Choice>,
    pub multi: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestKind {
    Approval {
        choices: Vec<Choice>,
    },
    Questions {
        questions: Vec<QuestionView>,
        /// The Submit button; its value comes back with the answers.
        submit: Choice,
    },
    /// Decided: the card keeps the record and drops its buttons.
    Answered {
        by: String,
        answer: String,
    },
}

/// A request an agent is waiting on (an approval or questions), with buttons and selects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestCard {
    pub title: String,
    /// The command, file list or reason: rendered as code.
    pub detail: Option<String>,
    pub kind: RequestKind,
}

impl StatusCard {
    /// Plain fallback text: notifications, screen readers, clients without blocks.
    pub fn text(&self) -> String {
        if self.status.trim().is_empty() {
            self.title.clone()
        } else {
            format!("{} · {}", self.title, self.status)
        }
    }

    /// The lines a renderer shows: the newest [`MAX_STATUS_LINES`], each cut.
    pub(crate) fn shown_lines(&self) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(MAX_STATUS_LINES);
        self.lines[skip..]
            .iter()
            .map(|l| crate::truncate(l, MAX_LINE_CHARS))
            .filter(|l| !l.is_empty())
            .collect()
    }
}

impl RequestCard {
    /// Plain fallback text.
    pub fn text(&self) -> String {
        match &self.kind {
            RequestKind::Approval { .. } => format!("{} · waiting for approval", self.title),
            RequestKind::Questions { questions, .. } => {
                let n = questions.len();
                let s = if n == 1 { "" } else { "s" };
                format!("{} · {n} question{s}", self.title)
            }
            RequestKind::Answered { by, answer } => {
                format!("{} · answered by {by}: {answer}", self.title)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(lines: usize) -> StatusCard {
        StatusCard {
            title: "Fix the flaky test".into(),
            tone: Tone::Running,
            status: "Running · 3 steps".into(),
            lines: (0..lines).map(|i| format!("step {i}")).collect(),
            links: vec![],
            footer: None,
        }
    }

    #[test]
    fn shows_the_newest_lines_cut() {
        let mut card = status(12);
        assert_eq!(card.shown_lines().first().unwrap(), "step 4");
        assert_eq!(card.shown_lines().len(), MAX_STATUS_LINES);
        card.lines = vec!["x".repeat(500)];
        assert_eq!(card.shown_lines()[0].chars().count(), MAX_LINE_CHARS);
    }
}
