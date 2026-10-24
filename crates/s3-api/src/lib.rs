//! roll storage's developer API: a Supabase-Storage-compatible REST API on its own listener, over
//! the same [`s3_store::Store`] and [`s3_server::Server`] as the S3 API.
//!
//! `@supabase/storage-js` works against it unchanged (`<host>/storage/v1`): buckets, uploads
//! (raw bodies and forms, `x-upsert`), downloads (public, authenticated, signed, ranges,
//! `?download=`), object info, lists, moves, copies, deletes, signed download and upload URLs,
//! TUS resumable uploads (`/upload/resumable`) and image transformations (`/render/image/{public|
//! authenticated|sign}`, `render.rs`, over `s3_server::render`; signed URLs bind their
//! transformation). roll's extras (versions, the trash, branches, bucket policies, rule presets
//! and public URLs) are under `/roll/...`.
//!
//! - **CDN** (`cdn.rs`): Cloudflare's Worker fetches the public and signed reads with the
//!   shared secret (anonymous or token-only; metered as origin pulls), and immutable URLs
//!   (`/v/<token>/<bucket>/<key>`) serve one blob for a year.
//!
//! - **Auth** (`auth.rs`): `apikey` and `Authorization: Bearer`. A project (an account) has an
//!   anon key and a service key (HS256 JWTs signed by a deployment key, [`s3_store::api`]); user
//!   tokens come from the project's auth providers (an HS256 secret like Supabase's JWT secret,
//!   or OIDC issuers through `s3-server`'s JWKS verification).
//! - **Rules**: bucket policies, decided by the S3 API's own evaluator ([`s3_server::app`]) with
//!   the JWT's claims as condition keys (`${jwt:sub}`), `{"App": "authenticated"}` principals and
//!   `roll:ObjectOwner`. The service key acts as the account's root user.
//! - **Writes** go through the S3 API's write path (`s3_server::put_object`, the store's multipart
//!   uploads): one customer object is one blob, committed only once it's durable; bucket limits
//!   are enforced by the store on every write path.
//! - **Metering** is the S3 API's: uploads, lists, copies and bucket writes are class A, reads
//!   class B, deletes free, plus bytes in and egress (internal or internet by the peer).
//!
//! ```ignore
//! let api = s3_api::Api::new(server, s3_api::Config::default());
//! s3_api::serve(tokio::net::TcpListener::bind("127.0.0.1:59001").await?, api).await?;
//! ```

mod auth;
mod bucket;
mod cdn;
mod error;
mod extras;
mod form;
mod object;
mod render;
mod req;
mod sign;
mod tus;
mod util;

pub use error::{ApiError, ApiResult};

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use s3_server::Server;
use s3_store::Store;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where the API is mounted (as Supabase's gateway mounts it); requests without it work too.
pub const PREFIX: &str = "/storage/v1";

#[derive(Clone, Debug)]
pub struct Config {
    /// This API's public URL (`https://storage.example.com/storage/v1`), for the upload URLs
    /// TUS hands out. `None`: the request's `Host` (and `X-Forwarded-Proto` from a proxy inside
    /// roll).
    pub public_url: Option<String>,
    /// Where uploads of unknown length are spooled and resumable uploads buffer their next part.
    pub spool_dir: PathBuf,
    /// Largest standard upload (one PUT; larger objects use resumable uploads).
    pub max_upload: u64,
    /// Largest resumable upload.
    pub max_resumable: u64,
    /// How long a resumable upload may stay unfinished.
    pub resumable_expiry: Duration,
    /// How often expired resumable uploads are cleaned up (`None`: never, tests).
    pub sweep_interval: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            public_url: None,
            spool_dir: std::env::temp_dir(),
            max_upload: 5 << 30,
            max_resumable: 5 << 40,
            resumable_expiry: Duration::from_secs(24 * 3600),
            sweep_interval: Some(Duration::from_secs(600)),
        }
    }
}

/// The developer API: the S3 API's server state plus its own.
pub struct Api {
    pub server: Arc<Server>,
    pub cfg: Config,
    /// One request at a time per resumable upload.
    pub(crate) tus_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Api {
    pub fn new(server: Arc<Server>, cfg: Config) -> Arc<Self> {
        let _ = std::fs::create_dir_all(tus::buffer_dir(&cfg.spool_dir));
        let api = Arc::new(Self { server, cfg, tus_locks: Default::default() });
        if let (Some(every), Ok(rt)) = (api.cfg.sweep_interval, tokio::runtime::Handle::try_current()) {
            let weak = Arc::downgrade(&api);
            rt.spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(api) = weak.upgrade() else { return };
                    api.sweep_resumable().await;
                }
            });
        }
        api
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.server.store
    }

    /// Abort resumable uploads past their expiry. Returns how many.
    pub async fn sweep_resumable(&self) -> usize {
        tus::sweep(self).await
    }
}

/// The developer API as an axum router. Serve it with connect info (see [`serve`]): egress is
/// classified by the peer address.
pub fn router(api: Arc<Api>) -> Router {
    Router::new().fallback(handle).with_state(api)
}

/// Serve the developer API on `listener` until it fails.
pub async fn serve(listener: tokio::net::TcpListener, api: Arc<Api>) -> std::io::Result<()> {
    axum::serve(listener, router(api).into_make_service_with_connect_info::<SocketAddr>()).await
}

async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    req::dispatch(api, peer, req).await
}
