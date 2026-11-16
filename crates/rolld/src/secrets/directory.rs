//! LDAP and Active Directory servers outside roll (roll-secrets' directories.rs). The address is
//! the tenant's, so it's handled as outside databases are (outside.rs): rolld resolves the host,
//! refuses a private address unless private ones are allowed, and connects to the address it
//! checked, or a gateway dials it (docs/secrets.md, "Gateways"). Always over TLS: `ldaps://`, or
//! `ldap://` with StartTLS (a password, and Active Directory's `unicodePwd`, never go in plain
//! text), from here to the server. The connection goes through a small relay (relay.rs) that stops
//! after 1 MiB from the server, and the whole conversation has 15 seconds.

use ldap3::exop::PasswordModify;
use ldap3::{Ldap, LdapConnAsync, LdapConnSettings, LdapError, Mod, Scope, SearchEntry, SearchOptions, StdStream};
use roll_secrets::directories::{AccountNames, Bind, Flavor};
use super::outside::Via;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// One conversation, connecting included.
const TIMEOUT: Duration = Duration::from_secs(15);
/// The most a conversation reads from the server: its answers are a handful of small results.
const MAX_READ: u64 = 1 << 20;
/// The Password Modify extended operation (RFC 3062).
const PASSMOD: &str = "1.3.6.1.4.1.4203.1.11.1";

/// The provider roll-secrets calls: a directory behind a gateway runs where the gateway's link is
/// ([`super::gateway::Gateways::call`]); any other runs here.
pub(crate) struct Guarded {
    pub allow_private: bool,
    pub gateways: Arc<super::gateway::Gateways>,
}

impl Guarded {
    fn here(&self) -> Engine {
        Engine { via: Via::Direct { allow_private: self.allow_private } }
    }
}

impl roll_secrets::directories::Directory for Guarded {
    fn check(&self, bind: &Bind<'_>, accounts: &[String], groups: &[String], user_base_dn: Option<&str>) -> Result<(), String> {
        use super::gateway::{Call, DirOp};
        match bind.gateway {
            Some(g) => self.gateways.call(g, Call::dir(bind, DirOp::Check { accounts: accounts.to_vec(), groups: groups.to_vec(), base: user_base_dn.map(Into::into) })).map(drop),
            None => self.here().check(bind, accounts, groups, user_base_dn),
        }
    }

    fn set_password(&self, bind: &Bind<'_>, dn: &str, password: &str) -> Result<(), String> {
        use super::gateway::{Call, DirOp};
        match bind.gateway {
            Some(g) => self.gateways.call(g, Call::dir(bind, DirOp::SetPassword { dn: dn.into(), password: password.into() })).map(drop),
            None => self.here().set_password(bind, dn, password),
        }
    }

    fn create_user(&self, bind: &Bind<'_>, dn: &str, password: &str, groups: &[String], description: &str, until: i64) -> Result<(), String> {
        use super::gateway::{Call, DirOp};
        match bind.gateway {
            Some(g) => self.gateways.call(g, Call::dir(bind, DirOp::CreateUser { dn: dn.into(), password: password.into(), groups: groups.to_vec(), description: description.into(), until })).map(drop),
            None => self.here().create_user(bind, dn, password, groups, description, until),
        }
    }

    fn extend_user(&self, bind: &Bind<'_>, dn: &str, until: i64) -> Result<(), String> {
        use super::gateway::{Call, DirOp};
        match bind.gateway {
            Some(g) => self.gateways.call(g, Call::dir(bind, DirOp::ExtendUser { dn: dn.into(), until })).map(drop),
            None => self.here().extend_user(bind, dn, until),
        }
    }

    fn delete_user(&self, bind: &Bind<'_>, dn: &str, groups: &[String]) -> Result<(), String> {
        use super::gateway::{Call, DirOp};
        match bind.gateway {
            Some(g) => self.gateways.call(g, Call::dir(bind, DirOp::DeleteUser { dn: dn.into(), groups: groups.to_vec() })).map(drop),
            None => self.here().delete_user(bind, dn, groups),
        }
    }

