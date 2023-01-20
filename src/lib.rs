use std::{
    collections::HashMap,
    fmt, io,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use bytes::BytesMut;
use futures::ready;
use quinn::{Accept, ConnectError, Connecting, Connection, RecvStream, SendStream, ServerConfig, ClientConfig};
use serde::{Deserialize, Serialize};
use tokio::{
    io::Interest,
    sync::{broadcast, mpsc, oneshot, RwLock},
};
use tracing::{debug, error, info};

const SERVER_NAME: &str = "localhost";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerId(String);
impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl From<&str> for PeerId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}
impl From<String> for PeerId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug)]
pub struct RendevouzRequest {
    peer_id: PeerId,
    reply: oneshot::Sender<anyhow::Result<SocketAddr>>,
}

type HolepunchReceive = broadcast::Receiver<IncomingHolepunchPacket>;
type HolepunchSend = mpsc::Sender<OutgoingHolepunchPacket>;

#[derive(Debug, Clone)]
pub struct Endpoint {
    endpoint: quinn::Endpoint,
    peer_id: PeerId,
    rendevouz_req_tx: mpsc::Sender<RendevouzRequest>,
}

impl Endpoint {
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        let addr = self.endpoint.local_addr()?;
        Ok(addr)
    }
    pub fn local_peer_id(&self) -> &PeerId {
        &self.peer_id
    }
    pub async fn bind(
        bind_addr: SocketAddr,
        rendevouz_addr: SocketAddr,
        peer_id: PeerId,
    ) -> anyhow::Result<Self> {
        let (server_config, _cert) = default_server_config()?;
        let (socket, holepunch_rx, holepunch_tx) = CustomUdpSocket::bind(bind_addr).await?;
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            Default::default(),
            Some(server_config),
            socket,
            quinn::TokioRuntime,
        )?;
        endpoint.set_default_client_config(default_insecure_client_config());

        let (rendevouz_req_tx, rendevouz_req_rx) = mpsc::channel(1024);
        let rendevouz_connecting = endpoint.connect(rendevouz_addr, SERVER_NAME)?;
        let peer_id_clone = peer_id.clone();
        tokio::spawn(async move {
            if let Err(err) = holepunch(
                peer_id_clone,
                rendevouz_connecting,
                rendevouz_req_rx,
                holepunch_rx,
                holepunch_tx,
            )
            .await
            {
                debug!("Error in holepunch loop: {err}");
            }
        });

        Ok(Self {
            endpoint,
            rendevouz_req_tx,
            peer_id,
        })
    }

    pub fn accept(&self) -> Accept<'_> {
        self.endpoint.accept()
    }

    pub fn connect_direct(&self, addr: SocketAddr) -> Result<Connecting, ConnectError> {
        self.endpoint.connect(addr, SERVER_NAME)
    }

    pub async fn connect_rendevouz(&self, peer_id: PeerId) -> anyhow::Result<Connecting> {
        let (tx, rx) = oneshot::channel();
        let req = RendevouzRequest { peer_id, reply: tx };
        self.rendevouz_req_tx.send(req).await?;
        let addr = rx.await??;
        let connecting = self.endpoint.connect(addr, SERVER_NAME)?;
        Ok(connecting)
    }
}

fn default_server_config() -> anyhow::Result<(ServerConfig, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
    let cert_der = cert.serialize_der()?;
    let priv_key = cert.serialize_private_key_der();
    let priv_key = rustls::PrivateKey(priv_key);
    let cert_chain = vec![rustls::Certificate(cert_der.clone())];

    let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
    Arc::get_mut(&mut server_config.transport)
        .unwrap()
        .max_concurrent_uni_streams(0_u8.into());

    Ok((server_config, cert_der))
}

#[derive(Clone, Debug)]
struct IncomingHolepunchPacket {
    data: [u8; 1],
    from: SocketAddr,
}

#[derive(Clone, Debug)]
struct OutgoingHolepunchPacket {
    data: [u8; 1],
    dest: SocketAddr,
}

#[derive(Debug)]
struct CustomUdpSocket {
    socket: Arc<tokio::net::UdpSocket>,
    quinn_socket_state: quinn_udp::UdpSocketState,
    sc_recv_tx: broadcast::Sender<IncomingHolepunchPacket>,
}

impl CustomUdpSocket {
    async fn bind(addr: SocketAddr) -> io::Result<(Self, HolepunchReceive, HolepunchSend)> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        let socket = socket.into_std()?;

        quinn_udp::UdpSocketState::configure((&socket).into())?;

        let socket = Arc::new(tokio::net::UdpSocket::from_std(socket)?);

        let (sc_recv_tx, sc_recv_rx) = broadcast::channel::<IncomingHolepunchPacket>(1024);
        let (sc_send_tx, mut sc_send_rx) = mpsc::channel::<OutgoingHolepunchPacket>(1024);

        let socket_clone = socket.clone();
        tokio::spawn(async move {
            while let Some(packet) = sc_send_rx.recv().await {
                match socket_clone.send_to(&packet.data, packet.dest).await {
                    Ok(_) => {}
                    Err(err) => debug!(
                        "Failed to send holepunch packet to {}: {}",
                        packet.dest, err
                    ),
                }
            }
        });

        Ok((
            Self {
                socket,
                quinn_socket_state: quinn_udp::UdpSocketState::new(),
                sc_recv_tx,
            },
            sc_recv_rx,
            sc_send_tx,
        ))
    }
}

