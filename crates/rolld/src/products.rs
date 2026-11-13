//! Which products this process runs (`ROLL_PRODUCTS`; docs/cloud/README.md, "Node settings").
//!
//! One set decides what starts: `--products` (default: every product), narrowed by the older
//! switches, which are shorthands over it: `--no-storage`, `--no-queues`, `--no-nosql` and
//! `--no-search` take one product out, and `--code-only`, `--queues-only`, `--nosql-only` and
//! `--search-only` leave just one. A product
//! that's off binds no port, starts no background work, and its HTTP routes answer 404 saying
//! so ([`Products::off_routes`]); with Postgres off rolld never runs `docker` for computes.

use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use clap::ValueEnum;
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum)]
pub enum Product {
    Postgres,
    Redis,
    Streams,
    Tasks,
    Storage,
    Nosql,
    Queues,
    Code,
    Apps,
    Search,
}

impl Product {
    pub const ALL: [Product; 10] = [Product::Postgres, Product::Redis, Product::Streams, Product::Tasks, Product::Storage, Product::Nosql, Product::Queues, Product::Code, Product::Apps, Product::Search];

    pub fn name(self) -> &'static str {
        match self {
            Product::Postgres => "postgres",
            Product::Redis => "redis",
            Product::Streams => "streams",
            Product::Tasks => "tasks",
            Product::Storage => "storage",
            Product::Nosql => "nosql",
            Product::Queues => "queues",
            Product::Code => "code",
            Product::Apps => "apps",
            Product::Search => "search",
        }
    }

    /// The internal API's path prefixes this product serves. One table: off products' 404s and
    /// the product an access line and its metrics are counted under both come from it.
    pub fn prefixes(self) -> &'static [&'static str] {
        match self {
            Product::Postgres => &["/v1/projects", "/v1/endpoints", "/v1/health", "/internal/wal"],
            Product::Redis => &["/v1/redis"],
            Product::Streams => &["/v1/streams", "/v1/kafka", "/kafka/v3", "/internal/streams"],
            Product::Tasks => &["/v1/tasks", "/t", "/_roll/tenants", "/internal/tasks"],
            Product::Storage => &["/v1/storage"],
            Product::Nosql => &["/v1/nosql", "/internal/nosql"],
            Product::Queues => &["/v1/queues"],
            Product::Code => &["/v1/code", "/git", "/internal/code"],
            Product::Apps => &["/v1/apps"],
            Product::Search => &["/v1/search", "/internal/search"],
        }
    }

    /// The product a route (a matched pattern or a path) belongs to, if any.
    pub fn of_route(route: &str) -> Option<Product> {
        Product::ALL.into_iter().find(|p| {
            p.prefixes().iter().any(|pre| route.strip_prefix(pre).is_some_and(|rest| rest.is_empty() || rest.starts_with('/')))
        })
    }
}

impl std::fmt::Display for Product {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The older switches, as given.
#[derive(Default)]
pub struct Switches {
    pub products: Option<Vec<Product>>,
    pub no_storage: bool,
    pub no_queues: bool,
    pub no_nosql: bool,
    pub no_search: bool,
    pub code_only: bool,
    pub queues_only: bool,
    pub nosql_only: bool,
    pub search_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Products(u16);

impl Products {
    pub fn all() -> Products {
        Products::from_iter(Product::ALL)
    }

    pub fn resolve(s: Switches) -> anyhow::Result<Products> {
        let only: Vec<Product> = [(s.code_only, Product::Code), (s.queues_only, Product::Queues), (s.nosql_only, Product::Nosql), (s.search_only, Product::Search)].into_iter().filter(|(on, _)| *on).map(|(_, p)| p).collect();
        anyhow::ensure!(only.len() <= 1, "--code-only, --queues-only, --nosql-only and --search-only can't be combined: use ROLL_PRODUCTS={}", only.iter().map(|p| p.name()).collect::<Vec<_>>().join(","));
        let mut set = match (&s.products, only.first()) {
            (Some(_), Some(p)) => anyhow::bail!("--{p}-only and ROLL_PRODUCTS (--products) can't both be set: ROLL_PRODUCTS={p} is the same"),
            (Some(list), None) => Products::from_iter(list.iter().copied()),
            (None, Some(p)) => Products::from_iter([*p]),
            (None, None) => Products::all(),
        };
        for (off, p) in [(s.no_storage, Product::Storage), (s.no_queues, Product::Queues), (s.no_nosql, Product::Nosql), (s.no_search, Product::Search)] {
            if off {
                anyhow::ensure!(only.first() != Some(&p), "--{p}-only with --no-{p} leaves nothing to run");
                set.0 &= !Self::bit(p);
            }
        }
        anyhow::ensure!(set.0 != 0, "no product is on (ROLL_PRODUCTS): name at least one of {}", Product::ALL.map(Product::name).join(","));
        Ok(set)
    }

