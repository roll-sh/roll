//! `index.auto_expand_replicas` (`0-1`, `0-all`, `false`): the replica count follows the number
//! of data nodes. A roll cluster reports a single data node, so the count is the range's lower
//! bound, written into `index.number_of_replicas` as Elasticsearch's allocation service does.

use super::settings::Settings;

/// The replica count `auto_expand_replicas` asks for on a one-data-node cluster, if set.
pub fn desired_replicas(value: &str) -> Option<u32> {
    if value == "false" {
        return None;
    }
    let (lo, hi) = value.split_once('-')?;
    let lo: u32 = lo.parse().ok()?;
    // One data node: zero replicas fit, clamped into [lo, hi].
    let max = if hi == "all" { u32::MAX } else { hi.parse().ok()? };
    Some(0u32.clamp(lo, max.max(lo)))
}

/// Applies `index.auto_expand_replicas` to `index.number_of_replicas`; returns whether it
/// changed.
pub fn apply(settings: &mut Settings) -> bool {
    let Some(n) = settings.get("index.auto_expand_replicas").and_then(desired_replicas) else { return false };
    let n = n.to_string();
    if settings.get("index.number_of_replicas") == Some(n.as_str()) {
        return false;
    }
    settings.set("index.number_of_replicas", n);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(desired_replicas("0-1"), Some(0));
        assert_eq!(desired_replicas("0-all"), Some(0));
        assert_eq!(desired_replicas("1-3"), Some(1));
        assert_eq!(desired_replicas("false"), None);
        let mut s = Settings::new();
        s.set("index.auto_expand_replicas", "0-1");
        s.set("index.number_of_replicas", "1");
        assert!(apply(&mut s));
        assert_eq!(s.get("index.number_of_replicas"), Some("0"));
        assert!(!apply(&mut s));
    }
}
