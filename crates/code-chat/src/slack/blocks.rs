//! Block Kit rendering of [`crate::card`] views. Every piece of user text is escaped (mrkdwn) or
//! sent as `plain_text` (which Slack never parses for markup), and cut to Slack's limits:
//! Slack rejects the whole message when any one field is over.
//!
//! Interactive elements carry roll's ids: `roll.choice.<i>` (an approval button, value =
//! the choice's value), `roll.q.<question id>` in block `roll.qb.<question id>` (a question's
//! select or text input), `roll.submit` (value = the Submit choice's value), `roll.link.<i>`
//! (a link button; Slack still posts its click, which can be ignored).

use super::escape;
use crate::card::{Choice, ChoiceStyle, QuestionView, RequestCard, RequestKind, StatusCard, Tone};
use crate::{cut_bytes, truncate};
use serde_json::{Value, json};

pub const MAX_BLOCKS: usize = 50;
pub const MAX_SECTION_TEXT: usize = 3000;
pub const MAX_HEADER_TEXT: usize = 150;
pub const MAX_BUTTON_TEXT: usize = 75;
pub const MAX_OPTION_TEXT: usize = 75;
pub const MAX_OPTION_VALUE: usize = 150;
pub const MAX_BUTTON_VALUE: usize = 2000;
pub const MAX_URL: usize = 3000;
pub const MAX_ACTIONS_ELEMENTS: usize = 25;
pub const MAX_OPTIONS: usize = 100;
pub const MAX_INPUT_LABEL: usize = 2000;
/// `block_id` and `action_id`.
pub const MAX_ID: usize = 255;
pub const MAX_CONTEXT_TEXT: usize = 3000;
const MAX_MODAL_TITLE: usize = 24;

/// Escape `s` for mrkdwn and cut it to `max` characters without splitting an entity.
fn mrkdwn(s: &str, max: usize) -> String {
    let s = s.trim();
    let escaped = escape(s);
    if escaped.chars().count() <= max {
        return escaped;
    }
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let piece = escape(c.encode_utf8(&mut [0; 4]));
        let len = piece.chars().count();
        if n + len > max.saturating_sub(1) {
            break;
        }
        out.push_str(&piece);
        n += len;
    }
    out.push('…');
    out
}

fn plain(s: &str, max: usize) -> Value {
    json!({ "type": "plain_text", "text": truncate(s, max), "emoji": true })
}

fn id(prefix: &str, rest: &str) -> String {
    cut_bytes(&format!("{prefix}{rest}"), MAX_ID).to_owned()
}

/// A code block for `detail`. Triple backticks inside are broken up so they can't end it early.
fn code(detail: &str) -> String {
    let body = detail.trim_matches('\n').replace("```", "`\u{200b}``");
    format!("```{}```", mrkdwn(&body, MAX_SECTION_TEXT - 6))
}

fn marker(tone: Tone) -> &'static str {
    match tone {
        Tone::Running => ":large_blue_circle:",
        Tone::Waiting => ":white_circle:",
        Tone::Done => ":white_check_mark:",
        Tone::Failed => ":x:",
        Tone::Info => ":information_source:",
    }
}

fn button(text: &str, action_id: String, value: &str, style: ChoiceStyle) -> Value {
    let mut b = json!({
        "type": "button",
        "text": plain(text, MAX_BUTTON_TEXT),
        "action_id": action_id,
        "value": cut_bytes(value, MAX_BUTTON_VALUE),
    });
    match style {
        ChoiceStyle::Primary => b["style"] = json!("primary"),
        ChoiceStyle::Danger => b["style"] = json!("danger"),
        ChoiceStyle::Default => {}
    }
    b
}

/// Buttons in actions blocks of at most [`MAX_ACTIONS_ELEMENTS`].
fn actions(block_prefix: &str, buttons: Vec<Value>) -> Vec<Value> {
    buttons
        .chunks(MAX_ACTIONS_ELEMENTS)
        .enumerate()
        .map(|(i, chunk)| {
            json!({ "type": "actions", "block_id": format!("{block_prefix}.{i}"), "elements": chunk })
        })
        .collect()
}

fn context(text: String) -> Value {
    json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": text }] })
}

fn section(text: String) -> Value {
    json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } })
}

/// Keep the first [`MAX_BLOCKS`] blocks, keeping the last one (the buttons that decide) when
/// cutting.
fn finish(mut blocks: Vec<Value>) -> Value {
    if blocks.len() > MAX_BLOCKS {
        let last = blocks.pop().expect("more than MAX_BLOCKS");
        blocks.truncate(MAX_BLOCKS - 1);
        blocks.push(last);
    }
    Value::Array(blocks)
}

