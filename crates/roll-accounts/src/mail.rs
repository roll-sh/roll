//! Mail: SES v2 in the cloud, a directory of JSON files for local development and tests.
//! Nothing here logs a mail's body (it carries codes and invite links).

use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailConfig {
    /// No mail: email sign-in is off, and invites return their link only.
    Off,
    /// Every mail as `<dir>/<ms>-<id>.json` (`ROLL_ACCOUNTS_OUTBOX`).
    Outbox(PathBuf),
    /// SES v2 from this address (`ROLL_ACCOUNTS_MAIL_FROM`), with the SDK's default credentials.
    Ses { from: String },
}

/// A mail to send. `Debug` shows only the recipient.
#[derive(Clone, Serialize)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub text: String,
}

impl std::fmt::Debug for Mail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mail").field("to", &self.to).finish_non_exhaustive()
    }
}

pub enum Mailer {
    Off,
    Outbox(PathBuf),
    Ses { client: aws_sdk_sesv2::Client, from: String },
}

impl Mailer {
    pub async fn new(config: &MailConfig) -> anyhow::Result<Mailer> {
        Ok(match config {
            MailConfig::Off => Mailer::Off,
            MailConfig::Outbox(dir) => {
                std::fs::create_dir_all(dir)?;
                tracing::warn!(dir = %dir.display(), "accounts: mail goes to a local outbox (ROLL_ACCOUNTS_OUTBOX), not to anyone");
                Mailer::Outbox(dir.clone())
            }
            MailConfig::Ses { from } => {
                let aws = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
                Mailer::Ses { client: aws_sdk_sesv2::Client::new(&aws), from: from.clone() }
            }
        })
    }

    pub fn on(&self) -> bool {
        !matches!(self, Mailer::Off)
    }

    pub async fn send(&self, mail: &Mail) -> anyhow::Result<()> {
        match self {
            Mailer::Off => Ok(()),
            Mailer::Outbox(dir) => {
                let name = format!("{}-{}.json", code_sql::now_ms(), crate::new_id("mail"));
                let tmp = dir.join(format!(".{name}"));
                tokio::fs::write(&tmp, serde_json::to_vec(mail)?).await?;
                tokio::fs::rename(&tmp, dir.join(name)).await?;
                Ok(())
            }
            Mailer::Ses { client, from } => {
                use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
                let content = |s: &str| Content::builder().data(s).charset("UTF-8").build();
                let message = Message::builder().subject(content(&mail.subject)?).body(Body::builder().text(content(&mail.text)?).build()).build();
                client
                    .send_email()
                    .from_email_address(from)
                    .destination(Destination::builder().to_addresses(&mail.to).build())
                    .content(EmailContent::builder().simple(message).build())
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("SES: {}", aws_sdk_sesv2::error::DisplayErrorContext(&e)))?;
                Ok(())
            }
        }
    }
}

impl crate::Accounts {
    /// Send `mail` off the request: the answer doesn't wait for (or reveal) it. The log says
    /// `what` went out, never the address or the body.
    pub(crate) fn send_later(self: &std::sync::Arc<Self>, mail: Mail, what: &'static str) {
        let accounts = self.clone();
        tokio::spawn(async move {
            match accounts.mailer.send(&mail).await {
                Ok(()) => tracing::info!(what, "accounts: mail sent"),
                Err(e) => tracing::warn!(what, error = %e, "accounts: mail failed"),
            }
        });
    }

    pub(crate) fn mail_on(&self) -> bool {
        self.mailer.on()
    }
}
