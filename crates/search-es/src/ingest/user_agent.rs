//! The `user_agent` processor and its parsers (`UserAgentProcessor` over the `user-agent`
//! module's `UserAgentParserImpl`, `DeviceTypeParser` and `UserAgentCache`).
//!
//! A parser is a regex file in uap-core's format: `user_agent_parsers`, `os_parsers` and
//! `device_parsers` lists of `regex` (plus `regex_flag: 'i'`) and replacements. Each of the three
//! lists is tried in order and the first pattern that `find`s in the user agent wins; its groups
//! give the name and up to four version parts unless the pattern replaces them (`$1` in a name
//! replacement is the first group). The default parser is uap-core's `regexes.yml` as
//! Elasticsearch 9.5.5 bundles it, with Elasticsearch's `device_type_regexes.yml` for
//! `extract_device_type` (both Apache-2.0, vendored in `assets/`); every `*.yml` in the node's
//! `<path.conf>/user-agent` and `<path.conf>/ingest-user-agent` directories is another parser,
//! named by its file name, read when the node first needs a parser (Elasticsearch reads them at
//! startup). Patterns are Java regular expressions ([`search_analysis::jregex`]).
//!
//! Parsing one user agent can try a thousand patterns, so results are cached per parser
//! (1000 entries, like Elasticsearch's `user_agent.cache_size` default), and the cache is
//! cleared when full instead of evicting entries one by one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use search_analysis::jregex::{self, JavaRegex};

use crate::error::{EsError, EsResult};

/// `UserAgentParserRegistry.DEFAULT_PARSER_NAME`.
pub const DEFAULT_PARSER: &str = "_default_";

/// Entries a parser's cache holds before it starts over.
const CACHE_SIZE: usize = 1000;

/// A name and its dotted version (`VersionedName`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionedName {
    pub name: String,
    pub version: Option<String>,
}

/// What a user agent parses into (`Details`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Details {
    pub name: Option<String>,
    pub version: Option<String>,
    pub os: Option<VersionedName>,
    pub os_full: Option<String>,
    pub device: Option<VersionedName>,
    pub device_type: Option<String>,
}

/// One pattern of a list (`UserAgentSubpattern`).
struct Subpattern {
    re: JavaRegex,
    name: Option<String>,
    versions: [Option<String>; 4],
}

impl Subpattern {
    /// `UserAgentSubpattern.match`.
    fn matches(&self, agent: &str) -> Option<VersionedName> {
        let mut caps = self.re.captures();
        if !self.re.find_at(agent, 0, &mut caps) {
            return None;
        }
        let groups = self.re.group_count();
        let group = |i: usize| if groups >= i { caps.get(i).map(|(s, e)| agent[s..e].to_string()) } else { None };
        let name = match &self.name {
            Some(r) if r.contains("$1") && groups >= 1 => match group(1) {
                Some(g) => r.replacen("$1", &g, 1),
                None => r.clone(),
            },
            Some(r) => r.clone(),
            None => group(1)?,
        };
        let part = |i: usize| self.versions[i].clone().or_else(|| group(i + 2));
        let version = version_string([part(0), part(1), part(2), part(3)]);
        Some(VersionedName { name, version })
    }
}

/// `UserAgentParserImpl.versionToString`: major, then each following part while present.
fn version_string(parts: [Option<String>; 4]) -> Option<String> {
    let mut out = String::new();
    for (i, p) in parts.into_iter().enumerate() {
        match p.filter(|p| !p.is_empty()) {
            Some(p) => {
                if i > 0 {
                    out.push('.');
                }
                out.push_str(&p);
            }
            None => break,
        }
    }
    (!out.is_empty()).then_some(out)
}

type TypeList = Vec<(JavaRegex, Option<String>)>;

/// The device type lists (`DeviceTypeParser`).
struct DeviceTypes {
    os: TypeList,
    browser: TypeList,
    device: TypeList,
    agent: TypeList,
}

impl DeviceTypes {
    fn parse(text: &str) -> Result<DeviceTypes, String> {
        let sections = parse_regex_file(text)?;
        let list = |key: &str| -> Result<TypeList, String> {
            let Some(items) = sections.iter().find(|(k, _)| k == key).map(|(_, v)| v) else {
                return Err("not a valid regular expression file".into());
            };
            items.iter().map(|m| Ok((compile(get(m, "regex").unwrap_or_default(), None)?, get(m, "replacement").map(str::to_string)))).collect()
        };
        Ok(DeviceTypes { os: list("os_parsers")?, browser: list("browser_parsers")?, device: list("device_parsers")?, agent: list("agent_string_parsers")? })
    }