    fn account_names(&self, bind: &Bind<'_>, dns: &[String]) -> Result<Vec<AccountNames>, String> {
        use super::gateway::{Call, DirOp};
        match bind.gateway {
            Some(g) => {
                let v = self.gateways.call(g, Call::dir(bind, DirOp::AccountNames { dns: dns.to_vec() }))?;
                let pairs: Vec<(String, String)> = serde_json::from_value(v).map_err(|_| "the gateway's node answered names roll doesn't read".to_string())?;
                Ok(pairs.into_iter().map(|(account_name, upn)| AccountNames { account_name, upn }).collect())
            }
            None => self.here().account_names(bind, dns),
        }
    }
}

/// One conversation's way to the directory.
pub(crate) struct Engine {
    pub via: Via,
}

impl roll_secrets::directories::Directory for Engine {
    fn check(&self, bind: &Bind<'_>, accounts: &[String], groups: &[String], user_base_dn: Option<&str>) -> Result<(), String> {
        let dns: Vec<String> = accounts.iter().chain(groups).cloned().chain(user_base_dn.map(str::to_string)).collect();
        self.run(bind, async move |l| {
            for dn in &dns {
                if !exists(l, dn).await? {
                    return Err(format!("there's no {dn}"));
                }
            }
            Ok(())
        })
    }

    fn set_password(&self, bind: &Bind<'_>, dn: &str, password: &str) -> Result<(), String> {
        let flavor = bind.flavor;
        self.run(bind, async move |l| {
            // Active Directory keeps taking an account's previous password for an hour after a
            // reset (NTLM's old password allowed period, which Samba applies to simple binds as
            // well): a throwaway password first makes the previous one a password nobody knows.
            if flavor == Flavor::ActiveDirectory {
                set_password(l, flavor, dn, &throwaway()).await?;
            }
            set_password(l, flavor, dn, password).await
        })
    }

    fn create_user(&self, bind: &Bind<'_>, dn: &str, password: &str, groups: &[String], description: &str, until: i64) -> Result<(), String> {
        let flavor = bind.flavor;
        let cn = dn.strip_prefix("cn=").and_then(|r| r.split(',').next()).filter(|c| !c.is_empty()).ok_or("a user's DN starts cn=")?.to_string();
        self.run(bind, async move |l| {
            match flavor {
                Flavor::Openldap => {
                    let attrs = vec![attr("objectClass", &["top", "person", "organizationalPerson", "inetOrgPerson"]), attr("cn", &[&cn]), attr("sn", &["roll"]), attr("description", &[description])];
                    l.add(dn, attrs).await.and_then(|r| r.success()).map_err(ldap_error)?;
                    set_password(l, flavor, dn, password).await?;
                }
                Flavor::ActiveDirectory => {
                    let names = AccountNames::dynamic(dn);
                    // Made disabled, then given its password, then enabled (AD refuses an enabled
                    // account without a password that meets its policy).
                    let attrs = vec![
                        attr("objectClass", &["top", "person", "organizationalPerson", "user"]),
                        attr("cn", &[&cn]),
                        attr("sAMAccountName", &[&names.account_name]),
                        attr("userPrincipalName", &[&names.upn]),
                        attr("description", &[description]),
                        attr("userAccountControl", &["514"]),
                        attr("accountExpires", &[&filetime(until)]),
                    ];
                    l.add(dn, attrs).await.and_then(|r| r.success()).map_err(ldap_error)?;
                    set_password(l, flavor, dn, password).await?;
                    l.modify(dn, vec![Mod::Replace(b"userAccountControl".to_vec(), set(b"512"))]).await.and_then(|r| r.success()).map_err(ldap_error)?;
                }
            }
            for g in groups {
                l.modify(g, vec![Mod::Add(b"member".to_vec(), set(dn.as_bytes()))]).await.and_then(|r| r.success()).map_err(|e| format!("adding it to {g}: {}", ldap_error(e)))?;
            }
            Ok(())
        })
    }

