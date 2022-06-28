use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use actix::{Actor, Addr};
use actix_rt::System;
use clap::Parser;
use futures::channel::oneshot;
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use ya_net_gateway::error::{Error, Result};
use ya_net_gateway::network::virt::{Channel, ConnectionChannels, VirtualNetworkConfig};
use ya_net_gateway::network::{Connect, Network, Register, Routes, Unregister};
use ya_relay_stack::smoltcp::wire;
use ya_relay_stack::{Protocol, SocketDesc};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const RUST_LOG_VAR: &str = "RUST_LOG";

const IP4_PROXY_ADDRESS: Ipv4Addr = Ipv4Addr::new(9, 0, 0x0d, 0x01);
const IP6_PROXY_ADDRESS: Ipv6Addr = IP4_PROXY_ADDRESS.to_ipv6_mapped();
const IP4_SERVICE_ADDRESS: Ipv4Addr = Ipv4Addr::new(9, 0, 0x0d, 0x02);
const IP6_SERVICE_ADDRESS: Ipv6Addr = IP4_SERVICE_ADDRESS.to_ipv6_mapped();
const SERVICE_PORT: u16 = 1;

const READER_BUFFER_SIZE: usize = 2048;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Socket to listen on
    #[clap(short, long, value_parser, default_value = "0.0.0.0:12345")]
    listen: SocketAddr,
}

struct ForwardContext {
    net: Addr<Network>,
    #[allow(unused)]
    routes: Routes,
    desc: SocketDesc,
}

pub async fn tcp_acceptor(
    net: Addr<Network>,
    routes: Routes,
    local: SocketAddr,
    listener: tokio::net::TcpListener,
) {
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(tuple) => tuple,
            Err(err) => {
                log::error!("[proxy] local socket {local} error: {err}");
                break;
            }
        };

        log::debug!("[proxy] accepted TCP connection from {from}");

        let remote = match routes.get(local).await {
            Some(addr) => addr,
            None => {
                log::error!("[proxy] forward destination not found for {local}");
                continue;
            }
        };

        let ctx = ForwardContext {
            net: net.clone(),
            routes: routes.clone(),
            desc: SocketDesc {
                protocol: Protocol::Tcp,
                local: from.into(),
                remote: remote.into(),
            },
        };

        tokio::task::spawn_local(tcp_forwarder(ctx, stream));
    }
}

async fn tcp_forwarder(ctx: ForwardContext, stream: TcpStream) {
    let _ = async move {
        // overwrite existing entries for TCP
        let (channels, _) = net_register(ctx.net.clone(), ctx.desc).await?;
        net_connect(ctx.net.clone(), ctx.desc).await?;

        let tx = channels.send.sender();
        let rx = channels
            .ingress
            .receiver()
            .ok_or_else(|| Error::Network("Ingress TCP channel already taken".to_string()))?;

        let (reader, writer) = tokio::io::split(stream);

        tokio::task::spawn_local(aux_reader_to_sink(reader, tx).then(move |_| {
            net_unregister(ctx.net, ctx.desc)
                .map_err(|e| log::error!("{e}"))
                .then(|_| futures::future::ready(()))
        }));
        tokio::task::spawn_local(aux_stream_to_writer(rx, writer));

        Ok::<_, Error>(())
    }
    .map_err(|e| log::error!("{e}"))
    .await;
}

pub async fn aux_reader_to_sink<S, E, T>(mut reader: ReadHalf<T>, mut tx: S)
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
                log::error!("[proxy] reader to sink channel error: {e}");
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
                    log::error!("[proxy] stream to writer channel error: {e}");
                    break;
                }
            }
            if idx >= vec.len() {
                break;
            }
        }
    }
}

async fn aux_forward<St, Si, E>(mut rx: St, mut tx: Si)
where
    St: Stream<Item = Vec<u8>> + Unpin,
    Si: Sink<Vec<u8>, Error = E> + Unpin,
{
    while let Some(data) = rx.next().await {
        if tx.send(data).await.is_err() {
            break;
        }
    }
}

// helper: creates TCP connection communication channels via `Network` actor
// this is done BEFORE connecting so that we can exchange any prior traffic
// (e.g. ARP requests / responses)
async fn net_register(net: Addr<Network>, desc: SocketDesc) -> Result<(ConnectionChannels, bool)> {
    match net.send(Register { desc }).await {
        Ok((Ok(channels), pending)) => Ok((channels, pending)),
        Ok((Err(e), _)) => Err(Error::Network(format!(
            "Error adding connection {desc:?}: {e}"
        ))),
        Err(_) => Err(Error::Network(format!(
            "Cannot add connection {desc:?}: virtual network not started"
        ))),
    }
}

// helper: closes a TCP connection using the `Network` actor
async fn net_unregister(net: Addr<Network>, desc: SocketDesc) -> Result<()> {
    match net.send(Unregister { desc }).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(Error::Network(format!(
            "Error disconnecting from {desc:?}: {e}"
        ))),
        Err(_) => Err(Error::Network(format!(
            "Cannot disconnect from {desc:?}: virtual network not started"
        ))),
    }
}

