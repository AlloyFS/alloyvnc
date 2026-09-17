//! The listener: one task per accepted connection.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use crate::session::{self, SessionConfig};
use crate::shared::Shared;

pub struct Server {
    listener: TcpListener,
    shared: Arc<Shared>,
    session: Arc<SessionConfig>,
}

impl Server {
    pub async fn bind(addr: SocketAddr, session: SessionConfig, shared: Arc<Shared>) -> Result<Server> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        Ok(Server {
            listener,
            shared,
            session: Arc::new(session),
        })
    }

    /// The address actually bound, which matters when port 0 was asked for.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    pub async fn run(self) -> Result<()> {
        loop {
            let (stream, peer) = self.listener.accept().await.context("accept")?;
            tracing::info!(%peer, "connection");
            let shared = self.shared.clone();
            let cfg = self.session.clone();
            tokio::spawn(async move {
                match session::run(stream, peer, shared, cfg).await {
                    Ok(()) => tracing::info!(%peer, "session ended"),
                    Err(e) => tracing::warn!(%peer, error = %e, "session ended"),
                }
            });
        }
    }
}
