//! Discord rendering of [`crate::card`] views, and interaction responses. Text is cut to Discord's
//! limits (one field over fails the whole message), and messages set `allowed_mentions` to none
//! so agent text can't ping `@everyone` or anyone else.
//!
//! A component's `custom_id` is all that comes back on a click, and holds ≤ 100 characters:
//!
//! - An approval button's `custom_id` is the choice's value, cut to 100 bytes.
//! - A question's select is `roll.q:<submit value>:<question id>`, and a free-text question's
//!   Answer button (which opens [`respond::modal`]) is `roll.a:<submit value>:<question id>`. The
//!   question id is kept whole and the submit value is cut to fit, so keep
//!   `len(submit value) + len(question id) ≤ 92` for both to round-trip. [`question_id`] splits
//!   them back.
//! - The Submit button's `custom_id` is the submit value.

use crate::card::{Choice, ChoiceStyle, QuestionView, RequestCard, RequestKind, StatusCard, Tone};
use crate::{cut_bytes, truncate};
use serde_json::{Value, json};

pub const MAX_CONTENT: usize = 2000;
pub const MAX_EMBED_TITLE: usize = 256;
pub const MAX_EMBED_DESCRIPTION: usize = 4096;
pub const MAX_EMBED_FOOTER: usize = 2048;
pub const MAX_EMBED_FIELDS: usize = 25;
pub const MAX_FIELD_NAME: usize = 256;
pub const MAX_FIELD_VALUE: usize = 1024;
pub const MAX_CUSTOM_ID: usize = 100;
pub const MAX_BUTTON_LABEL: usize = 80;
pub const MAX_ROWS: usize = 5;
pub const MAX_BUTTONS_PER_ROW: usize = 5;
pub const MAX_SELECT_OPTIONS: usize = 25;
pub const MAX_OPTION_LABEL: usize = 100;
pub const MAX_OPTION_VALUE: usize = 100;
pub const MAX_PLACEHOLDER: usize = 150;
pub const MAX_URL: usize = 512;
const MAX_MODAL_TITLE: usize = 45;
const MAX_INPUT_LABEL: usize = 45;

pub const QUESTION_PREFIX: &str = "roll.q:";
pub const ANSWER_PREFIX: &str = "roll.a:";

const BLUE: u32 = 0x4c8dff;
const GREY: u32 = 0x8f8f8f;
const DARK_GREY: u32 = 0x5c5c5c;
/// roll's danger red (`--danger`): failure only.
const RED: u32 = 0xff3b30;

fn colour(tone: Tone) -> u32 {
    match tone {
        Tone::Running | Tone::Waiting => BLUE,
        Tone::Done => GREY,
        Tone::Failed => RED,
        Tone::Info => DARK_GREY,
    }
}

fn custom_id(prefix: &str, submit: &str, question: &str) -> String {
    let question = cut_bytes(question, MAX_CUSTOM_ID - prefix.len() - 1);
    let room = MAX_CUSTOM_ID - prefix.len() - 1 - question.len();
    format!("{prefix}{}:{question}", cut_bytes(submit, room))
}

/// The `custom_id` of a question's select.
pub fn question_custom_id(submit: &str, question: &str) -> String {
    custom_id(QUESTION_PREFIX, submit, question)
}

/// The `custom_id` of a free-text question's Answer button (and of the modal it opens).
pub fn answer_custom_id(submit: &str, question: &str) -> String {
    custom_id(ANSWER_PREFIX, submit, question)
}

/// `(submit value, question id)` from a select's or Answer button's `custom_id`.
pub fn question_id(custom_id: &str) -> Option<(&str, &str)> {
    custom_id
        .strip_prefix(QUESTION_PREFIX)
        .or_else(|| custom_id.strip_prefix(ANSWER_PREFIX))?
        .rsplit_once(':')
}

