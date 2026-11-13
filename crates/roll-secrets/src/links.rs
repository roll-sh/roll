//! Links (docs/secrets.md, "Links"): a secret whose value comes from a roll resource in the same
//! project. roll-secrets knows no product: each kind is a [`LinkProvider`] that rolld registers
//! (`crates/rolld/src/secrets.rs`), calling the product's own API.

use crate::Value;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The shortest a dynamic credential lives: one that ends before a process has started serving
/// isn't worth issuing, and renewals stay calm.
pub const TTL_MIN: u64 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Read from the product on every resolve.
    Live,
    /// Issued once by the product, stored, rotated on demand or on a schedule.
    Managed,
    /// Issued per resolve with a TTL, as a lease.
    Dynamic,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Managed => "managed",
            Mode::Dynamic => "dynamic",
        }
    }
}

/// A secret's link, as stored and as the API shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub kind: String,
    pub r#ref: String,
    pub field: String,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub options: Map<String, Json>,
}

impl Link {
    /// What two links compare by in the grid: equal links give equal values.
    pub fn fingerprint(&self) -> String {
        format!("link:{}:{}:{}", self.kind, self.r#ref, self.field)
    }

    pub fn option_u64(&self, name: &str) -> Option<u64> {
        self.options.get(name).and_then(Json::as_u64)
    }

    /// Minutes a dynamic credential lives (default 60, at most 7 days).
    pub fn ttl_minutes(&self) -> u64 {
        self.option_u64("ttl_minutes").unwrap_or(60).clamp(TTL_MIN, 7 * 24 * 60)
    }

    /// Minutes the previous managed credential keeps working after a rotation (default 60).
    pub fn grace_minutes(&self) -> u64 {
        self.option_u64("grace_minutes").unwrap_or(60).min(7 * 24 * 60)
    }

    pub fn rotate_days(&self) -> Option<u64> {
        self.option_u64("rotate_days").filter(|d| *d > 0)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub field: &'static str,
    pub label: &'static str,
    pub modes: &'static [Mode],
    /// Whether the value is a credential (masked) or plain connection detail.
    pub secret: bool,
    /// The key the dashboard suggests (`DATABASE_URL`, `AWS_ACCESS_KEY_ID`).
    pub suggest: &'static str,
    /// Issued fields that come from one credential share its name (`key`: an access key's id
    /// and secret). Secrets of one environment linked to the same resource and credential share
    /// one issue: rotating one rotates them all, in one change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<&'static str>,
}

/// A credential a provider issued: its parts by field (`access_key_id`, `secret_access_key`),
/// and what it needs to revoke it later (an access key's id, a role's name; never a secret).
#[derive(Debug)]
pub struct Issued {
    pub parts: BTreeMap<String, Value>,
    pub handle: Json,
}

/// One kind of linkable resource. Calls block (they run on the store's blocking threads).
pub trait LinkProvider: Send + Sync {
    fn kind(&self) -> &str;
    fn fields(&self) -> &[Field];
    /// The fields a project usually links for one of these resources, and how (a credential of
    /// its own, managed, wherever the product issues one): what `…/suggestions` offers.
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[]
    }
    /// A resource's display name, if the product knows it.
    fn name(&self, r#ref: &str) -> Option<String> {
        let _ = r#ref;
        None
    }
    /// The current value of a `live` field.
    fn read(&self, r#ref: &str, field: &str, link: &Link) -> Result<Value>;
    /// A new credential for a `managed` or `dynamic` field's credential, labelled `label` in
    /// the product. `ttl_minutes` is set for a dynamic one (the product may enforce it too).
    fn issue(&self, r#ref: &str, credential: &str, link: &Link, label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let _ = (r#ref, label, link, ttl_minutes);
        Err(Error::BadRequest(format!("{} can't issue {credential} credentials", self.kind())))
    }
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let _ = (r#ref, handle);
        Ok(())
    }
    /// Keep a dynamic credential working until `until` (unix ms): a lease was renewed. Only a
    /// product that enforces an expiry of its own needs this (a Postgres role's `VALID UNTIL`);
    /// for the rest, roll's lease is the expiry.
    fn renew(&self, r#ref: &str, handle: &Json, until: i64) -> Result<()> {
        let _ = (r#ref, handle, until);
        Ok(())
    }
    /// Rotate the resource's own credential (a database password), which `live` fields then
    /// read. `Ok(false)`: this kind has nothing to rotate.
    fn rotate_source(&self, r#ref: &str) -> Result<bool> {
        let _ = r#ref;
        Ok(false)
    }
}

/// The providers rolld registered, by kind.
#[derive(Clone, Default)]
pub struct Links(BTreeMap<String, Arc<dyn LinkProvider>>);

impl std::fmt::Debug for Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

impl Links {
    pub fn register(&mut self, p: Arc<dyn LinkProvider>) {
        self.0.insert(p.kind().to_string(), p);
    }

    pub fn get(&self, kind: &str) -> Result<&Arc<dyn LinkProvider>> {
        self.0.get(kind).ok_or_else(|| Error::BadRequest(format!("{kind} resources can't be linked on this server")))
    }

    /// The field's description, if the provider has it.
    pub fn field(&self, l: &Link) -> Option<&Field> {
        self.0.get(&l.kind)?.fields().iter().find(|f| f.field == l.field)
    }

    /// The credential a managed or dynamic link draws from (`None` for live links).
    pub fn credential(&self, l: &Link) -> Option<&'static str> {
        if l.mode == Mode::Live {
            return None;
        }
        self.field(l).map(|f| f.credential.unwrap_or(f.field))
    }

    pub fn kinds(&self) -> impl Iterator<Item = &Arc<dyn LinkProvider>> {
        self.0.values()
    }

    /// A link the provider can serve: a known kind, field and mode.
    pub fn check(&self, l: &Link) -> Result<()> {
        let p = self.get(&l.kind)?;
        let f = p
            .fields()
            .iter()
            .find(|f| f.field == l.field)
            .ok_or_else(|| Error::BadRequest(format!("{} has no field {:?}: one of {}", l.kind, l.field, p.fields().iter().map(|f| f.field).collect::<Vec<_>>().join(", "))))?;
        if !f.modes.contains(&l.mode) {
            return Err(Error::BadRequest(format!("{} {} is {}, not {}", l.kind, l.field, f.modes.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(" or "), l.mode.as_str())));
        }
        for k in l.options.keys() {
            if !["ttl_minutes", "grace_minutes", "rotate_days", "branch", "permission"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown link option {k}")));
            }
        }
        Ok(())
    }
}

/// A linkable resource of a project and its fields (`GET …/links`).
#[derive(Debug, Serialize)]
pub struct Target {
    pub kind: String,
    pub r#ref: String,
    pub name: String,
    pub fields: Vec<Field>,
}

impl crate::Secrets {
    /// The project's resources a provider can link, with their fields.
    pub fn link_targets(&self, project: &str, caller: &crate::Caller) -> Result<Vec<Target>> {
        caller.need(crate::Action::Read, &crate::Scope::project(project))?;
        self.project(project)?;
        let members = self.members(project)?;
        let p = self.workspace.get(roll_workspace::Scope::All, project).map_err(|e| Error::Internal(e.to_string()))?;
        let mut out: Vec<Target> = p
            .resources
            .into_iter()
            .filter(|r| members.contains(&(r.kind.as_str().to_string(), r.r#ref.clone())))
            .filter_map(|r| {
                let provider = self.links.0.get(r.kind.as_str())?;
                Some(Target { kind: r.kind.as_str().into(), name: provider.name(&r.r#ref).unwrap_or_else(|| r.r#ref.clone()), r#ref: r.r#ref, fields: provider.fields().to_vec() })
            })
            .collect();
        if let Some(provider) = self.links.0.get(crate::rotators::KIND) {
            out.extend(self.rotator_refs(project)?.into_iter().map(|(id, name)| Target { kind: crate::rotators::KIND.into(), r#ref: id, name, fields: provider.fields().to_vec() }));
        }
        if let Some(provider) = self.links.0.get(crate::outside::KIND) {
            out.extend(self.outside_db_refs(project)?.into_iter().map(|(id, name)| Target { kind: crate::outside::KIND.into(), r#ref: id, name, fields: provider.fields().to_vec() }));
        }
        Ok(out)
    }
}
