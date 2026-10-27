//! ScyllaDB's cache of parsed expressions (`alternator_max_expression_cache_entries_per_shard`,
//! 2000; live). Parsed update, condition (also key condition and filter) and projection
//! expressions are kept by kind and text in an LRU of that many entries: a request repeating an
//! expression skips the parser. `scylla_alternator_expression_cache_hits` / `_misses` count
//! lookups per kind (label `expression`: `UpdateExpression`, `ConditionExpression`,
//! `ProjectionExpression`); `_evictions` counts entries pushed out by new ones. An expression
//! that doesn't parse is counted as a miss and not kept. Setting the size (even to the same
//! value) empties the cache, so `0` then `n` flushes it, as the suite does.
//!
//! The cache of the request's database is in scope while an operation runs ([`scope`]), so the
//! parsing helpers below use it without being handed it; outside a scope they just parse.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;

use parking_lot::Mutex;

use super::ast::{Cond, Path, Update};
use super::parse::{ParseFailure, parse_condition, parse_projection, parse_update};
use crate::metrics::Metrics;

/// What an expression is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Update,
    Condition,
    Projection,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Update => "UpdateExpression",
            Kind::Condition => "ConditionExpression",
            Kind::Projection => "ProjectionExpression",
        }
    }
}

#[derive(Clone)]
enum Parsed {
    Update(Arc<Update>),
    Condition(Arc<Cond>),
    Projection(Arc<Vec<Path>>),
}

/// The LRU of one database's Alternator.
pub struct Cache {
    inner: Mutex<Inner>,
    metrics: Arc<Metrics>,
}

struct Inner {
    capacity: usize,
    /// Entry and its last use.
    map: HashMap<(Kind, String), (Parsed, u64)>,
    /// Entries by last use (oldest first).
    order: BTreeMap<u64, (Kind, String)>,
    tick: u64,
}

impl Cache {
    pub fn new(capacity: usize, metrics: Arc<Metrics>) -> Cache {
        Cache {
            inner: Mutex::new(Inner {
                capacity,
                map: HashMap::new(),
                order: BTreeMap::new(),
                tick: 0,
            }),
            metrics,
        }
    }