    fn find(list: &[(JavaRegex, Option<String>)], text: &str) -> Option<String> {
        for (re, rep) in list {
            let mut caps = re.captures();
            if !re.find_at(text, 0, &mut caps) {
                continue;
            }
            // A matching pattern without a replacement gives no type (and ends the search).
            return match rep {
                Some(r) if r.contains("$1") && re.group_count() >= 1 => match caps.get(1) {
                    Some((s, e)) => Some(r.replacen("$1", &text[s..e], 1)),
                    None => Some(r.clone()),
                },
                other => other.clone(),
            };
        }
        None
    }

    /// `DeviceTypeParser.findDeviceType`: the agent string's own rules, else the most specific
    /// type the OS, browser and device names give.
    fn device_type(&self, agent: &str, ua: Option<&VersionedName>, os: Option<&VersionedName>, device: Option<&VersionedName>) -> String {
        if let Some(t) = Self::find(&self.agent, agent) {
            return t;
        }
        let found: Vec<String> =
            [os.and_then(|o| Self::find(&self.os, &o.name)), ua.and_then(|u| Self::find(&self.browser, &u.name)), device.and_then(|d| Self::find(&self.device, &d.name))]
                .into_iter()
                .flatten()
                .collect();
        for t in ["Robot", "Tablet", "Phone", "Desktop"] {
            if found.iter().any(|f| f == t) {
                return t.to_string();
            }
        }
        "Other".to_string()
    }
}

/// Parsed user agents by string and whether device types were asked for.
type DetailsCache = HashMap<(Box<str>, bool), Arc<Details>>;

/// One regex file (`UserAgentParserImpl`) with its cache.
pub struct Parser {
    ua: Vec<Subpattern>,
    os: Vec<Subpattern>,
    device: Vec<Subpattern>,
    device_types: Option<Arc<DeviceTypes>>,
    cache: parking_lot::Mutex<DetailsCache>,
}

impl Parser {
    fn new(text: &str, device_types: Option<Arc<DeviceTypes>>) -> Result<Parser, String> {
        let sections = parse_regex_file(text)?;
        let list = |key: &str, name: &str, versions: [&str; 4]| -> Result<Vec<Subpattern>, String> {
            let Some(items) = sections.iter().find(|(k, _)| k == key).map(|(_, v)| v) else { return Ok(Vec::new()) };
            items
                .iter()
                .map(|m| {
                    Ok(Subpattern {
                        re: compile(get(m, "regex").unwrap_or_default(), get(m, "regex_flag"))?,
                        name: get(m, name).map(str::to_string),
                        versions: versions.map(|v| get(m, v).map(str::to_string)),
                    })
                })
                .collect()
        };
        let ua = list("user_agent_parsers", "family_replacement", ["v1_replacement", "v2_replacement", "v3_replacement", "v4_replacement"])?;
        let os = list("os_parsers", "os_replacement", ["os_v1_replacement", "os_v2_replacement", "os_v3_replacement", "os_v4_replacement"])?;
        // Device patterns only replace the name.
        let device = list("device_parsers", "device_replacement", ["", "", "", ""])?;
        if ua.is_empty() && os.is_empty() && device.is_empty() {
            return Err("not a valid regular expression file".into());
        }
        Ok(Parser { ua, os, device, device_types, cache: parking_lot::Mutex::new(HashMap::new()) })
    }

    /// `UserAgentParserImpl.parseUserAgentInfo` (cached).
    pub fn parse(&self, agent: &str, extract_device_type: bool) -> Arc<Details> {
        let key = (Box::<str>::from(agent), extract_device_type);
        if let Some(d) = self.cache.lock().get(&key) {
            return d.clone();
        }
        let first = |list: &[Subpattern]| list.iter().find_map(|p| p.matches(agent));
        let ua = first(&self.ua);
        let os = first(&self.os);
        let device = first(&self.device);
        let device_type = match (&self.device_types, extract_device_type) {
            (Some(t), true) => Some(t.device_type(agent, ua.as_ref(), os.as_ref(), device.as_ref())),
            _ => None,
        };
        let os_full = os.as_ref().and_then(|o| o.version.as_ref().map(|v| format!("{} {v}", o.name)));
        let details = Arc::new(Details { name: ua.as_ref().map(|u| u.name.clone()), version: ua.and_then(|u| u.version), os, os_full, device, device_type });
        let mut cache = self.cache.lock();
        if cache.len() >= CACHE_SIZE {
            cache.clear();
        }
        cache.insert(key, details.clone());
        details
    }
}

fn compile(regex: &str, flag: Option<&str>) -> Result<JavaRegex, String> {
    let flags = if flag == Some("i") { jregex::CASE_INSENSITIVE } else { 0 };
    jregex::compile(regex, flags).map_err(|e| e.reason)
}

fn get<'a>(m: &'a [(String, String)], key: &str) -> Option<&'a str> {
    if key.is_empty() {
        return None;
    }
    m.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

type Sections = Vec<(String, Vec<Vec<(String, String)>>)>;

