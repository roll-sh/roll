//! Template APIs: composable (`_index_template`), component (`_component_template`) and legacy
//! (`_template`) templates, plus the simulate APIs.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::resolve::is_wildcard;
use crate::meta::templates::{self, ComponentTemplate, ComposableTemplate, LegacyTemplate};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;
use std::sync::Arc;

/// A reply with `?human` dates added (`created_date` next to `created_date_millis`).
fn human_reply(ctx: &Ctx, mut v: Value) -> EsResult<Reply> {
    if ctx.human {
        crate::units::humanize(&mut v);
    }
    Reply::ok(v)
}

/// `?local` no longer does anything on the get-template APIs.
fn warn_local(ctx: &Ctx) {
    // Compatible-with=8 requests still consume it silently.
    if ctx.has_query("local") && !matches!(ctx.compat, Some(7 | 8)) {
        ctx.warn("the [?local] query parameter to this API has no effect, is now deprecated, and will be removed in a future version");
    }
}

fn name(ctx: &Ctx) -> String {
    ctx.path_param("name").unwrap_or_default().to_string()
}

fn names(ctx: &Ctx) -> Vec<String> {
    ctx.list_param("name").unwrap_or_default()
}

fn selected<'a, T>(all: impl Iterator<Item = (&'a String, &'a T)>, patterns: &[String]) -> Vec<(&'a String, &'a T)>
where
    T: 'a,
{
    all.filter(|(n, _)| patterns.is_empty() || patterns.iter().any(|p| p == "*" || p == "_all" || glob_match(p, n))).collect()
}

pub async fn put_index_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = name(ctx);
    let create = ctx.bool_param("create", false)?;
    let _cause = ctx.query("cause");
    let body = ctx.body()?.unwrap_or_else(|| obj! {});
    let t = ComposableTemplate::parse(&body, ctx.raw_json(), ctx.now_ms)?;
    for (field, shown) in [("created_date_millis", "created_date"), ("modified_date_millis", "modified_date")] {
        if body.get(field).is_some() {
            return Err(EsError::invalid_index_template(&name, &format!("provided a template property which is managed by the system: {shown}")));
        }
    }
    let warnings = ctx
        .cluster
        .update(move |st| {
            let mut t = t;
            if let Some(old) = st.metadata.index_templates.get(&name) {
                t.created_date = old.created_date;
            }
            let w = templates::validate_composable(&st.metadata, &name, &t, create)?;
            st.metadata.index_templates.insert(name, Arc::new(t));
            Ok(w)
        })
        .await?;
    ctx.warn_all(warnings);
    Reply::acknowledged()
}

pub fn get_index_template(ctx: &Ctx) -> EsResult<Reply> {
    warn_local(ctx);
    if ctx.param("name").is_some_and(|n| n.contains(',')) {
        return Err(EsError::illegal_argument("template name may not contain ','"));
    }
    let st = ctx.state();
    let flat = ctx.flat_settings()?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let pattern = ctx.param("name").map(str::to_string);
    let list: Vec<Value> = st
        .metadata
        .index_templates
        .iter()
        .filter(|(n, _)| pattern.as_ref().is_none_or(|p| p == "*" || glob_match(p, n)))
        .map(|(n, t)| {
            let mut v = t.to_value_flat(flat);
            if include_defaults {
                rollover_defaults_at(&st, &mut v);
            }
            obj! { "name" => n.clone(), "index_template" => v }
        })
        .collect();
    if let Some(p) = &pattern
        && list.is_empty()
    {
        if !is_wildcard(p) {
            return Err(EsError::resource_not_found(format!("index template matching [{p}] not found")));
        }
        return Ok(Reply::Body(404, obj! { "index_templates" => Value::Arr(list) }));
    }
    human_reply(ctx, obj! { "index_templates" => Value::Arr(list) })
}