    /// Sets the size and empties the cache.
    pub fn set_capacity(&self, capacity: usize) {
        let mut i = self.inner.lock();
        i.capacity = capacity;
        i.map.clear();
        i.order.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(
        &self,
        kind: Kind,
        src: &str,
        parse: impl FnOnce(&str) -> Result<Parsed, ParseFailure>,
    ) -> Result<Parsed, ParseFailure> {
        let label = [("expression", kind.label())];
        {
            let mut i = self.inner.lock();
            i.tick += 1;
            let tick = i.tick;
            let key = (kind, src.to_string());
            if let Some((p, used)) = i.map.get_mut(&key) {
                let (p, old) = (p.clone(), *used);
                *used = tick;
                i.order.remove(&old);
                i.order.insert(tick, key);
                drop(i);
                self.metrics
                    .add("scylla_alternator_expression_cache_hits", &label, 1.0);
                return Ok(p);
            }
        }
        self.metrics
            .add("scylla_alternator_expression_cache_misses", &label, 1.0);
        let parsed = parse(src)?;
        let mut evicted = 0;
        {
            let mut i = self.inner.lock();
            if i.capacity > 0 {
                while i.map.len() >= i.capacity {
                    let Some((_, key)) = i.order.pop_first() else {
                        break;
                    };
                    i.map.remove(&key);
                    evicted += 1;
                }
                i.tick += 1;
                let tick = i.tick;
                let key = (kind, src.to_string());
                i.order.insert(tick, key.clone());
                i.map.insert(key, (parsed.clone(), tick));
            }
        }
        if evicted > 0 {
            self.metrics.add(
                "scylla_alternator_expression_cache_evictions",
                &[],
                evicted as f64,
            );
        }
        Ok(parsed)
    }

    /// Whether `src` is cached, without counting a lookup.
    fn contains(&self, kind: Kind, src: &str) -> bool {
        self.inner.lock().map.contains_key(&(kind, src.to_string()))
    }
}

tokio::task_local! {
    static CACHE: Arc<Cache>;
}

/// Runs `f` with `cache` as the expression cache of the parsing helpers.
pub async fn scope<F: Future>(cache: Arc<Cache>, f: F) -> F::Output {
    CACHE.scope(cache, f).await
}

fn lookup(
    kind: Kind,
    src: &str,
    parse: impl FnOnce(&str) -> Result<Parsed, ParseFailure>,
) -> Result<Parsed, ParseFailure> {
    match CACHE.try_with(Arc::clone) {
        Ok(c) => c.get(kind, src, parse),
        Err(_) => parse(src),
    }
}

/// A condition, key condition or filter expression.
pub fn condition(src: &str) -> Result<Arc<Cond>, ParseFailure> {
    match lookup(Kind::Condition, src, |s| {
        parse_condition(s).map(|c| Parsed::Condition(Arc::new(c)))
    })? {
        Parsed::Condition(c) => Ok(c),
        _ => unreachable!("condition entries hold conditions"),
    }
}

/// Checks a condition's syntax without counting a lookup (UpdateItem checks it before its
/// keys, then parses it with the rest of the request).
pub fn check_condition(src: &str) -> Result<(), ParseFailure> {
    if CACHE
        .try_with(|c| c.contains(Kind::Condition, src))
        .unwrap_or(false)
    {
        return Ok(());
    }
    parse_condition(src).map(|_| ())
}

pub fn update(src: &str) -> Result<Arc<Update>, ParseFailure> {
    match lookup(Kind::Update, src, |s| {
        parse_update(s).map(|u| Parsed::Update(Arc::new(u)))
    })? {
        Parsed::Update(u) => Ok(u),
        _ => unreachable!("update entries hold updates"),
    }
}

pub fn projection(src: &str) -> Result<Arc<Vec<Path>>, ParseFailure> {
    match lookup(Kind::Projection, src, |s| {
        parse_projection(s).map(|p| Parsed::Projection(Arc::new(p)))
    })? {
        Parsed::Projection(p) => Ok(p),
        _ => unreachable!("projection entries hold projections"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(m: &Metrics, name: &str) -> f64 {
        m.render()
            .lines()
            .filter(|l| l.starts_with(name))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .sum()
    }

    #[tokio::test]
    async fn lru_with_metrics() {
        let m = Arc::new(Metrics::default());
        let c = Arc::new(Cache::new(2, m.clone()));
        scope(c.clone(), async {
            update("SET a = :a").unwrap();
            update("SET a = :a").unwrap();
            condition("a = :a").unwrap();
            // Full: B evicts the least recently used (`SET a = :a`).
            update("SET a = :b").unwrap();
            update("SET a = :a").unwrap();
            assert!(update("SET a = ").is_err());
            check_condition("a = :a").unwrap();
            assert!(check_condition("a = ").is_err());
            projection("a, b").unwrap();
        })
        .await;
        assert_eq!(metric(&m, "scylla_alternator_expression_cache_hits"), 1.0);
        // Every parse counts, the failed one included; `check_condition` doesn't.
        assert_eq!(metric(&m, "scylla_alternator_expression_cache_misses"), 6.0);
        assert_eq!(
            metric(&m, "scylla_alternator_expression_cache_evictions"),
            3.0
        );
        assert!(m.render().contains(
            "scylla_alternator_expression_cache_hits{expression=\"UpdateExpression\",shard=\"0\"} 1"
        ));
        assert_eq!(c.len(), 2);
        c.set_capacity(0);
        assert!(c.is_empty());
        scope(c.clone(), async {
            update("SET a = :a").unwrap();
            update("SET a = :a").unwrap();
        })
        .await;
        assert!(c.is_empty());
        assert_eq!(metric(&m, "scylla_alternator_expression_cache_hits"), 1.0);
        // Outside a scope nothing is counted.
        update("SET a = :a").unwrap();
        assert_eq!(metric(&m, "scylla_alternator_expression_cache_misses"), 8.0);
    }
}
