//! Postgres column masks for a preview's branch (`masking` rules for `postgres:<ref>`): the
//! columns are rewritten in the branch, once it's made or reset and before anything serves it.
//! The parent is never touched. NoSQL masks are a read policy instead (`Brancher::mask`), which
//! can't hide a key column ([`nosql_key_masks`]).

use std::collections::BTreeMap;

/// The rules a column can have.
pub const RULES: &str = "null, redact, hash, email or fixed:<value>";

/// What masking a branch runs: one statement that rewrites every masked column (one `UPDATE`
/// per table, all in one transaction, with the tables' triggers off so none copies the old
/// values elsewhere), then each table's `VACUUM FULL` (the rows as they were leave the branch's
/// files), every materialized view refreshed (none keeps the old values), and a checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub update: String,
    pub after: Vec<String>,
}

/// The plan for `columns` (`schema.table.column` → rule), or why a rule can't be one. `salt`
/// (random per run) keys `hash` and `email`, so a value can't be found again by hashing guesses;
/// within one run the same input still gives the same output, so joins on it match.
pub fn postgres(columns: &BTreeMap<String, String>, salt: &str) -> Result<Plan, String> {
    let salt = literal(salt);
    let mut tables: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (path, rule) in columns {
        let parts: Vec<&str> = path.split('.').collect();
        let [schema, table, column] = parts[..] else { return Err(format!("{path:?} isn't schema.table.column")) };
        if [schema, table, column].iter().any(|p| p.is_empty()) {
            return Err(format!("{path:?} isn't schema.table.column"));
        }
        let c = ident(column);
        let value = match rule.trim() {
            "null" => "NULL".to_string(),
            "redact" => format!("CASE WHEN {c} IS NULL THEN NULL ELSE '***' END"),
            "hash" => format!("md5({salt} || {c}::text)"),
            "email" => format!("CASE WHEN {c} IS NULL THEN NULL ELSE 'user-' || left(md5({salt} || {c}::text), 12) || '@example.invalid' END"),
            r => match r.strip_prefix("fixed:") {
                Some(v) => literal(v),
                None => return Err(format!("{path}: {rule:?} isn't a rule ({RULES})")),
            },
        };
        tables.entry((schema.to_string(), table.to_string())).or_default().push(format!("{c} = {value}"));
    }
    // Only the catalog's own names resolve (nothing the branch's role made can stand in for a
    // function this runs as the superuser), literals read as written, and no trigger of the masked
    // tables runs for the rewrite (an audit log's copy of OLD, a notification): each table's user
    // triggers are switched off and then put back exactly as they were (`ALWAYS`, `REPLICA` and
    // disabled ones too; `session_replication_role` would let those fire).
    let mut update = String::from("BEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL standard_conforming_strings = on;\n");
    let rels: Vec<String> = tables.keys().map(|(schema, table)| format!("{}::regclass", literal(&format!("{}.{}", ident(schema), ident(table))))).collect();
    update.push_str(&format!(
        "CREATE TEMP TABLE roll_triggers ON COMMIT DROP AS SELECT tgrelid::regclass::text AS rel, tgname, tgenabled FROM pg_catalog.pg_trigger WHERE NOT tgisinternal AND tgrelid IN ({});\n",
        rels.join(", ")
    ));
    update.push_str("DO $roll$ DECLARE r record; BEGIN FOR r IN SELECT DISTINCT rel FROM roll_triggers LOOP EXECUTE pg_catalog.format('ALTER TABLE %s DISABLE TRIGGER USER', r.rel); END LOOP; END $roll$;\n");
    let mut after = Vec::new();
    for ((schema, table), sets) in &tables {
        let t = format!("{}.{}", ident(schema), ident(table));
        update.push_str(&format!("UPDATE {t} SET {};\n", sets.join(", ")));
        // Maintenance runs as the table's owner, with a safe search_path (PostgreSQL 17+).
        after.push(format!("VACUUM (FULL, ANALYZE) {t}"));
    }
    update.push_str(
        "DO $roll$ DECLARE r record; BEGIN FOR r IN SELECT * FROM roll_triggers LOOP EXECUTE pg_catalog.format('ALTER TABLE %s %s TRIGGER %I', r.rel, \
         CASE r.tgenabled WHEN 'A' THEN 'ENABLE ALWAYS' WHEN 'R' THEN 'ENABLE REPLICA' WHEN 'D' THEN 'DISABLE' ELSE 'ENABLE' END, r.tgname); END LOOP; END $roll$;\n",
    );
    update.push_str("COMMIT;");
    // A materialized view refreshes as its owner (the branch's role), never as the superuser.
    after.push(
        "SET search_path = pg_catalog, pg_temp; DO $roll$ DECLARE v record; BEGIN FOR v IN SELECT schemaname, matviewname FROM pg_catalog.pg_matviews WHERE ispopulated LOOP \
         EXECUTE pg_catalog.format('REFRESH MATERIALIZED VIEW %I.%I', v.schemaname, v.matviewname); END LOOP; END $roll$"
            .into(),
    );
    after.push("CHECKPOINT".into());
    Ok(Plan { update, after })
}