    fn extend_user(&self, bind: &Bind<'_>, dn: &str, until: i64) -> Result<(), String> {
        // Only Active Directory has an expiry for an account; elsewhere the lease is the expiry.
        if bind.flavor != Flavor::ActiveDirectory {
            return Ok(());
        }
        let at = filetime(until);
        self.run(bind, async move |l| l.modify(dn, vec![Mod::Replace(b"accountExpires".to_vec(), set(at.as_bytes()))]).await.and_then(|r| r.success()).map(|_| ()).map_err(ldap_error))
    }

    fn delete_user(&self, bind: &Bind<'_>, dn: &str, groups: &[String]) -> Result<(), String> {
        let flavor = bind.flavor;
        self.run(bind, async move |l| {
            // OpenLDAP keeps a deleted member's DN in its groups (without the refint overlay):
            // take it out first, as far as the server lets (a group's last member can't go).
            // Active Directory does this itself.
            if flavor == Flavor::Openldap {
                for g in groups {
                    let _ = l.modify(g, vec![Mod::Delete(b"member".to_vec(), set(dn.as_bytes()))]).await;
                }
            }
            match l.delete(dn).await.map_err(ldap_error)? {
                r if r.rc == 0 || r.rc == 32 => Ok(()),
                r => Err(ldap_error(LdapError::from(r))),
            }
        })
    }

    fn account_names(&self, bind: &Bind<'_>, dns: &[String]) -> Result<Vec<AccountNames>, String> {
        self.run(bind, async move |l| {
            let mut out = Vec::with_capacity(dns.len());
            for dn in dns {
                let (entries, _) = l.with_search_options(SearchOptions::new().sizelimit(1).timelimit(10)).search(dn, Scope::Base, "(objectClass=*)", vec!["sAMAccountName", "userPrincipalName"]).await.and_then(|r| r.success()).map_err(ldap_error)?;
                let e = entries.into_iter().next().map(SearchEntry::construct).ok_or_else(|| format!("there's no {dn}"))?;
                let one = |a: &str| e.attrs.get(a).and_then(|v| v.first()).cloned();
                let account_name = one("sAMAccountName").ok_or_else(|| format!("{dn} has no sAMAccountName"))?;
                let upn = one("userPrincipalName").unwrap_or_else(|| format!("{account_name}@{}", roll_secrets::directories::domain(dn)));
                out.push(AccountNames { account_name, upn });
            }
            Ok(out)
        })
    }
}

impl Engine {
    /// Bind as the directory's bind account, then `work`, all within [`TIMEOUT`].
    fn run<T: Send>(&self, bind: &Bind<'_>, work: impl AsyncFnOnce(&mut Ldap) -> Result<T, String>) -> Result<T, String> {
        let (url, host, port, starttls) = parse(bind.url)?;
        let tls = Arc::new(super::outside::tls_config(bind.ca)?);
        let via = self.via.clone();
        let go = async move {
            let remote = via.dial(&host, port).await?;
            let stream = super::relay::pinned(remote, MAX_READ).await.map_err(|e| format!("{host}: {e}"))?;
            let settings = LdapConnSettings::new().set_conn_timeout(TIMEOUT).set_config(tls).set_starttls(starttls).set_std_stream(StdStream::Tcp(stream));
            let (conn, mut l) = LdapConnAsync::with_settings(settings, &url).await.map_err(|e| format!("{host}: {}", ldap_error(e)))?;
            tokio::spawn(tokio::time::timeout(TIMEOUT, conn.drive()));
            l.simple_bind(bind.bind_dn, bind.bind_password).await.and_then(|r| r.success()).map_err(|e| format!("binding as {}: {}", bind.bind_dn, ldap_error(e)))?;
            let out = work(&mut l).await;
            let _ = l.unbind().await;
            out
        };
        tokio::runtime::Handle::current().block_on(async move { tokio::time::timeout(TIMEOUT, go).await.map_err(|_| "timed out".to_string())? })
    }
}

