//! `muxtls` provides multiplexed bidirectional streams over TLS/TCP.
//!
//! The crate offers a small async transport API:
//!
//! - [`Endpoint`] for client and server setup
//! - [`Connecting`] future for client handshakes
//! - [`Connection`] for opening and accepting streams
//! - [`SendStream`] and [`RecvStream`] for per-stream I/O
//!
//! Wire protocol primitives live in the `muxtls-proto`.
//!
//! # Design scope
//!
//! `muxtls` intentionally focuses on TLS/TCP transport semantics.
//!
//! # Example
//!
//! ```rust,no_run
//! use muxtls::{ClientConfig, Endpoint};
//! use std::net::SocketAddr;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> muxtls::Result<()> {
//! let endpoint = Endpoint::client(ClientConfig::with_native_roots()?);
//! let addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 4433));
//! let conn = endpoint.connect(addr, "localhost")?.await?;
//! let (send, recv) = conn.open_bi().await?;
//! let _ = (send, recv);
//! # Ok(())
//! # }
//! ```

mod config;
mod connection;
mod endpoint;
mod error;
mod limits;
mod stream;

pub use config::{ClientConfig, ServerConfig};
pub use connection::{Connection, ConnectionStats};
pub use endpoint::{Connecting, Endpoint};
pub use error::{Error, Result};
pub use limits::Limits;
pub use stream::{RecvStream, SendStream};
