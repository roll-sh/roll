//! Moving a service to another project (Render's, Vercel's and Heroku's transfers; docs/apps/
//! README.md, "Moving a service"): `POST /v1/apps/services/{id}/move {project}`, to another
//! project of the same owner. What only means something in its old project is refused first,
//! named: links either way (their credentials were made for this project's consumer), routes
//! either way (by name in the old project), `roll://` references (the old project's secrets), a
//! private network and running sandboxes (on the old project's network). Then, in one write, what
//! it used so far stays the old project's (as when a service is deleted), it joins the new
//! project, and its canvas card moves; every live environment then redeploys its live deployment
//! (a rollback to itself) onto the new project's network and rules (its signing rule, egress,
//! spend limits), and the old project's networks it was the last on go.

use crate::model::LinkKind;
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;

impl Apps {
    /// What keeps `service` in its project: each named, or none.
    async fn move_blockers(self: &Arc<Self>, service: &crate::store::ServiceRow) -> Result<Vec<String>> {
        let (sid, project) = (service.id.clone(), service.project.clone());
        let (own_links, others, envs) = self.db(move |s| Ok((s.links(&sid)?, s.services(Some(&project))?, s.environments(&sid)?))).await?;
        let mut why = Vec::new();
        for (kind, r, _) in &own_links {
            why.push(format!("its link to {} {r}", kind.as_str()));
        }
        for o in others.iter().filter(|o| o.id != service.id) {
            let oid = o.id.clone();
            let (links, oenvs) = self.db(move |s| Ok((s.links(&oid)?, s.environments(&oid)?))).await?;
            if links.iter().any(|(k, r, _)| *k == LinkKind::Service && r == &service.id) {
                why.push(format!("{}'s link to it", o.name));
            }
            if oenvs.iter().any(|e| e.routes.iter().any(|r| r.service == service.name)) {
                why.push(format!("{}'s route to it", o.name));
            }
        }
        for e in &envs {
            if !e.routes.is_empty() {
                why.push(format!("its {} routes to other services", e.name));
            }
            let (sid, en) = (service.id.clone(), e.name.clone());
            let (vars, peering, sandboxes) = self.db(move |s| Ok((s.variables(&sid, &en)?, s.peering(&sid, &en)?, s.sandboxes(Some(&sid), Some(&en), None)?))).await?;
            let refs: Vec<&String> = vars.iter().filter(|(_, v)| crate::model::secret_ref_env(v).is_some()).map(|(k, _)| k).collect();
            if !refs.is_empty() {
                why.push(format!("its {} variables {} (roll:// references to this project's secrets)", e.name, refs.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if peering.is_some() {
                why.push(format!("its {} private network", e.name));
            }
            if !sandboxes.is_empty() {
                why.push(format!("{} running sandboxes in {}", sandboxes.len(), e.name));
            }
        }
        let sid = service.id.clone();
        if self.db(move |s| s.git_login(&sid)).await?.is_some_and(|l| l.secret.as_deref().is_some_and(|s| s.starts_with("roll://"))) {
            why.push("its Git login's roll:// secret".into());
        }
        Ok(why)
    }

    /// `POST /v1/apps/services/{id}/move {project, secrets?}`. `grants`: a scoped token's and a
    /// person's session's, each of which must allow admin on the target project's apps (what
    /// making a service there takes), not only on this service wherever it is.
    pub async fn move_service(self: &Arc<Self>, id: &str, body: Value, grants: [Option<&roll_auth::Grant>; 2], actor: Option<String>) -> Result<Value> {
        let target = body["project"].as_str().filter(|p| !p.is_empty()).ok_or_else(|| Error::BadRequest("project is the project to move it to".into()))?.to_string();
        let sid = id.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        if svc.project == target {
            return Err(Error::Conflict(format!("{} is already in that project", svc.name)));
        }
        let Some(ws) = self.deps.workspace.clone() else { return Err(Error::Conflict("this node has no projects to move a service between".into())) };
        let (from, to) = match (ws.get(roll_workspace::Scope::All, &svc.project), ws.get(roll_workspace::Scope::All, &target)) {
            (Ok(f), Ok(t)) => (f, t),
            (_, Err(roll_workspace::Error::NotFound(_))) => return Err(Error::NotFound(format!("no project {target}"))),
            (Err(e), _) | (_, Err(e)) => return Err(Error::Internal(e.to_string())),
        };
        if from.owner != to.owner {
            return Err(Error::Forbidden("a service moves only to another project of the same owner".into()));
        }
        // Admin on apps in the target project, as making a service there takes: a rule on this
        // service's id alone would match it in any project.
        let there = roll_auth::scope::Resource { product: "apps".into(), id: None, project: Some(target.clone()), ..Default::default() };
        for g in grants.into_iter().flatten() {
            if g.allows(&there, roll_auth::scope::Action::Admin) != roll_auth::scope::Outcome::Allow {
                return Err(Error::Forbidden(format!("moving it needs admin on apps in {}", to.name)));
            }
        }
        let mut blockers = self.move_blockers(&svc).await?;
        // Routes in the target project that name a service of its name would start sending to it.
        let (t, n) = (target.clone(), svc.name.clone());
        let routed: Vec<String> = self
            .db(move |s| {
                let mut out = Vec::new();
                for o in s.services(Some(&t))? {
                    if s.environments(&o.id)?.iter().any(|e| e.routes.iter().any(|r| r.service == n)) {
                        out.push(o.name);
                    }
                }
                Ok(out)
            })
            .await?;
        blockers.extend(routed.into_iter().map(|o| format!("{o}'s route there to a service named {}", svc.name)));
        if !blockers.is_empty() {
            return Err(Error::Conflict(format!("{} can't move while it has {}: remove them, move it, then set them up in {}", svc.name, blockers.join("; "), to.name)));
        }
        // Its environments' secrets are its project's: after the move they're the target's.
        // Named first, and moved only when the caller says so.
        let sid = svc.id.clone();
        let envs = self.db(move |s| s.environments(&sid)).await?;
        let mut secrets = std::collections::BTreeSet::new();
        for e in &envs {
            if let Ok(vars) = self.deps.secrets.resolve(&svc.project, &e.name, &svc.id).await {
                secrets.extend(vars.into_iter().map(|v| v.name));
            }
        }
        if !secrets.is_empty() && body["secrets"] != "switch" {
            return Err(Error::Conflict(format!(
                "its environments get {}'s secrets {}: after the move they get {}'s of those names, or none (move with {{\"secrets\": \"switch\"}} to go ahead)",
                from.name,
                secrets.into_iter().collect::<Vec<_>>().join(", "),
                to.name
            )));
        }
        // An incident its checks opened ends on the old project's status page first.
        for e in &envs {
            for c in &e.checks {
                if self.check_states_of(&svc.id, &e.name, std::slice::from_ref(c)).first().is_some_and(|st| st.up == Some(false)) {
                    self.status_incident(&svc.id, crate::status_page::check_key(&svc.id, &e.name, &c.name), false).await;
                }
            }
        }
        let (sid, old, new, name) = (svc.id.clone(), svc.project.clone(), target.clone(), svc.name.clone());
        self.db(move |s| s.move_service(&sid, &old, &new, &name)).await?;
        // Its card goes along (where it was on the old canvas), and its home: what it was made
        // through is the project it's in now.
        let at = ws.get(roll_workspace::Scope::All, &svc.project).ok().and_then(|p| p.resources.into_iter().find(|r| r.kind == roll_workspace::Kind::Service && r.r#ref == svc.id)).map_or((0.0, 0.0), |r| (r.x, r.y));
        if let Err(e) = ws.place(roll_workspace::Scope::All, &target, roll_workspace::Kind::Service, &svc.id, at.0, at.1) {
            tracing::warn!(service = %svc.id, error = %e, "apps: a moved service's canvas card didn't move");
        }
        if roll_auth::roles::decisions::HOMES
            && let Err(e) = ws.set_home(&roll_workspace::Actor::operator(), &target, roll_workspace::Kind::Service, &svc.id, false)
        {
            tracing::warn!(service = %svc.id, error = %e, "apps: a moved service's home didn't move");
        }
        self.changed(&svc.id);
        // Each live environment onto the new project's network and rules: its live deployment
        // again, never holding later pushes (the old project's networks go when nothing runs on
        // them, by the network sweep, never under instances still serving).
        let mut redeployed = Vec::new();
        let mut failed = Vec::new();
        for e in envs.iter().filter(|e| e.active.is_some()) {
            let active = e.active.clone().unwrap_or_default();
            match self.rollback(&svc.id, &e.name, &active, Some("roll".into()), true, true).await {
                Ok(d) => {
                    let (sid, en, did) = (svc.id.clone(), e.name.clone(), d.id.clone());
                    let _ = self.db(move |s| s.clear_hold_if(&sid, &en, &did)).await;
                    redeployed.push(json!({ "environment": e.name, "deployment": d.id }));
                }
                Err(err) => failed.push(json!({ "environment": e.name, "error": err.to_string() })),
            }
        }
        self.audit("service.moved", &svc.id, "", actor, json!({ "from": svc.project, "to": target })).await;
        Ok(json!({ "id": svc.id, "project": target, "redeployed": redeployed, "failed": failed }))
    }
}