/// `ldaps://host[:port]` or `ldap://host[:port]` (roll-secrets checked the shape): the URL as
/// ldap3 takes it, the host, the port, and whether StartTLS comes first.
fn parse(url: &str) -> Result<(String, String, u16, bool), String> {
    let (rest, starttls, default) = match url.strip_prefix("ldaps://") {
        Some(r) => (r, false, 636),
        None => (url.strip_prefix("ldap://").ok_or("the URL is ldaps://host or ldap://host")?, true, 389),
    };
    let rest = rest.trim_end_matches('/');
    let (host, port) = match rest.strip_prefix('[') {
        Some(v6) => {
            let (h, after) = v6.split_once(']').ok_or("a bad IPv6 address")?;
            (h, after.strip_prefix(':'))
        }
        None => match rest.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        },
    };
    let port = port.map(|p| p.parse::<u16>().map_err(|_| "a bad port".to_string())).transpose()?.unwrap_or(default);
    if host.is_empty() || rest.contains(['/', '?', '@']) {
        return Err("the URL is ldaps://host or ldap://host".into());
    }
    Ok((url.trim_end_matches('/').to_string(), host.to_string(), port, starttls))
}

/// Whether `dn` exists (a base search for no attributes).
async fn exists(l: &mut Ldap, dn: &str) -> Result<bool, String> {
    let r = l.with_search_options(SearchOptions::new().sizelimit(1).timelimit(10)).search(dn, Scope::Base, "(objectClass=*)", vec!["1.1"]).await.map_err(ldap_error)?;
    match r.1.rc {
        0 => Ok(!r.0.is_empty()),
        32 => Ok(false),
        _ => Err(ldap_error(LdapError::from(r.1))),
    }
}

/// Replace `dn`'s password as an administrator. Active Directory: `unicodePwd`, the password in
/// double quotes as UTF-16LE (the connection is TLS). OpenLDAP: the Password Modify extended
/// operation where the server has it (it hashes as configured), else `userPassword`.
async fn set_password(l: &mut Ldap, flavor: Flavor, dn: &str, password: &str) -> Result<(), String> {
    match flavor {
        Flavor::ActiveDirectory => {
            let quoted: Vec<u8> = format!("\"{password}\"").encode_utf16().flat_map(u16::to_le_bytes).collect();
            l.modify(dn, vec![Mod::Replace(b"unicodePwd".to_vec(), HashSet::from([quoted]))]).await.and_then(|r| r.success()).map(|_| ()).map_err(ldap_error)
        }
        Flavor::Openldap if passmod(l).await? => l.extended(PasswordModify { user_id: Some(dn), old_pass: None, new_pass: Some(password) }).await.and_then(|r| r.success()).map(|_| ()).map_err(ldap_error),
        Flavor::Openldap => l.modify(dn, vec![Mod::Replace(b"userPassword".to_vec(), set(password.as_bytes()))]).await.and_then(|r| r.success()).map(|_| ()).map_err(ldap_error),
    }
}

/// Whether the root DSE lists the Password Modify extended operation.
async fn passmod(l: &mut Ldap) -> Result<bool, String> {
    let (entries, _) = l.with_search_options(SearchOptions::new().sizelimit(1).timelimit(10)).search("", Scope::Base, "(objectClass=*)", vec!["supportedExtension"]).await.and_then(|r| r.success()).map_err(ldap_error)?;
    Ok(entries.into_iter().next().map(SearchEntry::construct).is_some_and(|e| e.attrs.get("supportedExtension").is_some_and(|v| v.iter().any(|o| o == PASSMOD))))
}

/// A password nobody keeps, with each kind of character Active Directory's complexity rules ask for.
fn throwaway() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| format!("{x:02x}")).collect::<String>() + "Aa1!"
}

fn attr(name: &str, values: &[&str]) -> (Vec<u8>, HashSet<Vec<u8>>) {
    (name.as_bytes().to_vec(), values.iter().map(|v| v.as_bytes().to_vec()).collect())
}

fn set(v: &[u8]) -> HashSet<Vec<u8>> {
    HashSet::from([v.to_vec()])
}

