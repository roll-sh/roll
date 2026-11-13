//! What the code talks to: Postgres or Redis client libraries in its manifests, so New service
//! can offer to make the database with it (Railway's and Render's templates, without a
//! template). Only the root's manifests are read (plus `Cargo.lock`, since a Rust workspace's
//! dependencies are in its members), each bounded by [`crate::tree::MAX_READ`]; a guess is
//! only ever a suggestion the user ticks.

use crate::Tree;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// A datastore an app uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Datastore {
    Postgres,
    Redis,
}

impl Datastore {
    pub fn label(self) -> &'static str {
        match self {
            Datastore::Postgres => "Postgres",
            Datastore::Redis => "Redis",
        }
    }
}

const NODE_PG: &[&str] = &["pg", "postgres", "pg-promise", "@vercel/postgres", "slonik", "@neondatabase/serverless"];
const NODE_REDIS: &[&str] = &["redis", "ioredis", "bullmq", "bull", "connect-redis", "@socket.io/redis-adapter"];
const PY_PG: &[&str] = &["psycopg", "psycopg2", "psycopg2-binary", "psycopg-binary", "asyncpg", "pg8000"];
const PY_REDIS: &[&str] = &["redis", "rq", "django-redis", "aioredis", "arq"];
const RUBY_PG: &[&str] = &["pg"];
const RUBY_REDIS: &[&str] = &["redis", "sidekiq", "resque", "redis-client"];
const GO_PG: &[&str] = &["github.com/jackc/pgx", "github.com/lib/pq"];
const GO_REDIS: &[&str] = &["github.com/redis/go-redis", "github.com/go-redis/redis", "github.com/gomodule/redigo"];
const RUST_PG: &[&str] = &["tokio-postgres", "postgres", "sqlx-postgres", "pq-sys"];
const RUST_REDIS: &[&str] = &["redis", "fred"];
const PHP_PG: &[&str] = &["ext-pgsql", "ext-pdo_pgsql"];
const PHP_REDIS: &[&str] = &["ext-redis", "predis/predis"];
const JAVA_PG: &[&str] = &["org.postgresql"];
const JAVA_REDIS: &[&str] = &["redis.clients", "io.lettuce", "spring-boot-starter-data-redis"];

/// The datastores `tree` uses, each with what said so (for the plan's notes).
pub fn uses(tree: &dyn Tree) -> Vec<(Datastore, String)> {
    let mut found: Vec<(Datastore, String)> = Vec::new();
    let mut hit = |names: &BTreeSet<String>, pg: &[&str], redis: &[&str], file: &str| {
        for (store, list) in [(Datastore::Postgres, pg), (Datastore::Redis, redis)] {
            if found.iter().any(|(s, _)| *s == store) {
                continue;
            }
            if let Some(n) = list.iter().find(|n| names.contains(**n)) {
                found.push((store, format!("{n} in {file}")));
            }
        }
    };
    if let Some(v) = json(tree, "package.json") {
        hit(&keys(&v, &["dependencies"]), NODE_PG, NODE_REDIS, "package.json");
    }
    if let Some(v) = json(tree, "composer.json") {
        hit(&keys(&v, &["require"]), PHP_PG, PHP_REDIS, "composer.json");
    }
    for file in ["requirements.txt", "pyproject.toml", "Pipfile"] {
        if let Some(t) = text(tree, file) {
            hit(&python_names(&t), PY_PG, PY_REDIS, file);
        }
    }
    if let Some(t) = text(tree, "Gemfile") {
        hit(&gems(&t), RUBY_PG, RUBY_REDIS, "Gemfile");
    }
    if let Some(t) = text(tree, "go.mod") {
        hit(&go_modules(&t), GO_PG, GO_REDIS, "go.mod");
    }
    for file in ["Cargo.lock", "Cargo.toml"] {
        if let Some(t) = text(tree, file) {
            hit(&crates(&t), RUST_PG, RUST_REDIS, file);
        }
    }
    for file in ["pom.xml", "build.gradle", "build.gradle.kts"] {
        if let Some(t) = text(tree, file) {
            hit(&java_words(&t), JAVA_PG, JAVA_REDIS, file);
        }
    }
    found.sort();
    found
}

fn text(tree: &dyn Tree, path: &str) -> Option<String> {
    tree.read(path).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn json(tree: &dyn Tree, path: &str) -> Option<Value> {
    serde_json::from_slice(&tree.read(path)?).ok()
}

fn keys(v: &Value, fields: &[&str]) -> BTreeSet<String> {
    fields.iter().filter_map(|f| v.get(f)?.as_object()).flat_map(|o| o.keys().cloned()).collect()
}

/// A Python distribution name as pip compares them (PEP 503): lower case, `_`/`.` as `-`.
fn py_norm(s: &str) -> String {
    s.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// Names in requirements.txt (a name at a line's start), pyproject.toml (PEP 621's quoted
/// requirements, Poetry's and uv's keys) and a Pipfile (keys).
fn python_names(t: &str) -> BTreeSet<String> {
    let name = |s: &str| -> Option<String> {
        let n: String = s.trim_start().chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
        (!n.is_empty()).then(|| py_norm(&n))
    };
    let mut out = BTreeSet::new();
    for line in t.lines() {
        let line = line.split('#').next().unwrap_or("");
        out.extend(name(line));
        // `"psycopg[binary]>=3"` inside a list.
        for (i, part) in line.split(['"', '\'']).enumerate() {
            if i % 2 == 1 {
                out.extend(name(part));
            }
        }
    }
    out
}

fn gems(t: &str) -> BTreeSet<String> {
    t.lines()
        .filter_map(|l| l.trim_start().strip_prefix("gem "))
        .filter_map(|rest| rest.trim_start().split(['"', '\'']).nth(1))
        .map(String::from)
        .collect()
}

/// Module paths in go.mod, and each of their parents (`github.com/jackc/pgx/v5` also counts as
/// `github.com/jackc/pgx`).
fn go_modules(t: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for word in t.split_whitespace().filter(|w| w.contains('/')) {
        let mut path = word.to_string();
        loop {
            out.insert(path.clone());
            match path.rfind('/') {
                Some(i) => path.truncate(i),
                None => break,
            }
        }
    }
    out
}

/// Crates in Cargo.lock (`name = "…"`) or Cargo.toml (dependency keys), and a feature named
/// `postgres` on sqlx, diesel or sea-orm as `sqlx-postgres`.
fn crates(t: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in t.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("name = \"") {
            out.extend(rest.split('"').next().map(String::from));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().trim_matches('"');
        if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !key.is_empty() {
            out.insert(key.to_string());
            if matches!(key, "sqlx" | "diesel" | "sea-orm") && value.contains("postgres") {
                out.insert("sqlx-postgres".into());
            }
        }
    }
    out
}

/// Maven groups and artifacts, Gradle coordinates: every dotted or dashed word.
fn java_words(t: &str) -> BTreeSet<String> {
    t.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .filter(|w| !w.is_empty())
        .flat_map(|w| {
            // `org.postgresql` as a group, and `redis.clients` from `redis.clients.jedis`.
            let parts: Vec<&str> = w.split('.').collect();
            (1..=parts.len()).map(move |n| parts[..n].join(".")).collect::<Vec<_>>()
        })
        .collect()
}