pub async fn delete_index_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let patterns = names(ctx);
    let raw = name(ctx);
    ctx.cluster
        .update(move |st| {
            let matched: Vec<String> = selected(st.metadata.index_templates.iter(), &patterns).into_iter().map(|(n, _)| n.clone()).collect();
            let missing: Vec<&String> = patterns.iter().filter(|p| !is_wildcard(p) && !matched.contains(p)).collect();
            if matched.is_empty() || !missing.is_empty() {
                let shown = if missing.is_empty() { raw.clone() } else { missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",") };
                return Err(EsError::index_template_missing(&shown));
            }
            for n in matched {
                st.metadata.index_templates.remove(&n);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn exists_index_template(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let p = name(ctx);
    let any = st.metadata.index_templates.keys().any(|n| glob_match(&p, n));
    Ok(Reply::Empty(if any { 200 } else { 404 }))
}

pub fn simulate_index(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let index = ctx.path_param("name").unwrap_or_default().to_string();
    let body = ctx.body()?;
    let extra = match body {
        Some(b) => Some(ComposableTemplate::parse(&b, ctx.raw_json(), ctx.now_ms)?),
        None => None,
    };
    let tmp_name = format!("simulate_index_template_{}", crate::meta::names::random_uuid().to_lowercase());
    let mut v = templates::simulate(&st.metadata, Some(&index), extra.as_ref().map(|t| (tmp_name.as_str(), t)))?;
    if ctx.bool_param("include_defaults", false)? {
        rollover_defaults_at(&st, &mut v);
    }
    Reply::ok(v)
}

pub fn simulate_template(ctx: &Ctx) -> EsResult<Reply> {
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let st = ctx.state();
    let mut r = simulate_template_inner(ctx, &st)?;
    if include_defaults && let Reply::Body(_, v) = &mut r {
        rollover_defaults_at(&st, v);
    }
    Ok(r)
}

/// Adds the default rollover conditions to `template.lifecycle` (`include_defaults`).
fn rollover_defaults_at(st: &crate::meta::ClusterState, v: &mut Value) {
    if let Some(l) = v.as_object_mut().and_then(|m| m.get_mut("template")).and_then(|t| t.as_object_mut()).and_then(|t| t.get_mut("lifecycle")) {
        super::data_streams::add_rollover_defaults(st, l);
    }
}

fn simulate_template_inner(ctx: &Ctx, st: &crate::meta::ClusterState) -> EsResult<Reply> {
    let body = ctx.body()?;
    match (ctx.path_param("name"), body) {
        (Some(n), None) => {
            let Some(t) = st.metadata.index_templates.get(n) else {
                return Err(EsError::illegal_argument(format!("unable to simulate template [{n}] that does not exist")));
            };
            Reply::ok(templates::simulate(&st.metadata, None, Some((n, t)))?)
        }
        (n, Some(b)) => {
            let t = ComposableTemplate::parse(&b, ctx.raw_json(), ctx.now_ms)?;
            let tmp = n.map(str::to_string).unwrap_or_else(|| format!("simulate_template_{}", crate::meta::names::random_uuid().to_lowercase()));
            templates::validate_composable(&st.metadata, &tmp, &t, false).or_else(|e| if e.reason().contains("same priority") { Ok(Vec::new()) } else { Err(e) })?;
            Reply::ok(templates::simulate(&st.metadata, None, Some((&tmp, &t)))?)
        }
        (None, None) => Err(EsError::action_request_validation(&["either index name or index template body must be specified for simulation".into()])),
    }
}

pub async fn put_component_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = name(ctx);
    let create = ctx.bool_param("create", false)?;
    let body = ctx.body()?.unwrap_or_else(|| obj! {});
    let t = ComponentTemplate::parse(&body, ctx.raw_json(), ctx.now_ms)?;
    for field in ["created_date_millis", "modified_date_millis"] {
        if body.get(field).is_some() {
            return Err(EsError::action_request_validation(&[format!("Provided a template property which is managed by the system: {field}")]));
        }
    }
    ctx.cluster
        .update(move |st| {
            let mut t = t;
            if let Some(old) = st.metadata.component_templates.get(&name) {
                t.created_date = old.created_date;
            }
            templates::validate_component(&st.metadata, &name, &t, create)?;
            st.metadata.component_templates.insert(name, Arc::new(t));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn get_component_template(ctx: &Ctx) -> EsResult<Reply> {
    warn_local(ctx);
    let st = ctx.state();
    let flat = ctx.flat_settings()?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let pattern = ctx.param("name").map(str::to_string);
    let list: Vec<Value> = st
        .metadata
        .component_templates
        .iter()
        .filter(|(n, _)| pattern.as_ref().is_none_or(|p| p == "*" || p.split(',').any(|q| glob_match(q, n))))
        .map(|(n, t)| {
            let mut v = t.to_value_flat(flat);
            if include_defaults {
                rollover_defaults_at(&st, &mut v);
            }
            obj! { "name" => n.clone(), "component_template" => v }
        })
        .collect();
    if let Some(p) = &pattern
        && list.is_empty()
    {
        if !is_wildcard(p) {
            return Err(EsError::resource_not_found(format!("component template matching [{p}] not found")));
        }
        return Ok(Reply::Body(404, obj! { "component_templates" => Value::Arr(list) }));
    }
    human_reply(ctx, obj! { "component_templates" => Value::Arr(list) })
}

pub async fn delete_component_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let patterns = names(ctx);
    let raw = name(ctx);
    ctx.cluster
        .update(move |st| {
            let matched: Vec<String> = selected(st.metadata.component_templates.iter(), &patterns).into_iter().map(|(n, _)| n.clone()).collect();
            if matched.is_empty() {
                return Err(EsError::resource_not_found(raw.clone()));
            }
            let mut users: Vec<String> =
                st.metadata.index_templates.iter().filter(|(_, t)| t.composed_of.iter().any(|c| matched.contains(c))).map(|(n, _)| n.clone()).collect();
            users.sort();
            if !users.is_empty() {
                let in_use: Vec<&String> =
                    matched.iter().filter(|c| st.metadata.index_templates.values().any(|t| t.composed_of.contains(c))).collect();
                return Err(EsError::illegal_argument(format!(
                    "component templates [{}] cannot be removed as they are still in use by index templates [{}]",
                    in_use.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                    users.join(", ")
                )));
            }
            for n in matched {
                st.metadata.component_templates.remove(&n);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn exists_component_template(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let p = name(ctx);
    let any = st.metadata.component_templates.keys().any(|n| glob_match(&p, n));
    Ok(Reply::Empty(if any { 200 } else { 404 }))
}

pub async fn put_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = name(ctx);
    ctx.warn("Legacy index templates are deprecated in favor of composable templates.");
    let create = ctx.bool_param("create", false)?;
    let order = ctx.query("order").map(|o| o.parse::<i64>()).transpose().map_err(|_| EsError::illegal_argument("Failed to parse int parameter [order]"))?;
    let body = ctx.body_map()?.unwrap_or_default();
    let mut t = LegacyTemplate::parse(&Value::Obj(body))?;
    if let Some(o) = order {
        t.order = o;
    }
    ctx.cluster
        .update(move |st| {
            templates::validate_legacy(&st.metadata, &name, &t, create)?;
            st.metadata.templates.insert(name, Arc::new(t));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn get_template(ctx: &Ctx) -> EsResult<Reply> {
    warn_local(ctx);
    let st = ctx.state();
    let flat = ctx.flat_settings()?;
    let patterns = names(ctx);
    let mut out = Map::new();
    for (n, t) in selected(st.metadata.templates.iter(), &patterns) {
        out.insert(n.clone(), t.to_value(flat));
    }
    if !patterns.is_empty() && out.is_empty() {
        return Ok(Reply::Body(404, Value::Obj(out)));
    }
    Reply::ok(Value::Obj(out))
}

pub async fn delete_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let patterns = names(ctx);
    let raw = name(ctx);
    ctx.cluster
        .update(move |st| {
            let matched: Vec<String> = selected(st.metadata.templates.iter(), &patterns).into_iter().map(|(n, _)| n.clone()).collect();
            if matched.is_empty() {
                return Err(EsError::index_template_missing(&raw));
            }
            for n in matched {
                st.metadata.templates.remove(&n);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn exists_template(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let patterns = names(ctx);
    let any = !selected(st.metadata.templates.iter(), &patterns).is_empty();
    Ok(Reply::Empty(if any { 200 } else { 404 }))
}
