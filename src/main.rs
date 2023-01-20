use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use quinn::{Connecting, RecvStream, SendStream};
use quinn_holepunch::Endpoint;
use tracing::{error, info};

#[derive(Parser, Debug, Clone)]
#[clap(version, about, long_about = None)]
#[clap(about = "Send data.")]
struct Cli {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    Rendevouz {
        /// Listen address to bind to (default: 0.0.0.0:3033).
        addr: Option<SocketAddr>,
    },

    ConnectDirect {
        addr: SocketAddr,
    },

    ConnectRendevouz {
        /// Address of rendevouz server.
        #[clap(short, long)]
        rendevouz_addr: SocketAddr,

        /// Peer ID to connect to
        #[clap(short, long)]
        peer_id: String,
    },

    Listen {
        /// Listen address to bind to (default: 0.0.0.0:22022).
        #[clap(short, long)]
        addr: Option<SocketAddr>,

        /// Rendevouz addr to announce to.
        #[clap(short, long)]
        rendevouz_addr: SocketAddr,

        /// Peer ID (default: random string)
        #[clap(short, long)]
        peer_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Rendevouz { addr } => {
            let addr = addr.unwrap_or_else(|| "0.0.0.0:3033".parse().unwrap());
            quinn_holepunch::rendevouz_server(addr).await?;
        }
        Command::Listen {
            addr,
            peer_id,
            rendevouz_addr,
        } => {
            let addr = addr.unwrap_or_else(|| "0.0.0.0:22022".parse().unwrap());
            let peer_id = peer_id.unwrap_or_else(|| random_string());
            let endpoint = Endpoint::bind(addr, rendevouz_addr, peer_id.into()).await?;
            let local_addr = endpoint.local_addr()?;
            info!("Listening on {local_addr}");
            while let Some(conn) = endpoint.accept().await {
                tokio::spawn(async move {
                    let addr = conn.remote_address();
                    info!("New connection from {addr}");
                    if let Err(err) = handle_conn(conn, "server".into()).await {
                        error!("Error when handling conn from {}: {}", addr, err);
                    }
                });
            }
        },
        Command::ConnectRendevouz { rendevouz_addr, peer_id } => {
            info!("Connect to {peer_id} via {rendevouz_addr}");
            let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let local_id = random_string();
            let endpoint = Endpoint::bind(bind_addr, rendevouz_addr, local_id.into()).await?;
            let local_addr = endpoint.local_addr()?;
            info!("Listening on {local_addr}");
            let conn = endpoint.connect_rendevouz(peer_id.into()).await?;
            handle_conn(conn, "client".into()).await?;
        }
        Command::ConnectDirect { addr } => {
            info!("Connect to {addr}");
            unimplemented!()

        }
    }
    Ok(())
}

async fn handle_conn(conn: Connecting, name: String) -> anyhow::Result<()> {
    let conn = conn.await?;
    let remote_addr = conn.remote_address();

    let conn2 = conn.clone();
    let name2 = name.clone();
    tokio::spawn(async move {
        while let Ok((send, recv)) = conn2.accept_bi().await {
            let message = format!(
                "Hello! {} speaking on accept_bi from {:?} to {}",
                name2,
                conn2.local_ip(),
                remote_addr
            );
            tokio::spawn(async move {
                match handle_stream(send, recv, message).await {
                    Ok(()) => {}
                    Err(err) => error!("Error handling stream from {remote_addr}: {err}"),
                }
            });
        }
    });

    let (send, recv) = conn.open_bi().await?;
    let message = format!(
        "Hello! {} speaking on open_bi from {:?} to {}",
        name,
        conn.local_ip(),
        conn.remote_address()
    );
    handle_stream(send, recv, message).await?;

    Ok(())
}

async fn handle_stream(
    mut send: SendStream,
    recv: RecvStream,
    message: String,
) -> anyhow::Result<()> {
    info!("Send: {}", message);
    send.write_all(&message.as_bytes()).await?;
    send.finish().await?;
    let reply = recv.read_to_end(1024).await?;
    info!("Recv: {}", String::from_utf8(reply)?);
    Ok(())
}

pub fn random_string() -> String {
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    let rand_string: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect();
    rand_string
}