    fn bit(p: Product) -> u16 {
        1 << p as u16
    }

    pub fn on(self, p: Product) -> bool {
        self.0 & Self::bit(p) != 0
    }

    pub fn iter(self) -> impl Iterator<Item = Product> {
        Product::ALL.into_iter().filter(move |p| self.on(*p))
    }

    pub fn off(self) -> impl Iterator<Item = Product> {
        Product::ALL.into_iter().filter(move |p| !self.on(*p))
    }

    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(Product::name).collect()
    }

    /// Routes for every product that's off: 404 with a body that says so, instead of an empty
    /// 404 a client would take for a missing resource.
    pub fn off_routes(self) -> axum::Router {
        let mut r = axum::Router::new();
        for p in self.off() {
            let off = move || async move {
                let msg = format!("roll {p} is off on this node (ROLL_PRODUCTS)");
                (StatusCode::NOT_FOUND, Json(json!({ "error": msg, "product": p.name() }))).into_response()
            };
            for pre in p.prefixes() {
                r = r.route(pre, axum::routing::any(off)).route(&format!("{pre}/{{*rest}}"), axum::routing::any(off));
            }
        }
        r
    }

    /// `GET /v1/info`: this node's version and the products it runs (the dashboard hides the
    /// rest).
    pub fn info_route(self, node: String) -> axum::Router {
        let body = json!({ "version": env!("CARGO_PKG_VERSION"), "node": node, "products": self.names() });
        axum::Router::new().route("/v1/info", axum::routing::get(move || async move { Json(body.clone()) }))
    }
}

impl FromIterator<Product> for Products {
    fn from_iter<I: IntoIterator<Item = Product>>(iter: I) -> Self {
        Products(iter.into_iter().fold(0, |acc, p| acc | Self::bit(p)))
    }
}

impl std::fmt::Display for Products {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(s: Switches) -> anyhow::Result<String> {
        Products::resolve(s).map(|p| p.to_string())
    }

    #[test]
    fn the_old_switches_are_shorthands_over_one_set() {
        assert_eq!(resolve(Switches::default()).unwrap(), "postgres,redis,streams,tasks,storage,nosql,queues,code,apps,search");
        assert_eq!(resolve(Switches { products: Some(vec![Product::Code, Product::Redis]), ..Default::default() }).unwrap(), "redis,code");
        assert_eq!(resolve(Switches { no_storage: true, no_queues: true, ..Default::default() }).unwrap(), "postgres,redis,streams,tasks,nosql,code,apps,search");
        assert_eq!(resolve(Switches { code_only: true, ..Default::default() }).unwrap(), "code");
        assert_eq!(resolve(Switches { nosql_only: true, no_queues: true, ..Default::default() }).unwrap(), "nosql");
        assert!(resolve(Switches { nosql_only: true, no_nosql: true, ..Default::default() }).is_err());
        assert!(resolve(Switches { code_only: true, queues_only: true, ..Default::default() }).is_err());
        assert!(resolve(Switches { code_only: true, products: Some(vec![Product::Code]), ..Default::default() }).is_err());
        assert!(resolve(Switches { products: Some(vec![Product::Queues]), no_queues: true, ..Default::default() }).is_err(), "nothing left");
    }

    #[test]
    fn routes_belong_to_their_product() {
        assert_eq!(Product::of_route("/v1/code/repos/{owner}/{repo}"), Some(Product::Code));
        assert_eq!(Product::of_route("/git/{*rest}"), Some(Product::Code));
        assert_eq!(Product::of_route("/v1/health"), Some(Product::Postgres));
        assert_eq!(Product::of_route("/v1/healthz"), None);
        assert_eq!(Product::of_route("/kafka/v3/clusters"), Some(Product::Streams));
        assert_eq!(Product::of_route("/v1/search/clusters/{id}"), Some(Product::Search));
        assert_eq!(Product::of_route("/v1/info"), None);
        assert_eq!(Product::of_route("/v1/workspace/projects/{id}"), None, "workspace projects are always on");
    }
}