/// An agent thread's status: a marker, the title and status; the newest step lines; link
/// buttons; the footer.
pub fn status(card: &StatusCard) -> Value {
    let head = format!(
        "{} *{}*\n{}",
        marker(card.tone),
        mrkdwn(&card.title, 1000),
        mrkdwn(&card.status, 1000)
    );
    let mut blocks = vec![section(head)];
    let lines = card.shown_lines();
    if !lines.is_empty() {
        blocks.push(context(mrkdwn(&lines.join("\n"), MAX_CONTEXT_TEXT)));
    }
    let links: Vec<Value> = card
        .links
        .iter()
        // Slack refuses the message over one long URL; drop the link instead.
        .filter(|l| l.url.len() <= MAX_URL)
        .enumerate()
        .map(|(i, l)| {
            json!({
                "type": "button",
                "text": plain(&l.label, MAX_BUTTON_TEXT),
                "action_id": format!("roll.link.{i}"),
                "url": l.url,
            })
        })
        .collect();
    blocks.extend(actions("roll.links", links));
    if let Some(footer) = card.footer.as_deref().filter(|f| !f.trim().is_empty()) {
        blocks.push(context(mrkdwn(footer, MAX_CONTEXT_TEXT)));
    }
    finish(blocks)
}

/// A request the agent waits on: the title, the detail as code, then the approval buttons or
/// the questions and a Submit button. Answered requests keep the record and lose their actions.
pub fn request(card: &RequestCard) -> Value {
    let mut blocks = vec![section(format!(
        "{} *{}*",
        marker(match card.kind {
            RequestKind::Answered { .. } => Tone::Done,
            _ => Tone::Waiting,
        }),
        mrkdwn(&card.title, MAX_SECTION_TEXT - 40)
    ))];
    if let Some(detail) = card.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        blocks.push(section(code(detail)));
    }
    match &card.kind {
        RequestKind::Approval { choices } => {
            let buttons = choices
                .iter()
                .enumerate()
                .map(|(i, c)| button(&c.label, format!("roll.choice.{i}"), &c.value, c.style))
                .collect();
            blocks.extend(actions("roll.choices", buttons));
        }
        RequestKind::Questions { questions, submit } => {
            // Leave room for the Submit button.
            for q in questions.iter().take(MAX_BLOCKS - blocks.len() - 1) {
                blocks.push(question(q));
            }
            let submit = button(
                &submit.label,
                "roll.submit".into(),
                &submit.value,
                ChoiceStyle::Primary,
            );
            blocks.push(
                json!({ "type": "actions", "block_id": "roll.submit", "elements": [submit] }),
            );
        }
        RequestKind::Answered { by, answer } => {
            blocks.push(context(mrkdwn(
                &format!("Answered by {by}: {answer}"),
                MAX_CONTEXT_TEXT,
            )));
        }
    }
    finish(blocks)
}

fn question_text(q: &QuestionView) -> String {
    match (q.header.trim(), q.text.trim()) {
        ("", text) => mrkdwn(text, MAX_SECTION_TEXT),
        (header, "") => format!("*{}*", mrkdwn(header, 200)),
        (header, text) => format!(
            "*{}*\n{}",
            mrkdwn(header, 200),
            mrkdwn(text, MAX_SECTION_TEXT - 210)
        ),
    }
}

fn option(c: &Choice) -> Value {
    json!({ "text": plain(&c.label, MAX_OPTION_TEXT), "value": cut_bytes(&c.value, MAX_OPTION_VALUE) })
}

/// A question with options: a section with a select. A free-text one: an input block (its
/// value arrives in `state.values` with the Submit click).
fn question(q: &QuestionView) -> Value {
    let block_id = id("roll.qb.", &q.id);
    let action_id = id("roll.q.", &q.id);
    if q.options.is_empty() {
        let label = match (q.header.trim(), q.text.trim()) {
            ("", text) => text,
            (header, _) => header,
        };
        let mut block = json!({
            "type": "input",
            "block_id": block_id,
            "dispatch_action": false,
            "optional": true,
            "label": plain(if label.is_empty() { "Answer" } else { label }, MAX_INPUT_LABEL),
            "element": { "type": "plain_text_input", "action_id": action_id, "multiline": true },
        });
        if !q.header.trim().is_empty() && !q.text.trim().is_empty() {
            block["hint"] = plain(&q.text, MAX_INPUT_LABEL);
        }
        return block;
    }
    let (kind, placeholder) = if q.multi {
        ("multi_static_select", "Choose any")
    } else {
        ("static_select", "Choose one")
    };
    let options: Vec<Value> = q.options.iter().take(MAX_OPTIONS).map(option).collect();
    json!({
        "type": "section",
        "block_id": block_id,
        "text": { "type": "mrkdwn", "text": question_text(q) },
        "accessory": {
            "type": kind,
            "action_id": action_id,
            "placeholder": plain(placeholder, MAX_OPTION_TEXT),
            "options": options,
        },
    })
}