/// Whether `rule` is one a Postgres column can have.
/// A Postgres column's rule that leaves it as it is, where it would be masked by default (it looks
/// like people's data, and its parent has rules): the project's word that it isn't.
pub const KEEP: &str = "keep";

pub fn is_rule(rule: &str) -> bool {
    matches!(rule.trim(), "null" | "redact" | "hash" | "email") || rule.trim().starts_with("fixed:")
}

/// A quoted identifier: any name, as itself.
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A quoted string literal (standard strings: only `'` needs doubling).
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The NoSQL masks (`keyspace.table.column`) that name a column of a primary key, in the
/// parent's schema (roll nosql's `/schema`): the table, or a view of it keyed by that column,
/// can't be read with it masked (a key reaches its reader however the rows are masked), so the
/// preview's pages would fail. One line per problem; columns the schema doesn't have yet (a
/// migration may add them) are none.
pub fn nosql_key_masks(schema: &serde_json::Value, columns: &BTreeMap<String, String>) -> Vec<String> {
    let has = |t: &serde_json::Value, column: &str| {
        ["partition_key", "clustering_key"].iter().any(|k| t[k].as_array().is_some_and(|a| a.iter().any(|c| c == column)))
    };
    let mut out = Vec::new();
    for path in columns.keys() {
        let mut parts = path.splitn(3, '.');
        let (Some(ks), Some(table), Some(column)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(k) = schema["keyspaces"].as_array().into_iter().flatten().find(|k| k["name"] == ks) else { continue };
        let tables = k["tables"].as_array().into_iter().flatten();
        if tables.filter(|t| t["name"] == table).any(|t| has(t, column)) {
            out.push(format!("{path} is in {ks}.{table}'s primary key, and a table can't be read with a key column masked: mask its other columns"));
            continue;
        }
        for v in k["views"].as_array().into_iter().flatten().filter(|v| v["base_table"] == table && has(v, column)) {
            out.push(format!("{path} is in the key of the view {ks}.{}, which can't be read with it masked: mask other columns, or drop the view from what the preview reads", v["name"].as_str().unwrap_or_default()));
        }
    }
    out
}

/// A search mask's index and field (`index.field`): an index name can hold dots (`logs.2024`), so
/// the split is after the longest prefix that's one of `indices` (the cluster's), else at the
/// first dot.
pub fn split_mask<'a>(spec: &'a str, indices: &[String]) -> Option<(&'a str, &'a str)> {
    let longest = spec.match_indices('.').map(|(i, _)| i).filter(|&i| indices.iter().any(|x| x == &spec[..i])).max();
    match longest {
        Some(i) => Some((&spec[..i], &spec[i + 1..])),
        None => spec.split_once('.'),
    }
}

/// The search types a string goes into (`constant_keyword` takes only its one value, so not it).
const SEARCH_TEXT: [&str; 8] = ["text", "keyword", "match_only_text", "wildcard", "search_as_you_type", "semantic_text", "completion", "token_count"];

