use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, ActorResponse, Addr, AsyncContext, Context, Handler, Message, WrapFuture};
use actix_rt::System;
use clap::Parser;
use futures::channel::mpsc;
use futures::future::Either;
use futures::{Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, RwLock};
use tokio_tun::{Tun, TunBuilder};

use ya_net_gateway::net::{ConnectionChannels, Error, EssentialChannels, Result, VirtualNetwork};
use ya_relay_stack::{Protocol, SocketDesc};

const READER_BUFFER_SIZE: usize = 2048;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Socket to listen on
    #[clap(short, long, value_parser, default_value = "0.0.0.0:12345")]
    listen: SocketAddr,
    /// TAP interface path
    #[clap(long, value_parser, default_value = "proxy")]
    tap: String,
    /// TAP address
    #[clap(long, value_parser, default_value = "10.0.0.1")]
    tap_addr: Ipv4Addr,
    /// TAP service address to connect to
    #[clap(long, value_parser, default_value = "10.0.0.2:12345")]
    tap_service: SocketAddr,
}

struct Network {
    vnet: VirtualNetwork,
    ready_tx: Option<oneshot::Sender<Addr<Self>>>,
}

impl Network {
    pub fn spawn(ready_tx: oneshot::Sender<Addr<Self>>) -> (Self, EssentialChannels) {
        let (vnet, essentials) = VirtualNetwork::spawn();
        let ready_tx = Some(ready_tx);
        let this = Self { vnet, ready_tx };
        (this, essentials)
    }
}

impl Actor for Network {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        if let Some(tx) = self.ready_tx.take() {
            if tx.send(ctx.address()).is_err() {
                panic!("Unable to initialize network");
            }
        }
    }
}

impl Handler<Register> for Network {
    type Result = ActorResponse<Self, (Result<ConnectionChannels>, bool)>;

    fn handle(&mut self, msg: Register, _ctx: &mut Self::Context) -> Self::Result {
        let fut = match self.vnet.resolve_channel(msg.desc) {
            Either::Left(fut) => async move { (fut.await, true) }.into_actor(self),
            Either::Right(result) => return ActorResponse::reply((result, false)),
        };
        ActorResponse::r#async(fut)
    }
}

impl Handler<Unregister> for Network {
    type Result = <Unregister as Message>::Result;

    fn handle(&mut self, msg: Unregister, _ctx: &mut Self::Context) -> Self::Result {
        self.vnet.close_channel(msg.desc)
    }
}

impl Handler<Connect> for Network {
    type Result = ActorResponse<Self, Result<()>>;

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let net = self.vnet.clone();
        let fut = net.connect(msg.desc).into_actor(self);
        ActorResponse::r#async(fut)
    }
}

#[derive(Message)]
#[rtype(result = "(Result<ConnectionChannels>, bool)")]
struct Register {
    desc: SocketDesc,
}

#[derive(Message)]
#[rtype(result = "Result<()>")]
struct Unregister {
    desc: SocketDesc,
}

#[derive(Message)]
#[rtype(result = "Result<()>")]
struct Connect {
    desc: SocketDesc,
}

#[derive(Clone, Default)]
pub struct Routes {
    state: Arc<RwLock<RouteState>>,
}

impl Routes {
    #[inline]
    pub async fn get(&self, listen: SocketAddr) -> Option<SocketAddr> {
        let state = self.state.read().await;
        state.routes.get(&listen).copied()
    }

    #[inline]
    pub async fn add(&self, listen: SocketAddr, to: SocketAddr) {
        let mut state = self.state.write().await;
        state.routes.insert(listen, to);
    }

    #[inline]
    pub async fn remove(&self, listen: SocketAddr) -> Option<SocketAddr> {
        let mut state = self.state.write().await;
        state.routes.remove(&listen)
    }
}

#[derive(Default)]
struct RouteState {
    // when routes are dynamic
    #[allow(unused)]
    routes: HashMap<SocketAddr, SocketAddr>,
    // for UDP
    #[allow(unused)]
    channels: HashMap<SocketDesc, ConnectionChannels>,
}

struct ForwardContext {
    net: Addr<Network>,
    #[allow(unused)]
    routes: Routes,
    desc: SocketDesc,
}

async fn tcp_acceptor(
    net: Addr<Network>,
    routes: Routes,
    local: SocketAddr,
    listener: tokio::net::TcpListener,
) {
    // TODO: use routes for dynamic dispatching
    loop {
        let (stream, remote) = match listener.accept().await {
            Ok((stream, remote)) => (stream, remote),
            Err(err) => {
                log::error!("Local socket {local} error: {err}");
                break;
            }
        };

        let ctx = ForwardContext {
            net: net.clone(),
            routes: routes.clone(),
            desc: SocketDesc {
                protocol: Protocol::Tcp,
                local: local.into(),
                remote: remote.into(),
            },
        };

        tokio::task::spawn_local(tcp_forwarder(ctx, stream));
    }
}

