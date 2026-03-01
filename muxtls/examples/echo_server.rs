use muxtls::{Endpoint, ServerConfig};
use tracing::error;

#[tokio::main]
async fn main() -> muxtls::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("muxtls=info".parse().expect("valid log filter")),
        )
        .init();

    let (server_config, _cert) = ServerConfig::self_signed_for_localhost()?;
    let endpoint = Endpoint::server("127.0.0.1:4433", server_config).await?;
    println!("echo server listening on {}", endpoint.local_addr()?);

    loop {
        let connection = endpoint.accept().await?;
        tokio::spawn(async move {
            loop {
                let (send, recv) = match connection.accept_bi().await {
                    Ok(stream) => stream,
                    Err(err) => {
                        error!(%err, "accept_bi failed");
                        break;
                    }
                };

                tokio::spawn(async move {
                    if let Err(err) = run_echo(send, recv).await {
                        error!(%err, "echo stream failed");
                    }
                });
            }
        });
    }
}

async fn run_echo(send: muxtls::SendStream, recv: muxtls::RecvStream) -> muxtls::Result<()> {
    while let Some(chunk) = recv.read_chunk().await? {
        send.write_chunk(chunk).await?;
    }
    send.finish().await?;
    Ok(())
}