impl quinn::AsyncUdpSocket for CustomUdpSocket {
    fn poll_send(
        &mut self,
        state: &quinn_udp::UdpState,
        cx: &mut Context,
        transmits: &[quinn::Transmit],
    ) -> Poll<io::Result<usize>> {
        let quinn_socket_state = &mut self.quinn_socket_state;
        let io = &*self.socket;
        loop {
            ready!(io.poll_send_ready(cx))?;
            if let Ok(res) = io.try_io(Interest::WRITABLE, || {
                quinn_socket_state.send(io.into(), state, transmits)
            }) {
                return Poll::Ready(Ok(res));
            }
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        metas: &mut [quinn_udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.socket.poll_recv_ready(cx))?;
            if let Ok(res) = self.socket.try_io(Interest::READABLE, || {
                let res = self
                    .quinn_socket_state
                    .recv((&*self.socket).into(), bufs, metas);

                if let Ok(msg_count) = res {
                    send_holepunch_to_side_channels(&self.sc_recv_tx, bufs, metas, msg_count);
                }

                res
            }) {
                return Poll::Ready(Ok(res));
            }
        }
    }

    fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
    }
}

fn send_holepunch_to_side_channels(
    channel: &broadcast::Sender<IncomingHolepunchPacket>,
    bufs: &[std::io::IoSliceMut<'_>],
    metas: &[quinn_udp::RecvMeta],
    msg_count: usize,
) {
    for (meta, buf) in metas.iter().zip(bufs.iter()).take(msg_count) {
        let data: BytesMut = buf[0..meta.len].into();
        if data.len() == 1 {
            let packet = IncomingHolepunchPacket {
                data: [data[0]],
                from: meta.addr,
            };
            let _ = channel.send(packet);
        }
    }
}

async fn resolve_connection_request(
    conn: Connection,
    peer_id: PeerId,
) -> anyhow::Result<Option<SocketAddr>> {
    let (send, recv) = conn.open_bi().await?;
    let message = Message::ConnectRequest(peer_id);
    message.send(send).await?;
    let reply = Message::recv(recv).await?;
    let addr = reply.as_connect_reply()?;
    Ok(addr)
}

async fn holepunch(
    peer_id: PeerId,
    rendevouz_connecting: Connecting,
    mut rendevouz_req_rx: mpsc::Receiver<RendevouzRequest>,
    holepunch_rx: HolepunchReceive,
    holepunch_tx: HolepunchSend,
) -> anyhow::Result<()> {
    let rendevouz_conn = rendevouz_connecting.await?;

    // 1. Announce ourselves to the rendevouz server
    let (send, recv) = rendevouz_conn.open_bi().await?;
    let msg = Message::Listen(peer_id.clone());
    msg.send(send).await?;
    let reply = Message::recv(recv).await?;
    let global_addr = reply.as_accepted()?;
    info!("determined our global addr: {global_addr}");

    // 2. Send connection requests to the rendevouz server
    let conn = rendevouz_conn.clone();
    tokio::spawn(async move {
        while let Some(req) = rendevouz_req_rx.recv().await {
            let conn = conn.clone();
            tokio::spawn(async move {
                match resolve_connection_request(conn, req.peer_id).await {
                    Ok(Some(addr)) => req.reply.send(Ok(addr)),
                    Ok(None) => req.reply.send(Err(anyhow::anyhow!("Peer not found"))),
                    Err(err) => req.reply.send(Err(err)),
                }
            });
        }
    });

    // 3. Wait for incoming connection requests from the rendevouz server.
    info!("waiting for connections");
    loop {
        let stream = rendevouz_conn.accept_uni().await?;
        let msg = Message::recv(stream).await?;
        let addr = msg.as_incoming()?;
        debug!("new connection request from rendevouz server: from {addr}");
        tokio::spawn({
            let holepunch_tx = holepunch_tx.clone();
            let holepunch_rx = holepunch_rx.resubscribe();
            debug!("starting to holepunch {addr}");
            async move {
                if let Err(err) = holepunch_peer(holepunch_tx, holepunch_rx, addr).await {
                    debug!("Error while holepunching {addr}: {err}");
                } else {
                    info!("holepunch successfull to {addr}")
                }
            }
        });
    }
}

async fn holepunch_peer(
    holepunch_tx: HolepunchSend,
    mut holepunch_rx: HolepunchReceive,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let mut packet = OutgoingHolepunchPacket {
        dest: addr,
        data: [0u8],
    };
    let mut wait = true;
    loop {
        if wait {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::select! {
          send = holepunch_tx.send(packet.clone()) => {
              if let Err(err) = send {
                  debug!("Failed to forward holepunch packet to {addr}: {err}");
              }
              wait = true;
          }
          recv = holepunch_rx.recv() => {
              if let Ok(recv) = recv {
                  if recv.from == addr {
                      match recv.data[0] {
                          0 => {
                              packet.data = [1u8];
                          }
                          1 => break,
                          _ => debug!("Received invalid holepunch packet from {addr}")
                      }
                  } else {
                      wait = false;
                  }
              }
          }
        }
    }
    Ok(())
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
    let (server_config, _cert) = default_server_config()?;
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
    while let Ok((send, recv)) = conn.accept_bi().await {
        let message = Message::recv(recv).await?;
        match message {
            Message::Listen(peer_id) => {
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
    Ok(())
}

pub fn default_insecure_client_config() -> ClientConfig {
    let crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let client_cfg = ClientConfig::new(Arc::new(crypto));
    client_cfg
}

struct SkipServerVerification;
impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}