/// Search field masks that can't work against a `_mapping` answer (index → `mappings`): `redact`
/// and `email` write a string, which only a text-like field takes (a `dense_vector`, a number, a
/// date would refuse it and fail the masking, and the preview with it); such a field can only be
/// `remove`d. One line per problem; a field the mapping doesn't have yet (a document may add it)
/// is none.
/// `indices`: the cluster's, so an index whose name has dots is split off whole ([`split_mask`]).
pub fn search_field_masks(mapping: &serde_json::Value, fields: &BTreeMap<String, String>, indices: &[String]) -> Vec<String> {
    const TEXT: [&str; 8] = SEARCH_TEXT;
    let mut out = Vec::new();
    for (spec, rule) in fields {
        if rule == "remove" {
            continue;
        }
        let Some((index, path)) = split_mask(spec, indices) else { continue };
        // The index as named, or every index an alias or pattern answered for (`GET /<it>/_mapping`
        // is keyed by the real names): the first that says the field isn't text.
        let named: Vec<&serde_json::Value> = match mapping.get(index) {
            Some(m) => vec![m],
            None => mapping.as_object().into_iter().flatten().map(|(_, m)| m).collect(),
        };
        let Some(props) = named.iter().map(|m| &m["mappings"]["properties"]).find(|props| field_type(props, path).is_some_and(|t| !TEXT.contains(&t.as_str()) && t != "?")).or_else(|| named.first().map(|m| &m["mappings"]["properties"])) else { continue };
        if let Some(t) = field_type(props, path).filter(|t| !TEXT.contains(&t.as_str()) && t != "?") {
            let a = if t.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
            out.push(format!("{spec} is {a} {t} field: {rule} writes a string it won't take; use remove"));
        }
    }
    out
}

/// A field's mapping type down `path` through objects' `properties` (nested ones too), if the
/// mapping has it.
fn field_type(props: &serde_json::Value, path: &str) -> Option<String> {
    let mut at = props;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        let f = at.get(*part)?;
        if i == parts.len() - 1 {
            return Some(f["type"].as_str().unwrap_or(if f.get("properties").is_some() { "object" } else { "?" }).to_string());
        }
        at = &f["properties"];
    }
    None
}

// ── Mask suggestions: columns that look like they hold people's data ──

/// The most columns (or fields) read from one parent for suggestions: past it, the rest aren't
/// looked at (a schema that large is named in `unread`, not read whole).
pub const SUGGEST_COLUMNS_MAX: usize = 2000;

/// A column of a parent (or a search field), as [`suggest`] reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Column {
    /// As a mask names it: `schema.table.column`, `keyspace.table.column` or `index.field`.
    pub path: String,
    /// The product's type (`character varying(255)`, `text`, `keyword`), when it says.
    pub r#type: Option<String>,
    /// A null would fail the masking: Postgres's `NOT NULL`, or a unique index `NULLS NOT
    /// DISTINCT` covers it.
    pub not_null: bool,
    /// A unique index covers it: one value for every row (`redact`) would fail the masking.
    pub unique: bool,
    /// What a mask may write into it, whatever its name says (Postgres's constraints and types).
    pub takes: Takes,
}

/// What a mask may write into a column without failing the masking (and the preview with it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Takes {
    /// A value of its type, as [`suggest`] picks it.
    #[default]
    Any,
    /// Only a null: an array, a range, an enum or a composite (no text or typed value of
    /// [`RULES`] fits one), or a column of a foreign key (another value would point nowhere).
    Null,
    /// Nothing: another value or a null could break what holds it (a foreign key other rows
    /// point at, a CHECK, a domain's constraints, a unique index's expression or predicate, an
    /// exclusion constraint, a partition key).
    Nothing,
}

/// What kind of people's data a column's name says it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Personal {
    Email,
    Name,
    Phone,
    Address,
    BirthDate,
    Ip,
    /// A national id, tax id or passport number: unique per person and joined on, so hashed.
    NationalId,
    /// A bank account or a card's number.
    Financial,
    /// A secret, token or key: nothing a preview needs, so emptied.
    Secret,
}