/// Unix ms as a Windows FILETIME (100 ns since 1601), Active Directory's `accountExpires`.
fn filetime(ms: i64) -> String {
    (ms.max(0) * 10_000 + 116_444_736_000_000_000).to_string()
}

/// The server's answer, or what went wrong on the way (never a password: ldap3's errors hold
/// result codes, the server's text and I/O errors).
fn ldap_error(e: LdapError) -> String {
    match e {
        LdapError::LdapResult { result } if result.text.is_empty() => format!("result {}", result.rc),
        LdapError::LdapResult { result } => format!("{} (result {})", result.text.trim(), result.rc),
        e => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tenants_directory_is_reached_over_tls_at_a_public_address_only() {
        assert_eq!(parse("ldaps://dc1.example.com").unwrap(), ("ldaps://dc1.example.com".into(), "dc1.example.com".into(), 636, false));
        assert_eq!(parse("ldap://ldap.example.com:1389/").unwrap(), ("ldap://ldap.example.com:1389".into(), "ldap.example.com".into(), 1389, true), "ldap:// always starts with StartTLS");
        assert_eq!(parse("ldaps://[2001:db8::1]:636").unwrap().1, "2001:db8::1");
        assert!(parse("ldapi:///run/slapd.sock").is_err());
        let g = Engine { via: Via::Direct { allow_private: false } };
        let bind = Bind { url: "ldaps://127.0.0.1:636", flavor: Flavor::Openldap, bind_dn: "cn=admin,dc=example,dc=com", bind_password: "fake-pw", ca: None, gateway: None };
        let refused = tokio::task::spawn_blocking(move || roll_secrets::directories::Directory::check(&g, &bind, &[], &[], None)).await.unwrap();
        assert!(refused.unwrap_err().contains("private"));
        // unicodePwd is the quoted password as UTF-16LE.
        assert_eq!("\"Ab1!\"".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>(), b"\"\0A\0b\x001\0!\0\"\0");
        assert_eq!(filetime(0), "116444736000000000");
    }

    /// Against a real OpenLDAP with TLS, run by hand (docs/secrets.md, "Directories"):
    /// `ROLL_E2E_LDAP_URL=ldaps://127.0.0.1:PORT ROLL_E2E_LDAP_CA=ca.pem ROLL_E2E_LDAP_BIND=cn=roll,…
    /// ROLL_E2E_LDAP_PASSWORD=… ROLL_E2E_LDAP_BASE=dc=example,dc=com cargo test -p rolld real_openldap -- --ignored`,
    /// with `ou=svc` holding `cn=app-blue`, `ou=roll` and `ou=groups` holding `cn=readers`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_openldap_sets_passwords_and_makes_and_deletes_users() {
        use roll_secrets::directories::Directory;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let (url, ca, bind_dn, pw, base) = (env("ROLL_E2E_LDAP_URL"), std::fs::read_to_string(env("ROLL_E2E_LDAP_CA")).unwrap(), env("ROLL_E2E_LDAP_BIND"), env("ROLL_E2E_LDAP_PASSWORD"), env("ROLL_E2E_LDAP_BASE"));
        tokio::task::spawn_blocking(move || {
            let g = Engine { via: Via::Direct { allow_private: true } };
            let at = |dn: &'static str| format!("{dn},{base}");
            let (blue, users, group) = (at("cn=app-blue,ou=svc"), at("ou=roll"), at("cn=readers,ou=groups"));
            let admin = Bind { url: &url, flavor: Flavor::Openldap, bind_dn: &bind_dn, bind_password: &pw, ca: Some(&ca), gateway: None };
            let as_user = |dn: &str, password: &str| g.check(&Bind { url: &url, flavor: Flavor::Openldap, bind_dn: dn, bind_password: password, ca: Some(&ca), gateway: None }, &[], &[], None);
            // The bind proof, and a refusal for what isn't there.
            g.check(&admin, std::slice::from_ref(&blue), std::slice::from_ref(&group), Some(&users)).unwrap();
            assert!(g.check(&admin, &[at("cn=nobody,ou=svc")], &[], None).unwrap_err().contains("there's no"));
            assert!(g.check(&Bind { bind_password: "fake-wrong", ..admin }, &[], &[], None).unwrap_err().contains("49"));
            // A managed account's new password works and its old one doesn't.
            let first = roll_secrets_password();
            g.set_password(&admin, &blue, &first).unwrap();
            as_user(&blue, &first).unwrap();
            let second = roll_secrets_password();
            g.set_password(&admin, &blue, &second).unwrap();
            as_user(&blue, &second).unwrap();
            assert!(as_user(&blue, &first).is_err(), "the old password is refused");
            // A dynamic user: made in the group, binds, then is gone (twice: gone counts as deleted).
            let user = format!("cn=roll-lse_fakefakefakefake,{users}");
            let upw = roll_secrets_password();
            g.create_user(&admin, &user, &upw, std::slice::from_ref(&group), "roll secrets lease lse_fakefakefakefake (test)", 0).unwrap();
            as_user(&user, &upw).unwrap();
            assert!(member_of(&g, &admin, &group, &user), "it joined the group");
            g.delete_user(&admin, &user, std::slice::from_ref(&group)).unwrap();
            assert!(as_user(&user, &upw).is_err() && !member_of(&g, &admin, &group, &user), "deleted and out of the group");
            g.delete_user(&admin, &user, std::slice::from_ref(&group)).unwrap();
        })
        .await
        .unwrap();
    }

    /// Against a real Active Directory, run by hand (docs/secrets.md, "Directories"; checked with
    /// Samba's AD DC): `ROLL_E2E_AD_URL=ldaps://dc1.samdom.localhost:PORT ROLL_E2E_AD_CA=ca.pem
    /// ROLL_E2E_AD_BIND=CN=roll-bind,CN=Users,… ROLL_E2E_AD_PASSWORD=… ROLL_E2E_AD_BASE=DC=samdom,DC=localhost
    /// cargo test -p rolld real_active_directory -- --ignored`, with `OU=svc` holding `CN=app-blue`,
    /// `OU=roll` and `OU=groups` holding `CN=readers`, the bind account holding only the
    /// `active_directory` row's rights.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_real_active_directory_sets_passwords_and_makes_expires_and_deletes_users() {
        use roll_secrets::directories::Directory;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let (url, ca, bind_dn, pw, base) = (env("ROLL_E2E_AD_URL"), std::fs::read_to_string(env("ROLL_E2E_AD_CA")).unwrap(), env("ROLL_E2E_AD_BIND"), env("ROLL_E2E_AD_PASSWORD"), env("ROLL_E2E_AD_BASE"));
        tokio::task::spawn_blocking(move || {
            let g = Engine { via: Via::Direct { allow_private: true } };
            let at = |dn: &'static str| format!("{dn},{base}");
            let (blue, users, group) = (at("CN=app-blue,OU=svc"), at("OU=roll"), at("CN=readers,OU=groups"));
            let admin = Bind { url: &url, flavor: Flavor::ActiveDirectory, bind_dn: &bind_dn, bind_password: &pw, ca: Some(&ca), gateway: None };
            let as_user = |dn: &str, password: &str| g.check(&Bind { url: &url, flavor: Flavor::ActiveDirectory, bind_dn: dn, bind_password: password, ca: Some(&ca), gateway: None }, &[], &[], None);
            // The bind proof, a refusal for what isn't there, and a certificate from a CA nobody trusts.
            g.check(&admin, std::slice::from_ref(&blue), std::slice::from_ref(&group), Some(&users)).unwrap();
            assert!(g.check(&admin, &[at("CN=nobody,OU=svc")], &[], None).unwrap_err().contains("there's no"));
            assert!(g.check(&Bind { bind_password: "fake-wrong", ..admin }, &[], &[], None).unwrap_err().contains("49"));
            let untrusted = g.check(&Bind { ca: None, ..admin }, &[], &[], None).unwrap_err();
            assert!(untrusted.to_lowercase().contains("certificate"), "{untrusted}");
            // A managed account's new password works and its old one doesn't.
            let first = roll_secrets_password();
            g.set_password(&admin, &blue, &first).unwrap();
            as_user(&blue, &first).unwrap();
            let second = roll_secrets_password();
            g.set_password(&admin, &blue, &second).unwrap();
            as_user(&blue, &second).unwrap();
            assert!(as_user(&blue, &first).is_err(), "the old password is refused");
            // Its other names (no userPrincipalName set: the implicit one) log in too.
            let names = g.account_names(&admin, std::slice::from_ref(&blue)).unwrap();
            assert_eq!(names[0].account_name, "app-blue");
            as_user(&names[0].upn, &second).unwrap();
            // A dynamic user: made enabled, in the group, expiring, binds; renewing moves its
            // expiry; then it's gone (twice: gone counts as deleted).
            let user = format!("cn=roll-lse_fakefakefakefake,{users}");
            let upw = roll_secrets_password();
            let until = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64) + 3_600_000;
            g.create_user(&admin, &user, &upw, std::slice::from_ref(&group), "roll secrets lease lse_fakefakefakefake (test)", until).unwrap();
            as_user(&user, &upw).unwrap();
            let names = AccountNames::dynamic(&user);
            assert_eq!(g.account_names(&admin, std::slice::from_ref(&user)).unwrap(), vec![names.clone()]);
            as_user(&names.upn, &upw).unwrap();
            assert!(member_of(&g, &admin, &group, &user), "it joined the group");
            assert_eq!(attribute(&g, &admin, &user, "userAccountControl").as_deref(), Some("512"), "enabled");
            assert_eq!(attribute(&g, &admin, &user, "sAMAccountName").as_deref(), Some("roll-fakefakefakefak"));
            assert_eq!(attribute(&g, &admin, &user, "accountExpires"), Some(filetime(until)));
            g.extend_user(&admin, &user, until + 3_600_000).unwrap();
            assert_eq!(attribute(&g, &admin, &user, "accountExpires"), Some(filetime(until + 3_600_000)), "renewing moves the expiry");
            g.delete_user(&admin, &user, std::slice::from_ref(&group)).unwrap();
            assert!(as_user(&user, &upw).is_err() && !member_of(&g, &admin, &group, &user), "deleted and out of the group");
            g.delete_user(&admin, &user, std::slice::from_ref(&group)).unwrap();
            // An account that expired is refused.
            g.create_user(&admin, &user, &upw, &[], "roll secrets lease lse_fakefakefakefake (test)", 1_000).unwrap();
            assert!(as_user(&user, &upw).is_err(), "an expired account can't bind");
            g.delete_user(&admin, &user, &[]).unwrap();
        })
        .await
        .unwrap();
    }

    fn attribute(g: &Engine, admin: &Bind<'_>, dn: &str, name: &str) -> Option<String> {
        let (dn, name) = (dn.to_string(), name.to_string());
        g.run(admin, async move |l| {
            let (entries, _) = l.search(&dn, Scope::Base, "(objectClass=*)", vec![name.as_str()]).await.and_then(|r| r.success()).map_err(ldap_error)?;
            Ok(entries.into_iter().map(SearchEntry::construct).find_map(|e| e.attrs.get(&name).and_then(|v| v.first().cloned())))
        })
        .unwrap()
    }

    fn roll_secrets_password() -> String {
        let mut b = [0u8; 18];
        getrandom::fill(&mut b).unwrap();
        b.iter().map(|x| format!("{x:02x}")).collect::<String>() + "Aa1"
    }

    fn member_of(g: &Engine, admin: &Bind<'_>, group: &str, dn: &str) -> bool {
        let (group, dn) = (group.to_string(), dn.to_string());
        g.run(admin, async move |l| {
            let (entries, _) = l.search(&group, Scope::Base, "(objectClass=*)", vec!["member"]).await.and_then(|r| r.success()).map_err(ldap_error)?;
            Ok(entries.into_iter().map(SearchEntry::construct).any(|e| e.attrs.get("member").is_some_and(|m| m.iter().any(|v| v.eq_ignore_ascii_case(&dn)))))
        })
        .unwrap()
    }
}