/// The YAML subset regex files use: top-level keys holding block sequences of flat mappings
/// whose values are plain, single-quoted or double-quoted scalars, and `#` comments.
fn parse_regex_file(text: &str) -> Result<Sections, String> {
    let mut sections: Sections = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" {
            continue;
        }
        if !line.starts_with([' ', '-']) {
            let key = trimmed.strip_suffix(':').ok_or_else(|| format!("malformed regular expression file: [{trimmed}]"))?;
            sections.push((unquote(key.trim())?, Vec::new()));
            continue;
        }
        let Some((_, items)) = sections.last_mut() else { return Err("malformed regular expression file, expecting 'object'".into()) };
        let entry = match trimmed.strip_prefix("- ") {
            Some(rest) => {
                items.push(Vec::new());
                rest.trim_start()
            }
            None => trimmed,
        };
        let Some(item) = items.last_mut() else { return Err("malformed regular expression file, expecting 'object'".into()) };
        let (k, v) = split_key(entry).ok_or_else(|| format!("malformed regular expression file: [{trimmed}]"))?;
        item.push((k, unquote(v)?));
    }
    Ok(sections)
}

/// `key: value` (the first `": "`; keys are never quoted in regex files).
fn split_key(s: &str) -> Option<(String, &str)> {
    let i = s.find(": ").or_else(|| s.strip_suffix(':').map(str::len))?;
    Some((s[..i].trim().to_string(), s.get(i + 1..).unwrap_or("").trim()))
}

/// A YAML flow scalar: `'…'` (`''` is a quote), `"…"` (backslash escapes), or plain (a trailing
/// ` #` comment dropped).
fn unquote(v: &str) -> Result<String, String> {
    if let Some(body) = v.strip_prefix('\'') {
        let end = body.rfind('\'').ok_or_else(|| format!("unterminated quoted scalar [{v}]"))?;
        return Ok(body[..end].replace("''", "'"));
    }
    if let Some(body) = v.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Ok(out),
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('0') => out.push('\0'),
                    Some('/') => out.push('/'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(o) => {
                        out.push('\\');
                        out.push(o);
                    }
                    None => break,
                },
                c => out.push(c),
            }
        }
        return Err(format!("unterminated quoted scalar [{v}]"));
    }
    Ok(match v.find(" #") {
        Some(i) => v[..i].trim_end().to_string(),
        None => v.to_string(),
    })
}

static DEVICE_TYPES: LazyLock<Option<Arc<DeviceTypes>>> =
    LazyLock::new(|| DeviceTypes::parse(include_str!("../../assets/user_agent_device_type_regexes.yml")).ok().map(Arc::new));

static DEFAULT: LazyLock<Arc<Parser>> =
    LazyLock::new(|| Arc::new(Parser::new(include_str!("../../assets/user_agent_regexes.yml"), DEVICE_TYPES.clone()).expect("the bundled user agent regexes parse")));

/// Custom parsers per configuration directory (each directory read once).
type Registry = HashMap<String, Arc<Parser>>;
static CUSTOM: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Arc<Registry>>>> = LazyLock::new(Default::default);

/// The parser a processor's `regex_file` names; `conf` is the node's `path.conf`.
pub fn parser(conf: Option<&str>, name: &str) -> Option<Arc<Parser>> {
    if name == DEFAULT_PARSER {
        return Some(DEFAULT.clone());
    }
    let dir = PathBuf::from(conf?);
    let parsers = CUSTOM.lock().entry(dir.clone()).or_insert_with(|| Arc::new(load_dir(&dir))).clone();
    parsers.get(name).cloned()
}

/// `UserAgentParserRegistryImpl.createParsersMap`: `*.yml` in `user-agent/` then the legacy
/// `ingest-user-agent/` (a file in both comes from the latter); unreadable files are skipped.
fn load_dir(conf: &Path) -> Registry {
    let mut out = HashMap::new();
    for sub in ["user-agent", "ingest-user-agent"] {
        let Ok(entries) = std::fs::read_dir(conf.join(sub)) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).filter(|n| n.ends_with(".yml")).map(str::to_string) else { continue };
            if !path.is_file() {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(p) = Parser::new(&text, DEVICE_TYPES.clone())
            {
                out.insert(name, Arc::new(p));
            }
        }
    }
    out
}

/// The processor's `properties` (`UserAgentProcessor.Property`, in `EnumSet` order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Property {
    Name,
    Os,
    Device,
    Original,
    Version,
}

impl Property {
    pub const ALL: [Property; 5] = [Property::Name, Property::Os, Property::Device, Property::Original, Property::Version];