impl Personal {
    pub fn why(self) -> &'static str {
        match self {
            Personal::Email => "an email address",
            Personal::Name => "a person's name",
            Personal::Phone => "a phone number",
            Personal::Address => "a postal address",
            Personal::BirthDate => "a date of birth",
            Personal::Ip => "an IP address",
            Personal::NationalId => "a national id, tax id or passport number",
            Personal::Financial => "a bank account or card number",
            Personal::Secret => "a secret, token or key",
        }
    }
}

/// A rule suggested for a column, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub rule: String,
    pub why: &'static str,
}

/// A name's words, lowercase: `snake_case`, `camelCase`, `kebab-case`, dots and acronyms
/// (`IPAddress` is `ip address`) split.
pub fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        // A new word at `aB`, `a1`/`1a`, and at the last capital of a run before a lowercase (`IPAddress`).
        let boundary = c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
            || c.is_uppercase() && prev.is_some_and(char::is_uppercase) && next.is_some_and(char::is_lowercase)
            || c.is_ascii_digit() != prev.is_some_and(|p| p.is_ascii_digit()) && prev.is_some_and(char::is_alphanumeric);
        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Words that say what they are even glued to others in one lowercase word (Postgres folds an
/// unquoted `EmailAddress` to `emailaddress`; `homephone`, `useremail`), longest first.
const GLUED: [&str; 12] = ["firstname", "birthdate", "lastname", "fullname", "birthday", "passport", "address", "surname", "secret", "email", "phone", "token"];

/// Words that hold one of [`GLUED`] and are something else.
const GLUED_NOT: [&str; 17] = [
    "microphone", "headphone", "earphone", "megaphone", "saxophone", "xylophone", "gramophone", "smartphone", "iphone", "phonem", "phonet", "phonic", "phonolog", "secretar", "secretion",
    "tokeniz", "addressable",
];

/// `words` with a word glued from several split around the [`GLUED`] word in it, and a word of
/// [`NOT_THE_VALUE`] after it split off too (`emailverifiedat` is `email verified at`).
fn unglue(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for w in words {
        let found = GLUED.iter().find_map(|g| w.find(g).map(|i| (i, *g))).filter(|(_, g)| w.len() > g.len() && !GLUED_NOT.iter().any(|n| w.contains(n)));
        let Some((i, g)) = found else {
            out.push(w);
            continue;
        };
        let (head, tail) = (&w[..i], &w[i + g.len()..]);
        if !head.is_empty() {
            out.push(head.to_string());
        }
        out.push(g.to_string());
        match NOT_THE_VALUE.iter().find(|n| n.len() >= 4 && tail.len() > n.len() && tail.starts_with(*n)) {
            Some(n) => out.extend([n.to_string(), tail[n.len()..].to_string()]),
            None if !tail.is_empty() => out.push(tail.to_string()),
            None => {}
        }
    }
    out
}

/// Whether `words` holds `phrase` as consecutive words, or (a phrase of several words) glued as
/// one (`firstname`, `phonenumber`).
fn has(words: &[String], phrase: &[&str]) -> bool {
    words.windows(phrase.len()).any(|w| w.iter().zip(phrase).all(|(a, b)| a == b)) || (phrase.len() > 1 && words.iter().any(|w| *w == phrase.concat()))
}

/// The last word, when it says the column is about the value, not the value: when it was set
/// (`email_verified_at`), a reference to another row (`address_id`), a count, a flag or a kind.
const NOT_THE_VALUE: [&str; 24] = [
    "at", "on", "id", "ids", "uuid", "count", "verified", "confirmed", "enabled", "disabled", "sent", "status", "type", "kind", "template", "format", "length", "domain", "provider",
    "expires", "expiry", "version", "policy", "required",
];

/// A flag's first word (`is_email_verified`, `has_phone`).
const FLAG: [&str; 6] = ["is", "has", "can", "should", "allow", "needs"];

/// Words before `address` that make it a machine's, not a postal one (`mac_address`).
const NOT_POSTAL: [&str; 27] = [
    "mac", "wallet", "contract", "host", "server", "node", "peer", "bind", "listen", "proxy", "gateway", "hardware", "memory", "base", "return", "web", "url", "bitcoin", "btc", "eth",
    "ethereum", "crypto", "deposit", "multicast", "broadcast", "token", "validator",
];

/// Words before `id number` that make it a person's (`order_id_number` isn't).
const ID_OF_PERSON: [&str; 13] = ["national", "personal", "citizen", "government", "voter", "resident", "identity", "person", "user", "customer", "member", "employee", "patient"];

/// Tables (or a field's parent object) that are about people: a bare `name` there is a person's.
const PEOPLE: [&str; 30] = [
    "user", "customer", "person", "people", "member", "contact", "employee", "staff", "account", "profile", "author", "patient", "student", "client", "subscriber", "applicant",
    "candidate", "guest", "owner", "recipient", "sender", "buyer", "driver", "passenger", "lead", "visitor", "teacher", "attendee", "tenant", "billing",
];

/// What people's data a column named `name` holds, in `within` (its table, or a search field's
/// index and parent objects: the last part is what's looked at). Names only, case-insensitive.
///
/// Left out on purpose: a username, login or handle, and a password or its hash (they're how a
/// preview's test accounts sign in: masking them locks those out), `filename`/`hostname`/`*_name`
/// of things, `*_at` times, `*_id` references (but a national or tax id), counts, flags and
/// kinds. A bare `name` counts only on a table about people (`users.name`, not `products.name`).
pub fn personal(within: &str, name: &str) -> Option<Personal> {
    let w = unglue(words(name));
    let (first, last) = (w.first()?.as_str(), w.last()?.as_str());
    let any = |phrases: &[&[&str]]| phrases.iter().any(|p| has(&w, p));
    // A person's id number, not another row's (`order_id_number`).
    let id_number = w.iter().enumerate().any(|(i, x)| {
        let at = |j: usize, s: &str| w.get(j).is_some_and(|y| y == s);
        (x == "idnumber" || x == "id" && at(i + 1, "number")) && (i == 0 || ID_OF_PERSON.contains(&w[i - 1].as_str()))
    });
    // Ids that are the person's own, before `*_id` is left out.
    if (id_number || any(&[&["ssn"], &["social", "security"], &["national", "id"], &["national", "insurance"], &["tax", "id"], &["tax", "number"], &["passport"], &["driver", "license"], &["drivers", "license"], &["driving", "licence"], &["personal", "id"]]))
        && !FLAG.contains(&first)
        && (last == "id" || !NOT_THE_VALUE.contains(&last))
    {
        return Some(Personal::NationalId);
    }
    // When someone was born, before `*_at`/`*_on` times are left out.
    if w.len() >= 2 && matches!(last, "at" | "on") && matches!(w[w.len() - 2].as_str(), "born" | "birth") && !FLAG.contains(&first) {
        return Some(Personal::BirthDate);
    }
    if FLAG.contains(&first) || NOT_THE_VALUE.contains(&last) {
        return None;
    }
    let secret = any(&[&["secret"], &["token"], &["api", "key"], &["private", "key"], &["secret", "key"], &["access", "key"], &["otp"], &["totp"], &["mfa", "seed"], &["recovery", "code"], &["recovery", "codes"]]);
    // A password (or its hash, salt or digest) is how test accounts sign in: never suggested.
    if !secret && any(&[&["password"], &["passwd"]]) {
        return None;
    }
    if secret {
        return Some(Personal::Secret);
    }
    // `mail` alone, or a mail address; not a mail server or host.
    if any(&[&["email"], &["mail", "address"]]) || last == "mail" {
        return Some(Personal::Email);
    }
    if any(&[&["ip"], &["ipv", "4"], &["ipv", "6"], &["ip", "address"], &["remote", "addr"]]) {
        return Some(Personal::Ip);
    }
    if any(&[&["iban"], &["card", "number"], &["credit", "card"], &["account", "number"], &["bank", "account"], &["routing", "number"], &["sort", "code"], &["cvv"], &["cvc"]]) {
        return Some(Personal::Financial);
    }
    if any(&[&["phone"], &["telephone"], &["tel"], &["fax"], &["msisdn"], &["cellphone"], &["mobile", "number"], &["mobile", "no"]]) || w == ["mobile"] {
        return Some(Personal::Phone);
    }
    if any(&[&["birth", "date"], &["date", "of", "birth"], &["birthdate"], &["birthday"], &["dob"], &["born"]]) {
        return Some(Personal::BirthDate);
    }
    let postal = w.iter().enumerate().any(|(i, x)| (x == "address" || x == "addr") && (i == 0 || !NOT_POSTAL.contains(&w[i - 1].as_str())));
    if postal || any(&[&["street"], &["city"], &["postcode"], &["postal", "code"], &["zip"], &["zip", "code"], &["house", "number"]]) {
        return Some(Personal::Address);
    }
    let person = ["first", "last", "full", "given", "family", "middle", "maiden", "legal", "display", "contact", "customer", "recipient", "holder", "cardholder", "patient", "employee", "guardian", "sender", "billing", "shipping"];
    if w.len() >= 2 && last == "name" && person.contains(&w[w.len() - 2].as_str()) || any(&[&["surname"], &["forename"], &["firstname"], &["lastname"], &["fullname"]]) {
        return Some(Personal::Name);
    }
    if w.len() == 1 && first == "name" {
        let table = within.rsplit('.').next().unwrap_or(within);
        let about_people = words(table).iter().any(|t| {
            let one = t.strip_suffix('s').unwrap_or(t);
            PEOPLE.contains(&t.as_str()) || PEOPLE.contains(&one)
        });
        return about_people.then_some(Personal::Name);
    }
    None
}

/// A Postgres type as `format_type` writes it (`character varying(11)`, `timestamp(3) with time
/// zone`, `public.citext`, `bigint[]`): its name without the modifier or schema, the length in
/// the modifier, and whether it's an array.
fn pg_type(ty: &str) -> (String, Option<usize>, bool) {
    let ty = ty.trim().to_ascii_lowercase();
    let array = ty.ends_with("[]");
    let ty = ty.trim_end_matches("[]");
    let (name, len) = match (ty.find('('), ty.find(')')) {
        (Some(a), Some(b)) if a < b => {
            let len = ty[a + 1..b].split(',').next().and_then(|n| n.trim().parse().ok());
            (format!("{} {}", ty[..a].trim(), ty[b + 1..].trim()).trim().to_string(), len)
        }
        _ => (ty.to_string(), None),
    };
    let name = if name.starts_with('"') { name } else { name.rsplit('.').next().unwrap_or(&name).to_string() };
    (name, len, array)
}

/// Postgres's own types a string is written into.
const PG_TEXT: [&str; 7] = ["text", "citext", "character varying", "varchar", "character", "char", "bpchar"];

/// Postgres's built-in ranges and multiranges: no rule but `null` fits one.
const PG_RANGES: [&str; 12] = [
    "int4range", "int8range", "numrange", "tsrange", "tstzrange", "daterange", "int4multirange", "int8multirange", "nummultirange", "tsmultirange", "tstzmultirange", "datemultirange",
];

/// The most characters a text rule writes (`email`: `user-` + 12 + `@example.invalid`; `hash`:
/// an md5).
fn width(rule: &str) -> usize {
    match rule {
        "email" => 33,
        "hash" => 32,
        "redact" => 3,
        _ => 0,
    }
}

/// Whether a column of `kind` with type `ty` holds text (`None`: the product didn't say).
fn is_text(kind: crate::Kind, ty: Option<&str>) -> bool {
    let Some(ty) = ty else { return true };
    match kind {
        crate::Kind::Postgres => {
            let (name, _, array) = pg_type(ty);
            !array && PG_TEXT.contains(&name.as_str())
        }
        crate::Kind::Search => SEARCH_TEXT.contains(&ty),
        crate::Kind::Nosql => ["text", "varchar", "ascii"].contains(&ty.to_ascii_lowercase().as_str()),
        _ => false,
    }
}

/// The rule a Postgres column holding `what` takes: what its type and constraints can hold
/// (`redact` and `hash` write text that must fit its length, `null` needs a nullable column),
/// one value per row where a unique index covers it, nothing where only a null fits and it
/// can't hold one.
fn postgres_rule(what: Personal, c: &Column) -> Option<String> {
    let null = || (!c.not_null).then(|| "null".to_string());
    let (name, len, array) = pg_type(c.r#type.as_deref().unwrap_or("text"));
    match c.takes {
        Takes::Nothing => return None,
        Takes::Null => return null(),
        Takes::Any if array || PG_RANGES.contains(&name.as_str()) => return null(),
        Takes::Any => {}
    }
    if PG_TEXT.contains(&name.as_str()) {
        let tried: &[&str] = match what {
            Personal::Secret if !c.not_null => &["null"],
            Personal::Email if c.unique => &["email", "hash"],
            Personal::Email => &["email", "redact"],
            _ if c.unique => &["hash"],
            Personal::NationalId => &["hash", "redact"],
            _ => &["redact"],
        };
        // The first that fits the column's length; else a null where it can hold one.
        return tried.iter().find(|r| len.is_none_or(|n| width(r) <= n)).map(|r| r.to_string()).or_else(null);
    }
    if !c.not_null {
        return null();
    }
    // A column that must keep a value of its type: one that type takes, where one per row will do.
    if c.unique {
        return None;
    }
    let fixed = match name.as_str() {
        "date" | "timestamp without time zone" | "timestamp with time zone" | "timestamp" | "timestamptz" => "1970-01-01",
        "inet" | "cidr" => "0.0.0.0",
        "smallint" | "integer" | "bigint" | "numeric" | "decimal" | "real" | "double precision" => "0",
        _ => return None,
    };
    Some(format!("fixed:{fixed}"))
}

/// The rule a search field holding `what` takes: `redact` and `email` write a string, so only into
/// a text-like field ([`search_field_masks`]); the others are removed.
fn search_rule(what: Personal, c: &Column) -> String {
    let text = c.r#type.as_deref().is_none_or(|t| SEARCH_TEXT.contains(&t));
    match what {
        Personal::Secret => "remove",
        Personal::Email if text => "email",
        _ if text => "redact",
        _ => "remove",
    }
    .into()
}

/// The rule a NoSQL column holding `what` takes: a Cassandra mask function, which any column's
/// type takes (`mask_hash` isn't suggested: it changes the type a client reads).
fn nosql_rule(what: Personal) -> String {
    match what {
        Personal::Secret => "mask_null()",
        _ => "mask_default()",
    }
    .into()
}

/// Whether column `c` of a `kind` parent looks like it holds people's data, and the rule for it
/// (`None`: it doesn't, or no rule fits its type). Booleans never do, and a `*_id` is a national
/// or tax id only when it holds text (a number is a reference to another row).
pub fn suggest(kind: crate::Kind, c: &Column) -> Option<Suggestion> {
    if c.r#type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("boolean") || t.eq_ignore_ascii_case("bool")) {
        return None;
    }
    let (within, name) = c.path.rsplit_once('.')?;
    let what = personal(within, name)?;
    if what == Personal::NationalId && unglue(words(name)).last().is_some_and(|l| l == "id") && !is_text(kind, c.r#type.as_deref()) {
        return None;
    }
    let rule = match kind {
        crate::Kind::Postgres => postgres_rule(what, c)?,
        crate::Kind::Search => search_rule(what, c),
        crate::Kind::Nosql => nosql_rule(what),
        _ => return None,
    };
    Some(Suggestion { rule, why: what.why() })
}

/// A NoSQL schema's columns (roll nosql's `/schema`) that can be masked: not the system
/// keyspaces', and not a column of a table's key or of the key of a view of it
/// ([`nosql_key_masks`]). At most `max`.
pub fn nosql_columns(schema: &serde_json::Value, max: usize) -> Vec<Column> {
    let mut out = Vec::new();
    for k in schema["keyspaces"].as_array().into_iter().flatten() {
        let ks = k["name"].as_str().unwrap_or_default();
        if ks.is_empty() || ks.starts_with("system") {
            continue;
        }
        for t in k["tables"].as_array().into_iter().flatten() {
            let table = t["name"].as_str().unwrap_or_default();
            for c in t["columns"].as_array().into_iter().flatten() {
                let Some(name) = c["name"].as_str() else { continue };
                if out.len() >= max {
                    return out;
                }
                let path = format!("{ks}.{table}.{name}");
                if name.contains('.') || !nosql_key_masks(schema, &[(path.clone(), String::new())].into_iter().collect()).is_empty() {
                    continue;
                }
                out.push(Column { path, r#type: c["type"].as_str().map(str::to_string), ..Column::default() });
            }
        }
    }
    out
}

/// Whether a search index's fields are offered for masks: not a system index (`.…`), but a data
/// stream's backing index (`.ds-…`) is the stream's documents.
pub fn search_index_offered(index: &str) -> bool {
    !index.starts_with('.') || index.starts_with(".ds-")
}

/// A `_mapping` answer's fields (`index.a.b`), leaves only (an object's fields are walked; a
/// multi-field such as `name.raw` is the same value, masked with it), of the indices
/// [`search_index_offered`] offers. At most `max`.
pub fn search_columns(mapping: &serde_json::Value, max: usize) -> Vec<Column> {
    fn walk(props: &serde_json::Value, prefix: &str, depth: usize, max: usize, out: &mut Vec<Column>) {
        for (name, f) in props.as_object().into_iter().flatten() {
            if out.len() >= max || depth > 20 {
                return;
            }
            let path = format!("{prefix}.{name}");
            match f.get("properties") {
                Some(p) => walk(p, &path, depth + 1, max, out),
                None => out.push(Column { path, r#type: f["type"].as_str().map(str::to_string), ..Column::default() }),
            }
        }
    }
    let mut out = Vec::new();
    for (index, m) in mapping.as_object().into_iter().flatten().filter(|(i, _)| search_index_offered(i)) {
        walk(&m["mappings"]["properties"], index, 0, max, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {

    use std::collections::BTreeMap;

    /// Rules become one transaction per branch, names and values quoted whatever they hold.
    #[test]
    fn rules_become_one_quoted_transaction() {
        let cols: BTreeMap<String, String> = [
            ("public.users.email", "email"),
            ("public.users.name", "fixed:it's; DROP TABLE users; --"),
            ("app.we\"ird.ip", "null"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let p = super::postgres(&cols, "s4lt").unwrap();
        assert!(p.update.starts_with("BEGIN;\n") && p.update.ends_with("COMMIT;"), "{}", p.update);
        assert!(p.update.contains("SET LOCAL search_path = pg_catalog, pg_temp"), "only the catalog's names: {}", p.update);
        assert!(p.update.contains("DISABLE TRIGGER USER") && p.update.contains("ENABLE ALWAYS"), "triggers off, then back as they were: {}", p.update);
        assert!(!p.update.contains("session_replication_role"), "REPLICA triggers would fire: {}", p.update);
        assert!(p.update.contains("md5('s4lt' || \"email\"::text)"), "salted: {}", p.update);
        assert!(p.update.contains(r#"UPDATE "public"."users" SET "email" = CASE WHEN "email" IS NULL"#), "{}", p.update);
        assert!(p.update.contains(r#""name" = 'it''s; DROP TABLE users; --'"#), "a value stays one literal: {}", p.update);
        assert!(p.update.contains(r#"UPDATE "app"."we""ird" SET "ip" = NULL"#), "a quote in a name stays a name: {}", p.update);
        assert_eq!(p.after.len(), 4, "two tables' vacuums, the views, a checkpoint");
        assert!(super::postgres(&[("a.b.c".to_string(), "mask_default()".to_string())].into_iter().collect(), "s").is_err());
    }

}
