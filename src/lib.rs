use std::{
    fmt, io,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures::{ready, TryStreamExt};
use quinn::{Accept, ClientConfig, ConnectError, Connecting, ServerConfig};
use rendevouz::{RendevouzService, SimpleQuicRendevouz};
use serde::{Deserialize, Serialize};
use tokio::{
    io::Interest,
    sync::{broadcast, mpsc, oneshot},
};
use tracing::{debug, info};

use crate::rendevouz::AnnounceReply;

pub mod rendevouz;

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

pub type UdpReceive = broadcast::Receiver<IncomingHolepunchPacket>;
pub type UdpSend = mpsc::Sender<OutgoingHolepunchPacket>;

#[derive(Debug, Clone)]
pub struct Endpoint {
    endpoint: quinn::Endpoint,
    peer_id: PeerId,
    rendevouz_tx: mpsc::Sender<RendevouzRequest>,
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
        let (socket, udp_recv, udp_send) = PunchingUdpSocket::bind(bind_addr).await?;
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            Default::default(),
            Some(server_config),
            socket,
            quinn::TokioRuntime,
        )?;
        endpoint.set_default_client_config(default_insecure_client_config());

        let peer_id_clone = peer_id.clone();
        let (rendevouz_tx, rendevouz_rx) = mpsc::channel(1024);
        let rendevouz = SimpleQuicRendevouz::new(endpoint.clone(), rendevouz_addr).await?;

        tokio::spawn(async move {
            let res = holepunch(rendevouz, peer_id_clone, rendevouz_rx, udp_recv, udp_send).await;
            if let Err(err) = res {
                debug!("Error in holepunch loop: {err}");
            }
        });

        Ok(Self {
            endpoint,
            rendevouz_tx,
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
        self.rendevouz_tx.send(req).await?;
        let addr = rx.await??;
        let connecting = self.endpoint.connect(addr, SERVER_NAME)?;
        Ok(connecting)
    }
}

#[derive(Clone, Debug)]
pub struct IncomingHolepunchPacket {
    data: [u8; 1],
    from: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct OutgoingHolepunchPacket {
    data: [u8; 1],
    dest: SocketAddr,
}

#[derive(Debug)]
struct PunchingUdpSocket {
    socket: Arc<tokio::net::UdpSocket>,
    quinn_socket_state: quinn_udp::UdpSocketState,
    udp_recv_tx: broadcast::Sender<IncomingHolepunchPacket>,
}

impl PunchingUdpSocket {
    async fn bind(addr: SocketAddr) -> io::Result<(Self, UdpReceive, UdpSend)> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        let socket = socket.into_std()?;

        quinn_udp::UdpSocketState::configure((&socket).into())?;

        let socket = Arc::new(tokio::net::UdpSocket::from_std(socket)?);

        let (udp_recv_tx, udp_recv) = broadcast::channel::<IncomingHolepunchPacket>(1024);
        let (udp_send, mut udp_send_rx) = mpsc::channel::<OutgoingHolepunchPacket>(1024);

        let socket_clone = socket.clone();
        tokio::spawn(async move {
            while let Some(packet) = udp_send_rx.recv().await {
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
                udp_recv_tx,
            },
            udp_recv,
            udp_send,
        ))
    }
}

impl quinn::AsyncUdpSocket for PunchingUdpSocket {
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
                    forward_holepunch(&self.udp_recv_tx, bufs, metas, msg_count);
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

fn forward_holepunch(
    channel: &broadcast::Sender<IncomingHolepunchPacket>,
    bufs: &[std::io::IoSliceMut<'_>],
    metas: &[quinn_udp::RecvMeta],
    msg_count: usize,
) {
    for (meta, buf) in metas.iter().zip(bufs.iter()).take(msg_count) {
        if meta.len == 1 {
            let packet = IncomingHolepunchPacket {
                data: [*&buf[0]],
                from: meta.addr,
            };
            let _ = channel.send(packet);
        }
    }
}

async fn holepunch(
    rendevouz: impl RendevouzService,
    peer_id: PeerId,
    mut rendevouz_req_rx: mpsc::Receiver<RendevouzRequest>,
    udp_recv: UdpReceive,
    udp_send: UdpSend,
) -> anyhow::Result<()> {
    // 1. Announce ourselves to the rendevouz server
    let announce_stream = rendevouz.announce(peer_id).await?;

    // 2. Send connection requests to the rendevouz server
    tokio::spawn(async move {
        while let Some(req) = rendevouz_req_rx.recv().await {
            let rendevouz = rendevouz.clone();
            tokio::spawn(async move {
                let res = rendevouz.resolve(req.peer_id);
                let res = res.await;
                match res {
                    Ok(Some(addr)) => req.reply.send(Ok(addr)),
                    Ok(None) => req.reply.send(Err(anyhow::anyhow!("Peer not found"))),
                    Err(err) => req.reply.send(Err(err)),
                }
            });
        }
    });

    // 3. Wait for incoming connection requests from the rendevouz server.
    info!("waiting for connections");
    tokio::pin!(announce_stream);
    while let Some(reply) = announce_stream.try_next().await? {
        match reply {
            AnnounceReply::GlobalAddress(addr) => {
                info!("New global address: {addr}");
            }
            AnnounceReply::Knock(peer) => {
                let addr = peer.addr;
                debug!("new holepunch request from {addr}");
                let udp_send = udp_send.clone();
                let udp_recv = udp_recv.resubscribe();
                tokio::spawn({
                    async move {
                        match holepunch_peer(udp_send, udp_recv, addr).await {
                            Ok(_) => debug!("holepunch ok with {addr}"),
                            Err(err) => debug!("holepunch failed with {addr}: {err}"),
                        }
                    }
                });
            }
        }
    }
    Ok(())
}

async fn holepunch_peer(
    udp_send: UdpSend,
    mut udp_recv: UdpReceive,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let mut packet = OutgoingHolepunchPacket {
        dest: addr,
        data: [0u8],
    };
    let mut wait = false;
    loop {
        if wait {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::select! {
          send = udp_send.send(packet.clone()) => {
              if let Err(err) = send {
                  debug!("Failed to forward holepunch packet to {addr}: {err}");
              }
              wait = true;
          }
          recv = udp_recv.recv() => {
              if let Ok(recv) = recv {
                  if recv.from == addr {
                      match recv.data[0] {
                          0 => {
                              packet.data = [1u8];
                          }
                          1 => break,
                          _ => debug!("Received invalid holepunch packet from {addr}")
                      }
                  }
              }
              wait = false
          }
        }
    }
    Ok(())
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
