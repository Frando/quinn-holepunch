use std::{
    collections::{HashMap},
    net::SocketAddr,
    sync::Arc,
};

use async_stream::try_stream;
use async_trait::async_trait;
use futures::{stream::BoxStream, Stream, StreamExt};
use quinn::{Connecting, Connection, Endpoint, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::{PeerId, SERVER_NAME};

#[derive(Debug)]
pub enum AnnounceReply {
    GlobalAddress(SocketAddr),
    Knock(Knock),
}

#[derive(Debug)]
pub struct Knock {
    pub addr: SocketAddr,
    pub peer_id: Option<PeerId>,
}

#[async_trait]
pub trait RendevouzService: Clone + Unpin + Send + 'static {
    type AnnounceStream: Stream<Item = anyhow::Result<AnnounceReply>> + Send + 'static;
    async fn resolve(&self, peer_id: PeerId) -> anyhow::Result<Option<SocketAddr>>;
    async fn announce(&self, local_id: PeerId) -> anyhow::Result<Self::AnnounceStream>;
}

#[derive(Clone)]
pub struct SimpleQuicRendevouz {
    conn: Connection,
}

impl SimpleQuicRendevouz {
    pub async fn new(endpoint: Endpoint, rendevouz_addr: SocketAddr) -> anyhow::Result<Self> {
        let conn = endpoint.connect(rendevouz_addr, SERVER_NAME)?.await?;
        Ok(Self { conn })
    }
}

#[async_trait]
impl RendevouzService for SimpleQuicRendevouz {
    type AnnounceStream = BoxStream<'static, anyhow::Result<AnnounceReply>>;

    async fn resolve(&self, peer_id: PeerId) -> anyhow::Result<Option<SocketAddr>> {
        let conn = self.conn.clone();
        let (send, recv) = conn.open_bi().await?;
        let message = Message::ConnectRequest(peer_id);
        message.send(send).await?;
        let reply = Message::recv(recv).await?;
        let addr = reply.as_connect_reply()?;
        Ok(addr)
    }

    async fn announce(&self, local_id: PeerId) -> anyhow::Result<Self::AnnounceStream> {
        let (send, recv) = self.conn.open_bi().await?;
        let conn = self.conn.clone();
        let stream = try_stream! {
            let msg = Message::Listen(local_id);
            msg.send(send).await?;
            let reply = Message::recv(recv).await?;
            let global_addr = reply.as_accepted()?;
            debug!("Found our global adddress: {global_addr}");
            yield AnnounceReply::GlobalAddress(global_addr);
            loop {
                let stream = conn.accept_uni().await?;
                let msg = Message::recv(stream).await?;
                let addr = msg.as_incoming()?;
                debug!("new connection request from rendevouz server: from {addr}");
                yield AnnounceReply::Knock(Knock {
                    addr,
                    peer_id: None
                });
            }
        };
        Ok(stream.boxed())
    }
}

#[derive(Serialize, Deserialize)]
enum Message {
    Listen(PeerId),
    Accepted(SocketAddr),
    Incoming(SocketAddr),
    ConnectRequest(PeerId),
    ConnectReply(Option<SocketAddr>),
}

impl Message {
    async fn recv(stream: RecvStream) -> anyhow::Result<Self> {
        let msg = stream.read_to_end(1024).await?;
        let msg: Message = postcard::from_bytes(&msg)?;
        Ok(msg)
    }

    async fn send(&self, mut stream: SendStream) -> anyhow::Result<()> {
        let msg = postcard::to_stdvec(&self)?;
        stream.write_all(&msg).await?;
        stream.finish().await?;
        Ok(())
    }

    fn as_connect_reply(self) -> anyhow::Result<Option<SocketAddr>> {
        match self {
            Self::ConnectReply(addr) => Ok(addr),
            _ => anyhow::bail!("Invalid message"),
        }
    }

    fn as_accepted(self) -> anyhow::Result<SocketAddr> {
        match self {
            Self::Accepted(addr) => Ok(addr),
            _ => anyhow::bail!("Invalid message"),
        }
    }

    fn as_incoming(self) -> anyhow::Result<SocketAddr> {
        match self {
            Self::Incoming(addr) => Ok(addr),
            _ => anyhow::bail!("Invalid message"),
        }
    }
}

type RendevouzState = Arc<RwLock<HashMap<PeerId, Connection>>>;
pub async fn rendevouz_server(bind_addr: SocketAddr) -> anyhow::Result<()> {
    let (server_config, _cert) = crate::default_server_config()?;
    let endpoint = quinn::Endpoint::server(server_config, bind_addr)?;
    info!("Listening on {}", endpoint.local_addr()?);

    let state = Arc::new(RwLock::new(HashMap::new()));

    while let Some(conn) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = on_connection(conn, state).await {
                error!("Error in connection handling: {err}")
            }
        });
    }

    Ok(())
}

async fn on_connection(conn: Connecting, state: RendevouzState) -> anyhow::Result<()> {
    let conn = conn.await?;
    // let mut peer_ids: HashSet<PeerId> = HashSet::new();
    while let Ok((send, recv)) = conn.accept_bi().await {
        let message = Message::recv(recv).await?;
        match message {
            Message::Listen(peer_id) => {
                // peer_ids.insert(peer_id.clone());
                let addr = conn.remote_address();
                debug!("Listen: {peer_id} -> {addr}");
                let mut state = state.write().await;
                state.insert(peer_id, conn.clone());
                let reply = Message::Accepted(addr);
                reply.send(send).await?;
            }
            Message::ConnectRequest(peer_id) => {
                let state = state.read().await;
                let connect_addr = conn.remote_address();
                let reply = match state.get(&peer_id) {
                    Some(peer) => {
                        let addr = peer.remote_address();
                        let peer = peer.clone();
                        info!("Connect: {connect_addr} -> {peer_id} -> {addr}");
                        tokio::spawn(async move {
                            if let Ok(send) = peer.open_uni().await {
                                let message = Message::Incoming(connect_addr);
                                if let Err(err) = message.send(send).await {
                                    debug!(
                                        "Failed to send incoming message to {}: {err}",
                                        peer.remote_address()
                                    );
                                }
                            }
                        });
                        Some(addr)
                    }
                    None => {
                        info!("Connect: {connect_addr} -> {peer_id} -> NOT FOUND");
                        None
                    }
                };
                let reply = Message::ConnectReply(reply);
                reply.send(send).await?;
            }
            Message::Accepted(_) => {
                anyhow::bail!("Received invalid message: Accepted")
            }
            Message::Incoming(_) => {
                anyhow::bail!("Received invalid message: Accepted")
            }
            Message::ConnectReply(_) => {
                anyhow::bail!("Received invalid message: ConnectReply")
            }
        }
    }

    // TODO: Expire peer ids.
    // let mut state = state.write().await;
    // for peer_id in peer_ids.iter() {
    //     state.remove(peer_id);
    // }

    Ok(())
}