// helper: establishes a TCP connection using the `Network` actor
async fn net_connect(net: Addr<Network>, desc: SocketDesc) -> Result<()> {
    match net.send(Connect { desc }).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(Error::Network(format!("Error connecting to {desc:?}: {e}"))),
        Err(_) => Err(Error::Network(format!(
            "Cannot connect to {desc:?}: virtual network not started"
        ))),
    }
}

// the service - logs received bytes as strings
async fn service_thread<S>(mut rx: S)
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    while let Some(data) = rx.next().await {
        log::info!(
            "[service]: {}",
            String::from_utf8_lossy(&data[..]).replace("\n", "")
        );
    }
    log::info!("[service] stopped");
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    const DEFAULT_LOG: &str = "trace,mio=info,smoltcp=info,ya_relay_stack=info";
    const NET_SPAWN_TIMEOUT: Duration = Duration::from_millis(2000);

    std::env::set_var(
        RUST_LOG_VAR,
        std::env::var(RUST_LOG_VAR).unwrap_or_else(|_| DEFAULT_LOG.to_string()),
    );
    env_logger::init();

    let args: Args = Args::parse();

    log::info!("Starting {NAME} v{VERSION}");
    let remote = SocketAddr::new(IP4_SERVICE_ADDRESS.into(), SERVICE_PORT);
    let routes = Routes::default();
    routes.add(args.listen, remote).await;

    // service to proxy channel
    let s_to_p = Channel::default();
    // proxy to service channel
    let p_to_s = Channel::default();

    log::info!("[service] initializing smoltcp stack");
    let conf = VirtualNetworkConfig {
        mtu: 1500,
        hw_addr: wire::HardwareAddress::Ethernet(wire::EthernetAddress([
            0xb0, 0x01, 0x01, 0x01, 0x01, 0x01,
        ])),
        ip4_addr: IP4_SERVICE_ADDRESS,
        ip6_addr: IP6_SERVICE_ADDRESS,
        capture_ingress: true,
    };

    // receive outgoing packets from proxy
    let p_s_rx = p_to_s.receiver().unwrap();
    // send outgoing packets to proxy
    let s_p_tx = s_to_p.sender();

    let (tx_addr, rx_addr) = oneshot::channel();
    std::thread::spawn(move || {
        let system = System::new();
        system.block_on(async move {
            let (net, channels) = Network::spawn(conf);
            net.bind(Protocol::Tcp, IP4_SERVICE_ADDRESS.into(), SERVICE_PORT)
                .expect("[service] failed to bind socket");

            // receive outgoing packets from proxy
            tokio::task::spawn_local(aux_forward(p_s_rx, channels.receive));
            // send outgoing packets to proxy
            tokio::task::spawn_local(aux_forward(channels.egress, s_p_tx));
            // service consumes incoming socket payload and logs it as strings
            tokio::task::spawn_local(service_thread(channels.ingress.unwrap()));

            log::info!("[service] running");

            let addr = net.start();
            let _ = tx_addr.send(addr);
            let _ = tokio::signal::ctrl_c().await;
        });
    });
    let _ = tokio::time::timeout(NET_SPAWN_TIMEOUT, rx_addr).await??;

    log::info!("[proxy] initializing smoltcp stack");
    let conf = VirtualNetworkConfig {
        mtu: 1500,
        hw_addr: wire::HardwareAddress::Ethernet(wire::EthernetAddress([
            0xb0, 0x02, 0x02, 0x02, 0x02, 0x02,
        ])),
        ip4_addr: IP4_PROXY_ADDRESS,
        ip6_addr: IP6_PROXY_ADDRESS,
        capture_ingress: false,
    };

    // receive outgoing packets from service
    let s_p_rx = s_to_p.receiver().unwrap();
    // send outgoing packets to service
    let p_s_tx = p_to_s.sender();

    let (tx_addr, rx_addr) = oneshot::channel();
    std::thread::spawn(move || {
        let system = System::new();
        system.block_on(async move {
            let (net, channels) = Network::spawn(conf);

            // receive outgoing packets from service
            tokio::task::spawn_local(aux_forward(s_p_rx, channels.receive));
            // send outgoing packets to service
            tokio::task::spawn_local(aux_forward(channels.egress, p_s_tx));

            let addr = net.start();
            let _ = tx_addr.send(addr);
            let _ = tokio::signal::ctrl_c().await;
        });
    });
    let net = tokio::time::timeout(NET_SPAWN_TIMEOUT, rx_addr).await??;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;

    log::info!("[proxy] listening on {}", args.listen);
    tokio::task::spawn_local(tcp_acceptor(net, routes, args.listen, listener));

    tokio::signal::ctrl_c().await?;
    Ok(())
}
