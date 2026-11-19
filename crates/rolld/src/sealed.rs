//! A sealed start (`ROLL_SECRET_KEY_SEALED`, docs/secrets.md "Starting sealed"): before any
//! product starts, the API serves only roll secrets' unseal (`roll_secrets::unseal`; everything
//! else, health included, answers 503 `sealed`) until a threshold of the deployment key's shares
//! give the key back. It then reaches every product in memory through `Args::secret_key`, never
//! the environment or a file. Each node of a fleet unseals on its own, as Vault's do: an
//! unsealed node handing the key to the others would make whatever authenticates nodes to each
//! other (the cluster secret, in their environment) worth the key itself.

use crate::Args;
use axum::serve::ListenerExt as _;
use roll_secrets::unseal::{Sealed, Unsealer};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;
use zeroize::Zeroizing;

/// What may not be set beside `ROLL_SECRET_KEY_SEALED`: the key would then be in the environment
/// after all (finish a rotation before starting sealed).
const OTHER_SOURCES: [&str; 4] = ["ROLL_SECRET_KEY", "ROLL_SECRET_KEY_PREVIOUS", "ROLL_SECRET_KEY_KMS", "ROLL_SECRET_KEY_KMS_PREVIOUS"];

/// The API's listener, shared with the sealed server so it outlives it.
struct Shared(Arc<TcpListener>);

impl axum::serve::Listener for Shared {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok(c) => return c,
                // Out of descriptors and the like: as axum's own listener, wait and go on.
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Serve the unseal on the API's address until the shares give the key: the key, and the same
/// listener (connections wait in its backlog while the products start). `None`: stopped while
/// sealed.
pub async fn unseal(args: &Args, home: &Path, sealed: Sealed) -> anyhow::Result<Option<(TcpListener, Zeroizing<String>)>> {
    for var in OTHER_SOURCES {
        anyhow::ensure!(std::env::var(var).ok().is_none_or(|v| v.trim().is_empty()), "ROLL_SECRET_KEY_SEALED and {var} can't both be set: a sealed node's key comes only from its shares (finish a key rotation before starting sealed)");
    }
    let (target, pg) = crate::secrets::store(args, home)?;
    let (unsealer, key) = tokio::task::block_in_place(|| Unsealer::new(sealed, roll_secrets::open_db(&target, pg)?))?;
    let listener = Arc::new(TcpListener::bind(args.api).await?);
    tracing::warn!(addr = %args.api, set = %Sealed::set_hex(&sealed.set), need = sealed.threshold, "sealed: nothing runs until the deployment key's shares unseal this node (roll secrets unseal)");
    let router = roll_secrets::unseal::sealed_router(unsealer).layer(axum::middleware::from_fn(crate::logs::access));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(Shared(listener.clone()).tap_io(|_| ()), router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            let _ = stopped.await;
        })
        .into_future();
    tokio::pin!(serve);
    let key = tokio::select! {
        k = key => Some(k?),
        r = &mut serve => {
            r?;
            anyhow::bail!("the sealed API stopped");
        }
        () = crate::shutdown_signal() => None,
    };
    // The last share's answer goes out before the products start.
    let _ = stop.send(());
    serve.await?;
    let Some(key) = key else { return Ok(None) };
    tracing::info!("unsealed: starting the products");
    let listener = match Arc::try_unwrap(listener) {
        Ok(l) => l,
        Err(_) => TcpListener::bind(args.api).await?,
    };
    Ok(Some((listener, key)))
}
