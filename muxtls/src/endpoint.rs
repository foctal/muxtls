use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, info, instrument};

use crate::config::{ALPN_PROTOCOL, ClientConfig, ServerConfig};
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
    handshake_timeout: Duration,
    keepalive_interval: Option<Duration>,
    idle_timeout: Option<Duration>,
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
    const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Creates a client endpoint.
    pub fn client(config: ClientConfig) -> Self {
        Self {
            inner: EndpointInner::Client { config },
            limits: Limits::default(),
            handshake_timeout: Self::DEFAULT_HANDSHAKE_TIMEOUT,
            keepalive_interval: None,
            idle_timeout: None,
        }
    }

    /// Binds and creates a server endpoint.
    pub async fn server(addr: impl ToSocketAddrs, config: ServerConfig) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            inner: EndpointInner::Server { listener, config },
            limits: Limits::default(),
            handshake_timeout: Self::DEFAULT_HANDSHAKE_TIMEOUT,
            keepalive_interval: None,
            idle_timeout: None,
        })
    }

    /// Overrides default limits for newly created connections.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the timeout for TCP/TLS connection establishment.
    ///
    /// For clients this bounds both TCP connect and the TLS handshake. For
    /// servers it bounds the TLS handshake after TCP accept.
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Sends a `PING` frame at the configured interval on new connections.
    ///
    /// Keepalive is disabled by default. Any received frame, including `PING`,
    /// counts as activity for [`Endpoint::with_idle_timeout`].
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.keepalive_interval = Some(interval);
        self
    }

    /// Closes new connections when no frame is received for this duration.
    ///
    /// The idle timeout is disabled by default. When keepalive is enabled on
    /// both peers, this duration should be comfortably longer than the
    /// keepalive interval.
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
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
        limits.validate()?;
        self.validate_connection_policy()?;
        let handshake_timeout = self.handshake_timeout;
        let keepalive_interval = self.keepalive_interval;
        let idle_timeout = self.idle_timeout;
        let server_name = server_name.to_owned();
        let fut = async move {
            let tls = tokio::time::timeout(handshake_timeout, async {
                let tcp = TcpStream::connect(addr).await?;
                tcp.set_nodelay(true)?;

                let connector = TlsConnector::from(cfg.inner.clone());
                connector
                    .connect(ClientConfig::server_name(&server_name)?, tcp)
                    .await
                    .map_err(|e| Error::TlsHandshake(e.to_string()))
            })
            .await
            .map_err(|_| Error::Timeout("connection establishment"))??;
            if tls.get_ref().1.alpn_protocol() != Some(ALPN_PROTOCOL) {
                return Err(Error::TlsHandshake(
                    "peer did not negotiate the required muxtls/1 ALPN".to_owned(),
                ));
            }

            info!(remote = %addr, "client connection established");
            Connection::new(tls, limits, true, keepalive_interval, idle_timeout)
        };

        Ok(Connecting {
            inner: Box::pin(fut),
        })
    }

    /// Accepts and handshakes one incoming connection.
    ///
    /// A production accept loop should run multiple calls concurrently so one
    /// slow TLS peer cannot delay acceptance of unrelated connections.
    #[instrument(skip(self), level = "info")]
    pub async fn accept(&self) -> Result<Connection> {
        let EndpointInner::Server { listener, config } = &self.inner else {
            return Err(Error::EndpointRole(
                "accept is only available on server endpoints",
            ));
        };

        self.limits.validate()?;
        self.validate_connection_policy()?;
        let (tcp, peer) = listener.accept().await?;
        tcp.set_nodelay(true)?;

        let acceptor = TlsAcceptor::from(config.inner.clone());
        let tls = tokio::time::timeout(self.handshake_timeout, acceptor.accept(tcp))
            .await
            .map_err(|_| Error::Timeout("TLS handshake"))?
            .map_err(|e| Error::TlsHandshake(e.to_string()))?;
        if tls.get_ref().1.alpn_protocol() != Some(ALPN_PROTOCOL) {
            return Err(Error::TlsHandshake(
                "peer did not negotiate the required muxtls/1 ALPN".to_owned(),
            ));
        }

        debug!(remote = %peer, "server accepted connection");
        Connection::new(
            tls,
            self.limits.clone(),
            false,
            self.keepalive_interval,
            self.idle_timeout,
        )
    }

    fn validate_connection_policy(&self) -> Result<()> {
        if self.keepalive_interval == Some(Duration::ZERO) {
            return Err(Error::Config(
                "keepalive interval must be greater than zero".to_owned(),
            ));
        }
        if self.idle_timeout == Some(Duration::ZERO) {
            return Err(Error::Config(
                "idle timeout must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}