/// A code block for `detail`, with triple backticks inside broken up so they can't end it.
fn code(detail: &str, max: usize) -> String {
    let body = detail.trim_matches('\n').replace("```", "`\u{200b}``");
    format!("```\n{}\n```", truncate(&body, max - 8))
}

fn link_rows(card: &StatusCard) -> Vec<Value> {
    let buttons: Vec<Value> = card
        .links
        .iter()
        // Discord refuses the message over one long URL; drop the link instead.
        .filter(|l| l.url.len() <= MAX_URL)
        .take(MAX_ROWS * MAX_BUTTONS_PER_ROW)
        .map(|l| json!({ "type": 2, "style": 5, "label": truncate(&l.label, MAX_BUTTON_LABEL), "url": l.url }))
        .collect();
    rows(buttons)
}

fn rows(buttons: Vec<Value>) -> Vec<Value> {
    buttons
        .chunks(MAX_BUTTONS_PER_ROW)
        .map(|chunk| json!({ "type": 1, "components": chunk }))
        .collect()
}

fn no_mentions() -> Value {
    json!({ "parse": [] })
}

/// An agent thread's status: a message with one embed (title, status, the newest step lines,
/// footer; coloured by tone) and link buttons.
pub fn status(card: &StatusCard) -> Value {
    let mut description = format!("**{}**", truncate(&card.status, 500));
    let lines = card.shown_lines();
    if !lines.is_empty() {
        description.push_str("\n\n");
        description.push_str(&lines.join("\n"));
    }
    let mut embed = json!({
        "title": truncate(&card.title, MAX_EMBED_TITLE),
        "description": truncate(&description, MAX_EMBED_DESCRIPTION),
        "color": colour(card.tone),
    });
    if let Some(footer) = card.footer.as_deref().filter(|f| !f.trim().is_empty()) {
        embed["footer"] = json!({ "text": truncate(footer, MAX_EMBED_FOOTER) });
    }
    json!({
        "embeds": [embed],
        "components": link_rows(card),
        "allowed_mentions": no_mentions(),
    })
}

fn button_style(style: ChoiceStyle) -> u8 {
    match style {
        ChoiceStyle::Primary => 1,
        ChoiceStyle::Default => 2,
        ChoiceStyle::Danger => 4,
    }
}

fn button(c: &Choice, custom_id: &str) -> Value {
    json!({
        "type": 2,
        "style": button_style(c.style),
        "label": truncate(&c.label, MAX_BUTTON_LABEL),
        "custom_id": cut_bytes(custom_id, MAX_CUSTOM_ID),
    })
}

fn select(q: &QuestionView, submit: &str) -> Value {
    let options: Vec<Value> = q
        .options
        .iter()
        .take(MAX_SELECT_OPTIONS)
        .map(|o| json!({ "label": truncate(&o.label, MAX_OPTION_LABEL), "value": cut_bytes(&o.value, MAX_OPTION_VALUE) }))
        .collect();
    let placeholder = if q.header.trim().is_empty() {
        &q.text
    } else {
        &q.header
    };
    let mut s = json!({
        "type": 3,
        "custom_id": question_custom_id(submit, &q.id),
        "placeholder": truncate(placeholder, MAX_PLACEHOLDER),
        "options": options,
    });
    if q.multi {
        s["min_values"] = json!(1);
        s["max_values"] = json!(options.len());
    }
    json!({ "type": 1, "components": [s] })
}