    pub fn parse(s: &str) -> Result<Property, String> {
        Ok(match s.to_uppercase().as_str() {
            "NAME" => Property::Name,
            "OS" => Property::Os,
            "DEVICE" => Property::Device,
            "ORIGINAL" => Property::Original,
            "VERSION" => Property::Version,
            _ => return Err(format!("illegal property value [{s}]. valid values are [NAME, OS, DEVICE, ORIGINAL, VERSION]")),
        })
    }
}

/// The processor's configuration.
#[derive(Clone)]
pub struct Processor {
    pub field: String,
    pub target: String,
    pub parser: Arc<Parser>,
    /// Sorted, deduplicated.
    pub properties: Vec<Property>,
    pub extract_device_type: bool,
    pub ignore_missing: bool,
}

/// `UserAgentProcessor.execute`.
pub fn run(p: &Processor, d: &mut super::doc::IngestDoc) -> EsResult<()> {
    use crate::json::{Map, Value};
    let agent = match d.get_opt(&p.field) {
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Null) | None if p.ignore_missing => return Ok(()),
        None => return d.get(&p.field).map(|_| ()),
        Some(Value::Null) => return Err(EsError::illegal_argument(format!("field [{}] is null, cannot parse user-agent.", p.field))),
        Some(other) => return Err(EsError::illegal_argument(format!("field [{}] of type [{}] cannot be cast to [java.lang.String]", p.field, super::doc::java_type(other)))),
    };
    let details = p.parser.parse(&agent, p.extract_device_type);
    let mut out = Map::new();
    for prop in &p.properties {
        match prop {
            Property::Original => {
                out.insert("original".into(), Value::Str(agent.clone()));
            }
            Property::Name => {
                out.insert("name".into(), Value::Str(details.name.clone().unwrap_or_else(|| "Other".into())));
            }
            Property::Version => {
                if let Some(v) = &details.version {
                    out.insert("version".into(), Value::Str(v.clone()));
                }
            }
            Property::Os => {
                if let Some(os) = &details.os {
                    let mut m = Map::new();
                    m.insert("name".into(), Value::Str(os.name.clone()));
                    if let Some(v) = &os.version {
                        m.insert("version".into(), Value::Str(v.clone()));
                        m.insert("full".into(), Value::Str(details.os_full.clone().unwrap_or_default()));
                    }
                    out.insert("os".into(), Value::Obj(m));
                }
            }
            Property::Device => {
                let mut m = Map::new();
                m.insert("name".into(), Value::Str(details.device.as_ref().map_or_else(|| "Other".to_string(), |dv| dv.name.clone())));
                if p.extract_device_type {
                    m.insert("type".into(), Value::Str(details.device_type.clone().unwrap_or_else(|| "Other".into())));
                }
                out.insert("device".into(), Value::Obj(m));
            }
        }
    }
    // A `HashMap` in Elasticsearch.
    super::doc::java_hash_order(&mut out, true);
    d.set(&p.target, Value::Obj(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_9_2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/33.0.1750.149 Safari/537.36";

    #[test]
    fn default_parser_reads_chrome_on_mac() {
        let d = DEFAULT.parse(CHROME_MAC, true);
        assert_eq!(d.name.as_deref(), Some("Chrome"));
        assert_eq!(d.version.as_deref(), Some("33.0.1750.149"));
        assert_eq!(d.os, Some(VersionedName { name: "Mac OS X".into(), version: Some("10.9.2".into()) }));
        assert_eq!(d.os_full.as_deref(), Some("Mac OS X 10.9.2"));
        assert_eq!(d.device.as_ref().map(|d| d.name.as_str()), Some("Mac"));
        assert_eq!(d.device_type.as_deref(), Some("Desktop"));
        // Cached the second time, and identical.
        assert_eq!(*DEFAULT.parse(CHROME_MAC, true), *d);
    }

    #[test]
    fn custom_files_and_replacements() {
        let p = Parser::new("user_agent_parsers:\n  - regex: '.*'\n    family_replacement: 'Test'\n", None).unwrap();
        let d = p.parse(CHROME_MAC, false);
        assert_eq!((d.name.as_deref(), d.version.as_deref(), d.os.as_ref()), (Some("Test"), None, None));
        let p = Parser::new("os_parsers:\n  - regex: \"(Foo)/(\\\\d+)\"\n    os_replacement: 'Bar $1'\n", None).unwrap();
        let d = p.parse("x Foo/12", false);
        assert_eq!(d.os, Some(VersionedName { name: "Bar Foo".into(), version: Some("12".into()) }));
        assert!(Parser::new("# nothing\nother: \n", None).is_err());
    }

    #[test]
    fn version_parts_stop_at_the_first_missing() {
        let s = |x: &str| Some(x.to_string());
        assert_eq!(version_string([s("1"), s("2"), None, s("4")]).as_deref(), Some("1.2"));
        assert_eq!(version_string([None, s("2"), None, None]), None);
        assert_eq!(version_string([s("1"), s(""), s("3"), None]).as_deref(), Some("1"));
    }
}
