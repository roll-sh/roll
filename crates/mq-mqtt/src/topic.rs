//! MQTT topic names and filters: validation, matching, and RabbitMQ's mapping to AMQP topic
//! routing keys (`/` ↔ `.`, `+` ↔ `*`, `#` stays `#`; a `.` inside an MQTT level therefore
//! becomes a level separator on the AMQP side, exactly as in RabbitMQ).

/// MQTT topic (or filter) → AMQP routing (or binding) key.
pub fn mqtt_to_amqp(topic: &str, sparkplug: bool) -> String {
    let t = if sparkplug && sparkplug_mqtt(topic) { topic.replacen('.', "___", 1) } else { topic.to_string() };
    t.replace('/', ".").replace('+', "*")
}

/// AMQP routing (or binding) key → MQTT topic (or filter).
pub fn amqp_to_mqtt(key: &str, sparkplug: bool) -> String {
    let t = key.replace('*', "+").replace('.', "/");
    if sparkplug && sparkplug_amqp(key) { t.replacen("___", ".", 1) } else { t }
}

/// `^sp[AB]v\d+\.\d+/`
fn sparkplug_mqtt(t: &str) -> bool {
    sparkplug_prefix(t, ".", '/')
}

/// `^sp[AB]v\d+___\d+\.`
fn sparkplug_amqp(t: &str) -> bool {
    sparkplug_prefix(t, "___", '.')
}

fn sparkplug_prefix(t: &str, sep: &str, end: char) -> bool {
    let Some(rest) = t.strip_prefix("spAv").or_else(|| t.strip_prefix("spBv")) else { return false };
    let major = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if major == 0 {
        return false;
    }
    let Some(rest) = rest[major..].strip_prefix(sep) else { return false };
    let minor = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    minor > 0 && rest[minor..].starts_with(end)
}

/// A topic name a client may publish to: non-empty, no wildcards, no NUL.
pub fn valid_name(t: &str) -> bool {
    !t.is_empty() && !t.contains(['+', '#', '\0'])
}

/// A topic filter a client may subscribe to: `+` fills a whole level, `#` only as the last one.
pub fn valid_filter(f: &str) -> bool {
    if f.is_empty() || f.contains('\0') {
        return false;
    }
    let levels: Vec<&str> = f.split('/').collect();
    let last = levels.len() - 1;
    levels.iter().enumerate().all(|(i, l)| match *l {
        "#" => i == last,
        "+" => true,
        l => !l.contains(['+', '#']),
    })
}

/// A v5 shared subscription (`$share/{group}/{filter}`).
pub fn is_shared(f: &str) -> bool {
    f.starts_with("$share/")
}

/// MQTT filter matching (used for retained messages). Topics starting with `$` are not matched
/// by a leading wildcard.
pub fn matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && (filter.starts_with('+') || filter.starts_with('#')) {
        return false;
    }
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// AMQP topic-exchange matching (`*` one word, `#` zero or more), the semantics the broker
/// routed with. Used to find which of a session's subscriptions a delivery matched.
pub fn amqp_matches(binding: &str, key: &str) -> bool {
    fn go(b: &[&str], k: &[&str]) -> bool {
        match b.split_first() {
            None => k.is_empty(),
            Some((&"#", rest)) => (0..=k.len()).any(|i| go(rest, &k[i..])),
            Some((w, rest)) => match k.split_first() {
                Some((kw, krest)) => (*w == "*" || w == kw) && go(rest, krest),
                None => false,
            },
        }
    }
    let b: Vec<&str> = binding.split('.').collect();
    let k: Vec<&str> = if key.is_empty() { vec![""] } else { key.split('.').collect() };
    go(&b, &k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_follows_rabbitmq() {
        assert_eq!(mqtt_to_amqp("a/b/c", false), "a.b.c");
        assert_eq!(mqtt_to_amqp("a/+/#", false), "a.*.#");
        assert_eq!(mqtt_to_amqp("a.b/c", false), "a.b.c");
        assert_eq!(amqp_to_mqtt("a.*.#", false), "a/+/#");
        assert_eq!(amqp_to_mqtt("a.b.c", false), "a/b/c");
        assert_eq!(mqtt_to_amqp("/a", false), ".a");
    }

    #[test]
    fn sparkplug() {
        assert_eq!(mqtt_to_amqp("spBv1.0/group/NBIRTH/edge", true), "spBv1___0.group.NBIRTH.edge");
        assert_eq!(amqp_to_mqtt("spBv1___0.group.NBIRTH.edge", true), "spBv1.0/group/NBIRTH/edge");
        assert_eq!(mqtt_to_amqp("spBv1.0/group", false), "spBv1.0.group");
        assert_eq!(mqtt_to_amqp("spCv1.0/x", true), "spCv1.0.x");
    }

    #[test]
    fn validation() {
        assert!(valid_name("a/b"));
        assert!(!valid_name("a/+"));
        assert!(!valid_name(""));
        assert!(valid_filter("a/+/c"));
        assert!(valid_filter("#"));
        assert!(valid_filter("a/#"));
        assert!(!valid_filter("a/#/b"));
        assert!(!valid_filter("a+/b"));
        assert!(!valid_filter("a/b#"));
    }

    #[test]
    fn mqtt_matching() {
        assert!(matches("a/+/c", "a/b/c"));
        assert!(matches("a/#", "a"));
        assert!(matches("a/#", "a/b/c"));
        assert!(matches("#", "x/y"));
        assert!(!matches("#", "$SYS/x"));
        assert!(matches("$SYS/#", "$SYS/x"));
        assert!(!matches("a/+", "a/b/c"));
        assert!(matches("+/+", "/x"));
    }

    #[test]
    fn amqp_matching() {
        assert!(amqp_matches("a.*.c", "a.b.c"));
        assert!(amqp_matches("a.#", "a"));
        assert!(amqp_matches("#", "a.b"));
        assert!(!amqp_matches("a.*", "a.b.c"));
        assert!(amqp_matches("a.#.c", "a.c"));
    }
}