/// A `views.open` modal asking for free text: one multiline input (`roll.text` / `roll.text`).
/// `private_metadata` (≤ 3000 characters) comes back with the submission.
pub fn text_modal(callback_id: &str, private_metadata: &str, title: &str, label: &str) -> Value {
    json!({
        "type": "modal",
        "callback_id": cut_bytes(callback_id, MAX_ID),
        "private_metadata": private_metadata,
        "title": plain(title, MAX_MODAL_TITLE),
        "submit": plain("Send", MAX_MODAL_TITLE),
        "close": plain("Cancel", MAX_MODAL_TITLE),
        "blocks": [{
            "type": "input",
            "block_id": "roll.text",
            "label": plain(label, MAX_INPUT_LABEL),
            "element": { "type": "plain_text_input", "action_id": "roll.text", "multiline": true },
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Link;

    fn choice(i: usize) -> Choice {
        Choice {
            label: format!("Choice {i}"),
            value: format!("v{i}"),
            style: if i == 0 {
                ChoiceStyle::Primary
            } else {
                ChoiceStyle::Default
            },
        }
    }

    fn texts(v: &Value) -> String {
        v.to_string()
    }

    #[test]
    fn status_card_blocks() {
        let card = StatusCard {
            title: "Fix <script>alert(1)</script>".into(),
            tone: Tone::Running,
            status: "Running · 3 steps".into(),
            lines: (0..12).map(|i| format!("step {i} <@U1>")).collect(),
            links: vec![Link {
                label: "Open in roll".into(),
                url: "https://roll.sh/t/1".into(),
            }],
            footer: Some("roll · ada".into()),
        };
        let b = status(&card);
        let blocks = b.as_array().unwrap();
        assert_eq!(blocks.len(), 4);
        let head = blocks[0]["text"]["text"].as_str().unwrap();
        assert!(
            head.starts_with(":large_blue_circle: *Fix &lt;script&gt;"),
            "{head}"
        );
        let lines = blocks[1]["elements"][0]["text"].as_str().unwrap();
        assert_eq!(lines.lines().count(), 8);
        assert!(lines.starts_with("step 4 &lt;@U1&gt;"), "{lines}");
        assert_eq!(blocks[2]["elements"][0]["url"], "https://roll.sh/t/1");
        assert!(!texts(&b).contains("<script>"));
    }

    #[test]
    fn approval_buttons_split_into_actions_blocks() {
        let card = RequestCard {
            title: "Run a command".into(),
            detail: Some("rm -rf <target> ```oops```".into()),
            kind: RequestKind::Approval {
                choices: (0..30).map(choice).collect(),
            },
        };
        let b = request(&card);
        let blocks = b.as_array().unwrap();
        let rows: Vec<&Value> = blocks.iter().filter(|b| b["type"] == "actions").collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r["elements"].as_array().unwrap().len() <= MAX_ACTIONS_ELEMENTS)
        );
        assert_eq!(rows[1]["elements"][4]["action_id"], "roll.choice.29");
        assert_eq!(rows[0]["elements"][0]["style"], "primary");
        assert!(rows[0]["elements"][1].get("style").is_none());
        let detail = blocks[1]["text"]["text"].as_str().unwrap();
        assert!(detail.starts_with("```rm -rf &lt;target&gt;"), "{detail}");
        assert_eq!(detail.matches("```").count(), 2, "{detail}");
    }

    #[test]
    fn long_text_is_cut_to_slack_limits() {
        let long = "<".repeat(5000);
        let card = RequestCard {
            title: long.clone(),
            detail: Some(long.clone()),
            kind: RequestKind::Approval {
                choices: vec![Choice {
                    label: "x".repeat(100),
                    value: "v".into(),
                    style: ChoiceStyle::Danger,
                }],
            },
        };
        let b = request(&card);
        for block in b.as_array().unwrap() {
            if let Some(t) = block["text"]["text"].as_str() {
                assert!(t.chars().count() <= MAX_SECTION_TEXT, "{}", t.len());
                assert!(!t.contains('<'));
                // Cut between entities, never inside one.
                assert!(
                    t.trim_end_matches(['*', '…', '`']).ends_with("&lt;"),
                    "{}",
                    &t[t.len() - 10..]
                );
            }
        }
        let label = b[2]["elements"][0]["text"]["text"].as_str().unwrap();
        assert_eq!(label.chars().count(), MAX_BUTTON_TEXT);
        assert_eq!(b[2]["elements"][0]["style"], "danger");
    }

    #[test]
    fn questions_get_selects_inputs_and_a_submit() {
        let card = RequestCard {
            title: "Two questions".into(),
            detail: None,
            kind: RequestKind::Questions {
                questions: vec![
                    QuestionView {
                        id: "scope".into(),
                        header: "Scope".into(),
                        text: "Which crate?".into(),
                        options: (0..120).map(choice).collect(),
                        multi: false,
                    },
                    QuestionView {
                        id: "files".into(),
                        header: "Files".into(),
                        text: "".into(),
                        options: vec![choice(1)],
                        multi: true,
                    },
                    QuestionView {
                        id: "why".into(),
                        header: "Why".into(),
                        text: "Explain".into(),
                        options: vec![],
                        multi: false,
                    },
                ],
                submit: Choice {
                    label: "Submit".into(),
                    value: "req_1".into(),
                    style: ChoiceStyle::Primary,
                },
            },
        };
        let b = request(&card);
        let blocks = b.as_array().unwrap();
        assert_eq!(blocks[1]["block_id"], "roll.qb.scope");
        assert_eq!(blocks[1]["accessory"]["type"], "static_select");
        assert_eq!(blocks[1]["accessory"]["action_id"], "roll.q.scope");
        assert_eq!(
            blocks[1]["accessory"]["options"].as_array().unwrap().len(),
            MAX_OPTIONS
        );
        assert_eq!(blocks[2]["accessory"]["type"], "multi_static_select");
        assert_eq!(blocks[3]["type"], "input");
        assert_eq!(blocks[3]["dispatch_action"], false);
        assert_eq!(blocks[3]["element"]["type"], "plain_text_input");
        assert_eq!(blocks[3]["element"]["action_id"], "roll.q.why");
        let submit = &blocks[4]["elements"][0];
        assert_eq!(
            (submit["action_id"].as_str(), submit["value"].as_str()),
            (Some("roll.submit"), Some("req_1"))
        );
    }

    #[test]
    fn many_questions_stay_under_fifty_blocks_with_submit_last() {
        let questions = (0..80)
            .map(|i| QuestionView {
                id: format!("q{i}"),
                header: "h".into(),
                text: "t".into(),
                options: vec![choice(0)],
                multi: false,
            })
            .collect();
        let card = RequestCard {
            title: "Many".into(),
            detail: Some("d".into()),
            kind: RequestKind::Questions {
                questions,
                submit: choice(9),
            },
        };
        let b = request(&card);
        let blocks = b.as_array().unwrap();
        assert_eq!(blocks.len(), MAX_BLOCKS);
        assert_eq!(
            blocks.last().unwrap()["elements"][0]["action_id"],
            "roll.submit"
        );
    }

    #[test]
    fn answered_has_no_actions() {
        let card = RequestCard {
            title: "Run".into(),
            detail: None,
            kind: RequestKind::Answered {
                by: "<@U1>".into(),
                answer: "Allow".into(),
            },
        };
        let b = request(&card);
        assert!(b.as_array().unwrap().iter().all(|b| b["type"] != "actions"));
        assert_eq!(
            b[1]["elements"][0]["text"],
            "Answered by &lt;@U1&gt;: Allow"
        );
    }

    #[test]
    fn modal_shape() {
        let m = text_modal(
            "roll.answer",
            "r1:q1",
            "Answer the agent's question",
            "Your answer",
        );
        assert_eq!(m["type"], "modal");
        assert_eq!(m["private_metadata"], "r1:q1");
        assert!(m["title"]["text"].as_str().unwrap().chars().count() <= MAX_MODAL_TITLE);
        assert_eq!(m["blocks"][0]["block_id"], "roll.text");
        assert_eq!(m["blocks"][0]["element"]["action_id"], "roll.text");
        assert_eq!(m["blocks"][0]["element"]["multiline"], true);
    }
}
