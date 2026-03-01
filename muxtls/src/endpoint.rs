use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, info, instrument};

use crate::config::{ClientConfig, ServerConfig};
use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::limits::Limits;

/// Future returned by [`Endpoint::connect`].
pub struct Connecting {
    inner: Pin<Box<dyn Future<Output = Result<Connection>> + Send + 'static>>,
}

impl Future for Connecting {
    type Output = Result<Connection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

/// Network endpoint used to initiate or accept muxtls connections.
pub struct Endpoint {
    inner: EndpointInner,
    limits: Limits,
}

enum EndpointInner {
    Client {
        config: ClientConfig,
    },
    Server {
        listener: TcpListener,
        config: ServerConfig,
    },
}

impl Endpoint {
    /// Creates a client endpoint.
    pub fn client(config: ClientConfig) -> Self {
        Self {
            inner: EndpointInner::Client { config },
            limits: Limits::default(),
        }
    }

    /// Binds and creates a server endpoint.
    pub async fn server(addr: impl ToSocketAddrs, config: ServerConfig) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            inner: EndpointInner::Server { listener, config },
            limits: Limits::default(),
        })
    }

    /// Overrides default limits for newly created connections.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns local address if this endpoint is a server endpoint.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        match &self.inner {
            EndpointInner::Server { listener, .. } => Ok(listener.local_addr()?),
            EndpointInner::Client { .. } => Err(Error::EndpointRole(
                "local_addr is only available on server endpoints",
            )),
        }
    }

    /// Starts connecting to a remote server.
    #[instrument(skip(self), level = "info")]
    pub fn connect(&self, addr: SocketAddr, server_name: &str) -> Result<Connecting> {
        let EndpointInner::Client { config } = &self.inner else {
            return Err(Error::EndpointRole(
                "connect is only available on client endpoints",
            ));
        };

        let cfg = config.clone();
        let limits = self.limits.clone();
        let server_name = server_name.to_owned();
        let fut = async move {
            let tcp = TcpStream::connect(addr).await?;
            tcp.set_nodelay(true)?;

            let connector = TlsConnector::from(cfg.inner.clone());
            let tls = connector
                .connect(ClientConfig::server_name(&server_name)?, tcp)
                .await
                .map_err(|e| Error::Config(e.to_string()))?;

            info!(remote = %addr, "client connection established");
            Ok(Connection::new(tls, limits, true))
        };

        Ok(Connecting {
            inner: Box::pin(fut),
        })
    }

    /// Accepts one incoming connection from a server endpoint.
    #[instrument(skip(self), level = "info")]
    pub async fn accept(&self) -> Result<Connection> {
        let EndpointInner::Server { listener, config } = &self.inner else {
            return Err(Error::EndpointRole(
                "accept is only available on server endpoints",
            ));
        };

        let (tcp, peer) = listener.accept().await?;
        tcp.set_nodelay(true)?;

        let acceptor = TlsAcceptor::from(config.inner.clone());
        let tls = acceptor
            .accept(tcp)
            .await
            .map_err(|e| Error::Config(e.to_string()))?;

        debug!(remote = %peer, "server accepted connection");
        Ok(Connection::new(tls, self.limits.clone(), false))
    }
}