async fn tcp_forwarder(ctx: ForwardContext, stream: TcpStream) {
    log::info!("New connection: {:?}", ctx.desc);

    let _ = async move {
        // we're overwriting existing connections in case of TCP
        let (channels, _) = v_register(ctx.net.clone(), ctx.desc).await?;
        v_connect(ctx.net, ctx.desc).await?;

        let i_rx = channels
            .ingress
            .receiver()
            .ok_or_else(|| Error::Manager("Ingress TCP channel already taken".to_string()))?;
        let s_tx = channels.send.sender();

        let (reader, writer) = tokio::io::split(stream);

        tokio::task::spawn_local(forward_tcp_to_vnet(ctx.desc, reader, s_tx));
        tokio::task::spawn_local(forward_vnet_to_tcp(ctx.desc, i_rx, writer));

        Ok::<_, Error>(())
    }
    .map_err(|e| log::error!("{e}"))
    .await;
}

// Forwards data from a locally bound socket to a sink
async fn forward_tcp_to_vnet(
    desc: SocketDesc,
    reader: ReadHalf<TcpStream>,
    tx: mpsc::Sender<Vec<u8>>,
) {
    aux_reader_to_sink(reader, tx).await;
    log::info!("Socket to virtual network channel stopped for {desc:?}");
}

// Forwards stream to a locally bound socket
async fn forward_vnet_to_tcp(
    desc: SocketDesc,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: WriteHalf<TcpStream>,
) {
    aux_stream_to_writer(rx, writer).await;
    log::info!("Virtual network to socket channel stopped for {desc:?}");
}

async fn aux_reader_to_sink<S, E, T>(mut reader: ReadHalf<T>, mut tx: S)
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
    T: AsyncRead,
{
    let mut buf = [0u8; READER_BUFFER_SIZE];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                log::error!("Reader to sink channel error: {e}");
                break;
            }
        }
    }
}

async fn aux_stream_to_writer<S, T>(mut rx: S, mut writer: WriteHalf<T>)
where
    S: Stream<Item = Vec<u8>> + Unpin,
    T: AsyncWrite,
{
    while let Some(vec) = rx.next().await {
        let mut idx = 0 as usize;

        loop {
            match writer.write(&vec[idx..]).await {
                Ok(0) => break,
                Ok(n) => idx += n,
                Err(e) => {
                    log::error!("Stream to writer channel error: {e}");
                    break;
                }
            }

            if idx >= vec.len() {
                break;
            }
        }
    }
}

async fn v_register(net: Addr<Network>, desc: SocketDesc) -> Result<(ConnectionChannels, bool)> {
    match net.send(Register { desc }).await {
        Ok((Ok(channels), pending)) => Ok((channels, pending)),
        Ok((Err(e), _)) => Err(Error::Manager(format!(
            "Error adding connection {desc:?}: {e}"
        ))),
        Err(_) => Err(Error::Manager(format!(
            "Cannot add connection {desc:?}: virtual network not started"
        ))),
    }
}

async fn v_connect(net: Addr<Network>, desc: SocketDesc) -> Result<()> {
    match net.send(Connect { desc }).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(Error::Manager(format!("Error connecting to {desc:?}: {e}"))),
        Err(_) => Err(Error::Manager(format!(
            "Cannot connect to {desc:?}: virtual network not started"
        ))),
    }
}

fn create_tap(name: &str, address: Ipv4Addr) -> anyhow::Result<Tun> {
    Ok(TunBuilder::new()
        .name(name)
        .tap(true)
        .packet_info(false)
        .mtu(1500)
        .up()
        .address(address)
        .broadcast(Ipv4Addr::BROADCAST)
        .netmask(Ipv4Addr::new(255, 255, 255, 0))
        .try_build()
        .map_err(|e| anyhow::anyhow!("Failed to create a TAP interface: {}", e))?)
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    // FIXME: consider logging to a file
    env_logger::init();
    let args: Args = Args::parse();

    log::info!("Creating TAP interface {}", args.tap);
    let iface = create_tap(&args.tap, args.tap_addr)?;

    log::info!("Starting virtual network");
    let (tx_addr, rx_addr) = oneshot::channel();

    std::thread::spawn(move || {
        let system = System::new();
        system.block_on(async move {
            let (net, essentials) = Network::spawn(tx_addr);
            let (reader, writer) = tokio::io::split(iface);

            tokio::task::spawn_local(aux_reader_to_sink(reader, essentials.receive));
            tokio::task::spawn_local(aux_stream_to_writer(essentials.egress, writer));

            net.start();

            // TODO: abortable
            let _ = tokio::signal::ctrl_c().await;
        });
    });

    let net = tokio::time::timeout(Duration::from_millis(2000), rx_addr).await??;

    log::info!("Starting server on {}", args.listen);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;

    let routes = Routes::default();
    routes.add(args.listen, args.tap_service).await;

    tokio::task::spawn_local(tcp_acceptor(net, routes, args.listen, listener));

    // TODO: spawn an HTTP server
    tokio::signal::ctrl_c().await?;
    Ok(())
}