/// A request the agent waits on: an embed with the title and detail (as code), and
///
/// - approval: the choices as buttons (≤ 25);
/// - questions: each question as an embed field, a select per question with options, an Answer
///   button per free-text question, then Submit. Five rows hold at most four selects plus a row
///   of Answer buttons and Submit; questions that don't fit are counted in the description;
/// - answered: who answered and what, and no components (an edit clears the old ones).
pub fn request(card: &RequestCard) -> Value {
    let mut description = card
        .detail
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(|d| code(d, 3000))
        .unwrap_or_default();
    let mut embed = json!({ "title": truncate(&card.title, MAX_EMBED_TITLE), "color": BLUE });
    let components = match &card.kind {
        RequestKind::Approval { choices } => {
            let buttons = choices
                .iter()
                .take(MAX_ROWS * MAX_BUTTONS_PER_ROW)
                .map(|c| button(c, &c.value))
                .collect();
            rows(buttons)
        }
        RequestKind::Questions { questions, submit } => {
            let mut selects = Vec::new();
            let mut answers = Vec::new();
            let mut left_out = 0;
            for q in questions {
                if !q.options.is_empty() {
                    if selects.len() < MAX_ROWS - 1 {
                        selects.push(select(q, &submit.value));
                    } else {
                        left_out += 1;
                    }
                } else {
                    let label = if q.header.trim().is_empty() {
                        "Answer".to_owned()
                    } else {
                        format!("Answer: {}", q.header.trim())
                    };
                    let answer = Choice {
                        label,
                        value: String::new(),
                        style: ChoiceStyle::Default,
                    };
                    answers.push(button(&answer, &answer_custom_id(&submit.value, &q.id)));
                }
            }
            let room = (MAX_ROWS - selects.len()) * MAX_BUTTONS_PER_ROW - 1;
            if answers.len() > room {
                left_out += answers.len() - room;
                answers.truncate(room);
            }
            answers.push(button(
                &Choice {
                    style: ChoiceStyle::Primary,
                    ..submit.clone()
                },
                &submit.value,
            ));
            let fields: Vec<Value> = questions
                .iter()
                .take(MAX_EMBED_FIELDS)
                .map(|q| {
                    let name = if q.header.trim().is_empty() { "Question" } else { &q.header };
                    let value = if q.text.trim().is_empty() { "\u{200b}" } else { &q.text };
                    json!({ "name": truncate(name, MAX_FIELD_NAME), "value": truncate(value, MAX_FIELD_VALUE) })
                })
                .collect();
            embed["fields"] = json!(fields);
            if left_out > 0 {
                let s = if left_out == 1 { "" } else { "s" };
                description.push_str(&format!("\n{left_out} more question{s} to answer in roll."));
            }
            selects.into_iter().chain(rows(answers)).collect()
        }
        RequestKind::Answered { by, answer } => {
            embed["color"] = json!(GREY);
            description.push_str(&format!("\nAnswered by {by}: {answer}"));
            Vec::new()
        }
    };
    let description = description.trim();
    if !description.is_empty() {
        embed["description"] = json!(truncate(description, MAX_EMBED_DESCRIPTION));
    }
    json!({ "embeds": [embed], "components": components, "allowed_mentions": no_mentions() })
}

/// Interaction responses: the body answering the interaction's POST, within 3 seconds.
pub mod respond {
    use super::*;

    /// Answer to [`super::super::Interaction::Ping`].
    pub fn pong() -> Value {
        json!({ "type": 1 })
    }

    /// A new message in the channel; `ephemeral` shows it only to the person who acted.
    pub fn message(content: &str, ephemeral: bool) -> Value {
        let mut data =
            json!({ "content": truncate(content, MAX_CONTENT), "allowed_mentions": no_mentions() });
        if ephemeral {
            data["flags"] = json!(64);
        }
        json!({ "type": 4, "data": data })
    }

    /// Acknowledge a component click without changing the message now (edit it later).
    pub fn deferred_update() -> Value {
        json!({ "type": 6 })
    }

    /// Replace the clicked message with `payload` (from [`status`] or [`request`]).
    pub fn update(payload: Value) -> Value {
        json!({ "type": 7, "data": payload })
    }

