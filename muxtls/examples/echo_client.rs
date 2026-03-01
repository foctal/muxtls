use std::net::SocketAddr;

use bytes::Bytes;
use muxtls::{ClientConfig, Endpoint};

#[tokio::main]
async fn main() -> muxtls::Result<()> {
    let endpoint = Endpoint::client(ClientConfig::dangerous_insecure_no_verify_for_testing());
    let addr: SocketAddr = "127.0.0.1:4433".parse().expect("valid address");

    let connection = endpoint.connect(addr, "localhost")?.await?;
    let (send, recv) = connection.open_bi().await?;

    send.write_chunk(Bytes::from_static(b"hello from client"))
        .await?;
    send.finish().await?;

    while let Some(chunk) = recv.read_chunk().await? {
        println!("echo: {}", String::from_utf8_lossy(&chunk));
    }

    connection.close("client done").await?;
    // Wait for the connection to be fully closed before exiting.
    // Sleeping for a short time is a simple way to achieve this in this example.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    Ok(())
}
