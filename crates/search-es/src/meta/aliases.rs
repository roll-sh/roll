//! Alias definitions (`Alias.fromXContent`) as they appear in create-index bodies, templates and
//! `_aliases` actions, with Elasticsearch's validation texts.

use super::AliasMeta;
use crate::error::EsError;
use crate::json::Value;

/// Query names OpenSearch's plugins register that Elasticsearch doesn't (or registers with a
/// different syntax); filters using them are accepted unchecked.
const OPENSEARCH_QUERIES: &[&str] = &["neural", "neural_sparse", "hybrid", "knn", "template", "agentic", "xy_shape"];

/// The XContent token name Elasticsearch prints for a value (`Unknown token [VALUE_NUMBER]`).
pub fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
    }
}

/// Validates an alias filter: it must parse as a query (`AliasValidator.validateAliasFilter`;
/// the checks against the index's mappings happen when a search uses it). The filter is parsed
/// from its compact JSON form, as Elasticsearch re-reads the stored filter, so error locations
/// refer to that form.
pub fn validate_filter(alias: &str, filter: &Value) -> Result<(), EsError> {
    let failed = || EsError::illegal_argument(format!("failed to parse filter for alias [{alias}]"));
    let Value::Obj(_) = filter else {
        return Err(failed());
    };
    match crate::query::parse_stored(filter) {
        Ok(_) => Ok(()),
        Err(e) => {
            if uses_any(filter, OPENSEARCH_QUERIES) {
                return Ok(());
            }
            Err(failed().caused_by(e))
        }
    }
}

/// Whether any object key in `v` is one of `names`.
fn uses_any(v: &Value, names: &[&str]) -> bool {
    match v {
        Value::Obj(m) => m.iter().any(|(k, x)| names.contains(&k.as_str()) || uses_any(x, names)),
        Value::Arr(a) => a.iter().any(|x| uses_any(x, names)),
        _ => false,
    }
}

/// Parses one alias definition `{"filter": …, "routing": …, …}` (`Alias.fromXContent`).
pub fn parse_alias(name: &str, v: &Value) -> Result<AliasMeta, EsError> {
    let Value::Obj(m) = v else {
        return Err(EsError::illegal_argument("No alias is specified"));
    };
    const KNOWN: &[&str] = &["filter", "routing", "index_routing", "search_routing", "is_write_index", "is_hidden"];
    let mut a = AliasMeta::default();
    for (k, val) in m {
        if !KNOWN.contains(&k.as_str()) {
            return Err(EsError::illegal_argument(format!("Unknown field [{k}] in alias [{name}]")));
        }
        match val {
            Value::Obj(f) => {
                if k == "filter" {
                    if f.is_empty() {
                        a.filter = None;
                    } else {
                        validate_filter(name, val)?;
                        a.filter = Some(val.clone());
                    }
                }
            }
            Value::Str(s) => match k.as_str() {
                "routing" => {
                    a.index_routing = Some(s.clone());
                    a.search_routing = Some(s.clone());
                }
                "index_routing" => a.index_routing = Some(s.clone()),
                "search_routing" => a.search_routing = Some(s.clone()),
                _ => {
                    return Err(EsError::illegal_argument(format!("Unsupported String type value [{s}] for field [{k}] in alias [{name}]")));
                }
            },
            Value::Bool(b) => match k.as_str() {
                "is_write_index" => a.is_write_index = Some(*b),
                "is_hidden" => a.is_hidden = Some(*b),
                _ => {
                    return Err(EsError::illegal_argument(format!("Unsupported boolean type value [{b}] for field [{k}] in alias [{name}]")));
                }
            },
            other => return Err(EsError::illegal_argument(format!("Unknown token [{}] in alias [{name}]", token_name(other)))),
        }
    }
    if let Some(ir) = &a.index_routing
        && ir.contains(',')
    {
        return Err(EsError::illegal_argument(format!("alias [{name}] has several index routing values associated with it")));
    }
    Ok(a)
}

/// Parses an `aliases` object (`{"name": {…}, …}`), validating names.
pub fn parse_aliases(v: &Value) -> Result<Vec<(String, AliasMeta)>, EsError> {
    let mut out = Vec::new();
    let Value::Obj(m) = v else {
        return Err(EsError::parse("key [aliases] must be an object"));
    };
    for (name, def) in m {
        // Date math names (`<logs-{now/d}>`) are checked once resolved.
        if !(name.starts_with('<') && name.ends_with('>')) {
            super::names::validate_alias_name(name)?;
        }
        out.push((name.clone(), parse_alias(name, def)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{arr, obj};

    #[test]
    fn texts() {
        assert_eq!(parse_alias("a", &obj! { "foo" => 1 }).unwrap_err().reason(), "Unknown field [foo] in alias [a]");
        assert_eq!(parse_alias("a", &obj! { "routing" => 1 }).unwrap_err().reason(), "Unknown token [VALUE_NUMBER] in alias [a]");
        assert_eq!(parse_alias("a", &obj! { "filter" => "x" }).unwrap_err().reason(), "Unsupported String type value [x] for field [filter] in alias [a]");
        assert_eq!(parse_alias("a", &obj! { "routing" => true }).unwrap_err().reason(), "Unsupported boolean type value [true] for field [routing] in alias [a]");
        assert_eq!(parse_alias("a", &obj! { "routing" => arr![1] }).unwrap_err().reason(), "Unknown token [START_ARRAY] in alias [a]");
        assert_eq!(parse_alias("a", &obj! { "indexRouting" => "x" }).unwrap_err().reason(), "Unknown field [indexRouting] in alias [a]");
        assert_eq!(parse_alias("a", &Value::from("x")).unwrap_err().reason(), "No alias is specified");
        assert_eq!(parse_alias("a", &obj! { "is_write_index" => "x" }).unwrap_err().reason(), "Unsupported String type value [x] for field [is_write_index] in alias [a]");
        assert_eq!(parse_alias("a", &obj! { "index_routing" => "1,2" }).unwrap_err().reason(), "alias [a] has several index routing values associated with it");
        let e = parse_alias("a", &obj! { "filter" => obj! { "bogus" => obj! {} } }).unwrap_err();
        assert_eq!(e.reason(), "failed to parse filter for alias [a]");
        assert_eq!(e.caused_by.unwrap().reason(), "unknown query [bogus]");
        let a = parse_alias("a", &obj! { "routing" => "1" }).unwrap();
        assert_eq!(a.to_value().to_json(), r#"{"index_routing":"1","search_routing":"1"}"#);
    }
}
