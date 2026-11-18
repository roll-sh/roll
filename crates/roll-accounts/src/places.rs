//! Mail on a sign-in from somewhere new (GitHub's "a new sign-in to your account", docs/accounts.md
//! "New sign-ins"): the first full sign-in from a browser and network a person hasn't signed in
//! from before mails them, so a stolen code or link shows up in their inbox, not only in their
//! security log. A person's very first sign-in (and the first after this shipped) mails nothing.

use crate::audit::Who;
use crate::{Accounts, Mail, Result, sha256_hex};
use code_sql::params;
use std::net::IpAddr;

/// The places kept per person (the oldest go).
const KEPT: i64 = 50;

/// A browser in words, as the dashboard's sessions list says it ("Chrome on macOS").
pub(crate) fn device_name(ua: &str) -> String {
    let name = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.starts_with("roll/") || ua.starts_with("curl/") {
        "A command line"
    } else {
        "A browser"
    };
    let os = if ua.contains("iPhone") || ua.contains("iPad") {
        "iOS"
    } else if ua.contains("Mac OS X") {
        "macOS"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() { name.to_string() } else { format!("{name} on {os}") }
}

/// The network an address is in: its /24 (IPv4) or /48 (IPv6), so a new address from the same
/// home or office isn't "somewhere new".
pub(crate) fn network_of(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
        }
    }
}

impl Accounts {
    /// A full sign-in happened: remember where from, and mail the person if it's somewhere new.
    pub(crate) fn note_sign_in_place(&self, user: &str, who: &Who) {
        if let Err(e) = self.note_place(user, who) {
            tracing::warn!(error = %e, "accounts: noting a sign-in's place failed");
        }
    }

    fn note_place(&self, user: &str, who: &Who) -> Result<()> {
        let device = device_name(who.user_agent.as_deref().unwrap_or(""));
        let network = who.ip.map(network_of).unwrap_or_default();
        let place = sha256_hex(&format!("{device}\n{network}"));
        let now = self.db.now_ms()?;
        let new_place = self.db.write(|c| {
            let known: i64 = c.query_row("SELECT COUNT(*) FROM sign_in_places WHERE user_id = ?", [user], |r| r.get(0))?;
            if c.execute("UPDATE sign_in_places SET last_at = ? WHERE user_id = ? AND place = ?", params![now, user, place])? == 1 {
                return Ok::<_, crate::Error>(false);
            }
            c.execute("INSERT INTO sign_in_places (user_id, place, first_at, last_at) VALUES (?, ?, ?, ?) ON CONFLICT (user_id, place) DO UPDATE SET last_at = excluded.last_at", params![user, place, now, now])?;
            c.execute(
                "DELETE FROM sign_in_places WHERE user_id = ? AND place NOT IN (SELECT place FROM sign_in_places WHERE user_id = ? ORDER BY last_at DESC LIMIT ?)",
                params![user, user, KEPT],
            )?;
            Ok(known > 0)
        })?;
        if !new_place || !self.mail_on() {
            return Ok(());
        }
        let u = self.user(user)?;
        let from = match who.ip {
            Some(ip) => format!("{device}, from {ip}"),
            None => device,
        };
        let text = format!(
            "Someone signed in to your roll account ({}) from somewhere you haven't signed in from before: {from}.\n\nIf it was you, there's nothing to do.\n\nIf it wasn't: end that session and check what changed at {}/dashboard/account#activity, then tell an admin.\n",
            u.email, self.config.web_url
        );
        let mail = Mail { to: u.email, subject: "A new sign-in to your roll account".into(), text };
        let mailer = self.mailer.clone();
        tokio::spawn(async move {
            match mailer.send(&mail).await {
                Ok(()) => tracing::info!(what = "new sign-in", "accounts: mail sent"),
                Err(e) => tracing::warn!(what = "new sign-in", error = %e, "accounts: mail failed"),
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_name_a_browser_and_its_network() {
        assert_eq!(device_name("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15"), "Safari on macOS");
        assert_eq!(device_name("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36 Edg/131.0"), "Edge on Windows");
        assert_eq!(device_name(""), "A browser");
        assert_eq!(network_of("203.0.113.7".parse().unwrap()), "203.0.113.0/24");
        assert_eq!(network_of("2001:db8:1:2::9".parse().unwrap()), "2001:db8:1::/48");
    }
}