    /// A modal with one paragraph text input, `custom_id` `roll.text`. The submit comes back as a
    /// modal submit with this `custom_id`.
    pub fn modal(custom_id: &str, title: &str, label: &str) -> Value {
        json!({
            "type": 9,
            "data": {
                "custom_id": cut_bytes(custom_id, MAX_CUSTOM_ID),
                "title": truncate(title, MAX_MODAL_TITLE),
                "components": [{
                    "type": 1,
                    "components": [{
                        "type": 4,
                        "custom_id": "roll.text",
                        "label": truncate(label, MAX_INPUT_LABEL),
                        "style": 2,
                        "required": true,
                    }],
                }],
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Link;

    fn choice(i: usize) -> Choice {
        Choice {
            label: format!("Choice {i}"),
            value: format!("v{i}"),
            style: ChoiceStyle::Default,
        }
    }

    fn count_components(p: &Value) -> (usize, Vec<usize>) {
        let rows = p["components"].as_array().unwrap();
        (
            rows.len(),
            rows.iter()
                .map(|r| r["components"].as_array().unwrap().len())
                .collect(),
        )
    }

    #[test]
    fn status_embed_and_links() {
        let card = StatusCard {
            title: "t".repeat(400),
            tone: Tone::Running,
            status: "Running · 3 steps".into(),
            lines: (0..10).map(|i| format!("step {i}")).collect(),
            links: vec![
                Link {
                    label: "Open in roll".into(),
                    url: "https://roll.sh/t/1".into(),
                },
                Link {
                    label: "PR".into(),
                    url: format!("https://x/{}", "a".repeat(600)),
                },
            ],
            footer: Some("roll".into()),
        };
        let p = status(&card);
        let embed = &p["embeds"][0];
        assert_eq!(embed["color"], BLUE);
        assert_eq!(
            embed["title"].as_str().unwrap().chars().count(),
            MAX_EMBED_TITLE
        );
        let d = embed["description"].as_str().unwrap();
        assert!(d.starts_with("**Running · 3 steps**\n\nstep 2"), "{d}");
        assert_eq!(embed["footer"]["text"], "roll");
        assert_eq!(count_components(&p), (1, vec![1]));
        let link = &p["components"][0]["components"][0];
        assert_eq!(
            (link["style"].as_u64(), link.get("custom_id")),
            (Some(5), None)
        );
        assert_eq!(p["allowed_mentions"]["parse"], json!([]));
        let failed = status(&StatusCard {
            tone: Tone::Failed,
            ..card
        });
        assert_eq!(failed["embeds"][0]["color"], RED);
    }

    #[test]
    fn approval_buttons_respect_rows() {
        let mut choices: Vec<Choice> = (0..30).map(choice).collect();
        choices[0].style = ChoiceStyle::Danger;
        choices[1].value = "x".repeat(150);
        choices[2].label = "l".repeat(120);
        let p = request(&RequestCard {
            title: "Run".into(),
            detail: Some("cargo test ```x```".into()),
            kind: RequestKind::Approval { choices },
        });
        let (rows, per_row) = count_components(&p);
        assert_eq!(rows, MAX_ROWS);
        assert!(per_row.iter().all(|n| *n <= MAX_BUTTONS_PER_ROW));
        let first = &p["components"][0]["components"];
        assert_eq!(first[0]["style"], 4);
        assert_eq!(first[0]["custom_id"], "v0");
        assert_eq!(first[1]["custom_id"].as_str().unwrap().len(), MAX_CUSTOM_ID);
        assert_eq!(
            first[2]["label"].as_str().unwrap().chars().count(),
            MAX_BUTTON_LABEL
        );
        let d = p["embeds"][0]["description"].as_str().unwrap();
        assert!(d.starts_with("```\ncargo test"), "{d}");
        assert_eq!(d.matches("```").count(), 2, "{d}");
    }

    #[test]
    fn questions_selects_answers_and_submit() {
        let mut questions: Vec<QuestionView> = (0..6)
            .map(|i| QuestionView {
                id: format!("q{i}"),
                header: format!("Q{i}"),
                text: "pick".into(),
                options: (0..40).map(choice).collect(),
                multi: i == 0,
            })
            .collect();
        questions.push(QuestionView {
            id: "why".into(),
            header: "Why".into(),
            text: "say".into(),
            options: vec![],
            multi: false,
        });
        let submit = Choice {
            label: "Submit".into(),
            value: "req_1".into(),
            style: ChoiceStyle::Default,
        };
        let p = request(&RequestCard {
            title: "Qs".into(),
            detail: None,
            kind: RequestKind::Questions { questions, submit },
        });
        let (rows, per_row) = count_components(&p);
        assert_eq!(rows, MAX_ROWS);
        assert_eq!(per_row, vec![1, 1, 1, 1, 2]);
        let first = &p["components"][0]["components"][0];
        assert_eq!(first["custom_id"], "roll.q:req_1:q0");
        assert_eq!(
            first["options"].as_array().unwrap().len(),
            MAX_SELECT_OPTIONS
        );
        assert_eq!(first["max_values"], 25);
        assert!(
            p["components"][1]["components"][0]
                .get("max_values")
                .is_none()
        );
        let last = &p["components"][4]["components"];
        assert_eq!(last[0]["custom_id"], "roll.a:req_1:why");
        assert_eq!(
            (last[1]["custom_id"].as_str(), last[1]["style"].as_u64()),
            (Some("req_1"), Some(1))
        );
        assert_eq!(p["embeds"][0]["fields"].as_array().unwrap().len(), 7);
        assert!(
            p["embeds"][0]["description"]
                .as_str()
                .unwrap()
                .contains("2 more questions")
        );
    }

    #[test]
    fn question_custom_ids_fit_and_split() {
        assert_eq!(question_id("roll.q:req_1:scope"), Some(("req_1", "scope")));
        assert_eq!(question_id("roll.a:a:b:why"), Some(("a:b", "why")));
        assert_eq!(question_id("req_1"), None);
        let long = question_custom_id(&"s".repeat(200), "scope");
        assert_eq!(long.len(), MAX_CUSTOM_ID);
        assert_eq!(question_id(&long).unwrap().1, "scope");
        assert!(answer_custom_id("r", &"q".repeat(200)).len() <= MAX_CUSTOM_ID);
    }

    #[test]
    fn answered_has_no_components() {
        let p = request(&RequestCard {
            title: "Run".into(),
            detail: None,
            kind: RequestKind::Answered {
                by: "ada".into(),
                answer: "Allow".into(),
            },
        });
        assert_eq!(p["components"], json!([]));
        assert_eq!(p["embeds"][0]["description"], "Answered by ada: Allow");
    }

    #[test]
    fn responses() {
        assert_eq!(respond::pong(), json!({"type": 1}));
        let m = respond::message(&"x".repeat(3000), true);
        assert_eq!(
            (m["type"].as_u64(), m["data"]["flags"].as_u64()),
            (Some(4), Some(64))
        );
        assert_eq!(
            m["data"]["content"].as_str().unwrap().chars().count(),
            MAX_CONTENT
        );
        assert!(respond::message("hi", false)["data"].get("flags").is_none());
        assert_eq!(respond::deferred_update(), json!({"type": 6}));
        assert_eq!(respond::update(json!({"content": "x"}))["type"], 7);
        let modal = respond::modal(
            "roll.a:req_1:why",
            "Answer the agent's question, please now",
            "Your answer to the question here",
        );
        assert_eq!(modal["type"], 9);
        let input = &modal["data"]["components"][0]["components"][0];
        assert_eq!(
            (
                input["type"].as_u64(),
                input["style"].as_u64(),
                input["custom_id"].as_str()
            ),
            (Some(4), Some(2), Some("roll.text"))
        );
        assert!(modal["data"]["title"].as_str().unwrap().chars().count() <= 45);
        assert!(input["label"].as_str().unwrap().chars().count() <= 45);
    }
}
