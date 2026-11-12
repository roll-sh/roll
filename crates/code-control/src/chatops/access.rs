//! roll's access approvals in chat (docs/permissions.md, "Approvals in chat"): a request a scoped
//! token may only `ask` for is posted to the Slack channel the operator chose, as a card with
//! Approve and Deny, and edited when it's decided. A press arrives like every Slack action
//! (verified, recorded once, handled by a worker): only the install's members decide, and the
//! decision goes to roll's access service through the hook rolld sets ([`Code::on_access_decision`]),
//! which also checks the install is the one approvals go to.

use super::{Install, install_by_id};
use crate::Code;
use code_chat::card::{Choice, ChoiceStyle, RequestCard, RequestKind};
use code_chat::slack::Client;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A press on an approval card, for roll's access service to decide.
#[derive(Clone, Debug)]
pub struct AccessDecision {
    pub approval: String,
    pub approve: bool,
    /// Who pressed it: `slack:<user id>`.
    pub by: String,
    /// The install the card was pressed in.
    pub install: String,
}

/// Decides an approval; `Err` is what the person who pressed is told.
pub type AccessDecider = Arc<dyn Fn(AccessDecision) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// An approval as a chat card.
#[derive(Clone, Debug)]
pub struct AccessCard {
    /// `apr_…`
    pub approval: String,
    /// "ci wants to delete postgres prj_x".
    pub title: String,
    /// What would run: the route, the tool, the arguments (redacted).
    pub detail: String,
    /// Delete or deploy: Approve is styled as dangerous.
    pub destructive: bool,
    /// Decided: who, and what ("approved", "denied", "expired").
    pub decided: Option<(String, String)>,
}

/// A button's value: `x:<approval>:<y|n>` (≤ 100 bytes, every platform's limit).
fn value(approval: &str, approve: bool) -> String {
    format!("x:{approval}:{}", if approve { "y" } else { "n" })
}

/// What a button's value names: the approval and whether it approves.
pub(crate) fn parse(v: &str) -> Option<(&str, bool)> {
    let (approval, pick) = v.strip_prefix("x:")?.rsplit_once(':')?;
    let approve = match pick {
        "y" => true,
        "n" => false,
        _ => return None,
    };
    approval.starts_with("apr_").then_some((approval, approve))
}

fn card(c: &AccessCard) -> RequestCard {
    let kind = match &c.decided {
        Some((by, answer)) => RequestKind::Answered { by: by.clone(), answer: answer.clone() },
        None => RequestKind::Approval {
            choices: vec![
                Choice { label: "Approve".into(), value: value(&c.approval, true), style: if c.destructive { ChoiceStyle::Danger } else { ChoiceStyle::Primary } },
                Choice { label: "Deny".into(), value: value(&c.approval, false), style: ChoiceStyle::Default },
            ],
        },
    };
    RequestCard { title: c.title.clone(), detail: Some(c.detail.clone()), kind }
}

fn message(channel: &str, c: &AccessCard) -> code_chat::slack::Message {
    let rc = card(c);
    code_chat::slack::Message { channel: channel.into(), text: rc.text(), blocks: Some(code_chat::slack::blocks::request(&rc)), thread_ts: None }
}

impl Code {
    /// Where presses on approval cards go (rolld: roll's access service). Set once.
    pub fn on_access_decision(&self, f: AccessDecider) {
        let _ = self.access_decider.set(f);
    }

    fn slack_install(&self, install: &str) -> Result<(&Client, String), String> {
        let Some((_, client)) = self.slack() else { return Err("Slack isn't configured on this node".into()) };
        let i = install_by_id(self, install).map_err(|e| e.to_string())?.ok_or_else(|| format!("no chat install {install}"))?;
        if i.platform != super::Platform::Slack {
            return Err("approvals go to Slack installs for now".into());
        }
        let token = i.token(self).ok_or("the install's token can't be opened (ROLL_SECRET_KEY)")?;
        Ok((client, token))
    }

    /// Post an approval card; its message id (`ts`).
    pub async fn post_access_card(&self, install: &str, channel: &str, c: &AccessCard) -> Result<String, String> {
        let (client, token) = self.slack_install(install)?;
        client.post_message(&token, &message(channel, c)).await.map(|p| p.ts).map_err(|e| e.to_string())
    }

    /// Edit a posted approval card (decided, used or expired).
    pub async fn update_access_card(&self, install: &str, channel: &str, ts: &str, c: &AccessCard) -> Result<(), String> {
        let (client, token) = self.slack_install(install)?;
        client.update_message(&token, channel, ts, &message(channel, c)).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

/// A press on an approval card: which approval, which button, who, where.
pub(super) struct Press<'a> {
    pub approval: &'a str,
    pub approve: bool,
    pub user: &'a str,
    pub channel: &'a str,
}

/// A press on an approval card in Slack.
pub(super) async fn pressed(code: &Arc<Code>, client: &Client, token: &str, install: &Install, p: Press<'_>) -> super::Handled {
    let Press { approval, approve, user, channel } = p;
    let say = |text: String| async move {
        let _ = client.post_ephemeral(token, channel, user, &text).await;
    };
    if !install.allows(user) {
        say("Only people the roll owner allows can decide this.".into()).await;
        return super::Handled::Done;
    }
    let Some(decide) = code.access_decider.get() else { return super::Handled::Retry("approvals aren't wired on this node".into()) };
    let d = AccessDecision { approval: approval.to_string(), approve, by: format!("slack:{user}"), install: install.id.clone() };
    if let Err(e) = decide(d).await {
        say(e).await;
    }
    // The card itself is edited by roll's access service once the decision is recorded.
    super::Handled::Done
}

#[cfg(test)]
mod tests {
    #[test]
    fn values_round_trip_and_fit() {
        let v = super::value("apr_abcdefghijklmnop", true);
        assert!(v.len() <= 100);
        assert_eq!(super::parse(&v), Some(("apr_abcdefghijklmnop", true)));
        assert_eq!(super::parse("x:apr_a:n"), Some(("apr_a", false)));
        assert_eq!(super::parse("r:card:d0"), None);
        assert_eq!(super::parse("x:notanapproval:y"), None);
    }
}
