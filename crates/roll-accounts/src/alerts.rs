//! Security alerts to admins (docs/accounts.md, "Admin alerts"), as Google Workspace's alert
//! center and Okta's admin notifications send them: when someone is made an admin or loses it, an
//! admin's account is turned off, an admin invite is made, or how people sign in changes (the
//! account policy, allowed networks, hardware keys only, an SSO connection or its SCIM token),
//! every other admin gets a mail saying what changed and who did it. A stolen admin session that
//! makes another account an admin, or quietly turns enforcement off, is seen at once. The admin who made the
//! change isn't mailed; the operator's token is named as such.

use crate::{Accounts, Mail, Result};

impl Accounts {
    /// The mails telling every admin but `by` (an admin's id, or `operator`) that `what` happened,
    /// to at most 50 of them.
    pub(crate) fn alert_mails(&self, by: &str, what: &str) -> Result<Vec<Mail>> {
        if !self.mail_on() {
            return Ok(Vec::new());
        }
        let who = match by {
            "operator" => "the operator's token".to_string(),
            id => self.user(id).map(|u| format!("{} ({})", u.handle, u.email)).unwrap_or_else(|_| id.to_string()),
        };
        let to: Vec<String> = self.db.read(|c| c.query("SELECT email FROM users WHERE admin = 1 AND disabled = 0 AND deleted_at IS NULL AND id <> ? ORDER BY created_at LIMIT 50", [by], |r| r.get::<_, String>(0)))?;
        let web = &self.config.web_url;
        let mut said = what.to_string();
        if let Some(first) = said.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        Ok(to
            .into_iter()
            .map(|to| Mail {
                to,
                subject: format!("roll security: {what}"),
                text: format!(
                    "{said}, by {who}.\n\nIf you didn't expect it, check what else changed in roll's audit log ({web}/dashboard/access/audit?product=accounts) and the People section of your account page ({web}/dashboard/account).\n\nEvery roll admin gets this mail, except whoever made the change.\n"
                ),
            })
            .collect())
    }

    /// An SSO connection's name for an alert (its id if it's gone).
    pub(crate) fn sso_name(&self, id: &str) -> String {
        self.sso_connections().ok().and_then(|all| all.into_iter().find(|c| c.id == id)).map_or_else(|| id.to_string(), |c| c.name)
    }
}
