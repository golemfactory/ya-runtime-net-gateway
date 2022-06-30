use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix::{Actor, Addr};
use actix_rt::System;
use actix_web;
use actix_web::{web, get, post, delete};
use clap::Parser;
use futures::channel::oneshot;
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use serde;
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

async fn udp_forwarder(
    net: Addr<Network>,
    routes: Routes,
    local: SocketAddr,
    socket: tokio::net::UdpSocket,
) {
    let remote = match routes.get(local).await {
        Some(addr) => addr,
        None => {
            log::error!("Forward destination not found for {local}");
            return;
        }
    };

    let desc = SocketDesc {
        protocol: Protocol::Udp,
        local: local.into(),
        remote: remote.into(),
    };

    let _ = async move {
        let (channels, _) = net_register(net.clone(), desc).await?;
        net_connect(net, desc).await?;

        let mut tx = channels.send.sender();
        let mut rx = channels
            .ingress
            .receiver()
            .ok_or_else(|| Error::Network("Ingress UDP channel already taken".to_string()))?;

        let socket_recv = Arc::new(socket);
        let socket_send = socket_recv.clone();

        tokio::task::spawn_local(async move {
            let mut buf = [0u8; READER_BUFFER_SIZE];
            loop {
                match socket_recv.recv(&mut buf).await {
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
        });

        tokio::task::spawn_local(async move {
            while let Some(vec) = rx.next().await {
                let mut idx = 0 as usize;
                loop {
                    match socket_send.send(&vec[idx..]).await {
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
        });

        Ok::<_, Error>(())
    }
    .map_err(|e| log::error!("{e}"))
    .await;
}


// helper: creates TCP connection or UDP "connection" communication channels via `Network` actor
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
// also, it establishes UDP "connection" in the sense that we forward all relevant UDP packets to
// a certain endpoint in `Network`
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


/*
 * control API
 */

struct WebData {
    prev_id: u32,
    routes_tcp: Routes,
    routes_udp: Routes,
    net: Addr<Network>,
}

#[derive(serde::Deserialize)]
struct TcpUdpConnectRequest {
    // XXX this probably needs only port! allowing IP address could be an oracle if the real IP
    // address (internal, after NAT) needs to remain confidential
	listen: String,

    // XXX rename to "forward"?
    remote: String,
}

#[derive(serde::Serialize)]
struct TcpUdpConnectResponse {
    id: u32,
}

#[post("/tcp")]
async fn tcp_post(
    data: web::Data<Mutex<WebData>>, req: web::Json<TcpUdpConnectRequest>
) -> actix_web::Result<actix_web::HttpResponse> {
    let mut data = data.lock().unwrap();
    data.prev_id += 1;

    let listen: SocketAddr;
    match req.listen.parse() {
        Ok(l) => listen = l,
        Err(e) => return Ok(actix_web::HttpResponse::BadRequest().body(format!("listen: {e}"))),
    };

    let remote: SocketAddr;
    match req.remote.parse() {
        Ok(r) => remote = r,
        Err(e) => return Ok(actix_web::HttpResponse::BadRequest().body(format!("remote: {e}"))),
    };

    log::debug!("listen: {listen:?} remote: {remote:?}");

    // XXX this badly needs error handling in case `listen` is duplicated
    data.routes_tcp.add(listen, remote).await;

    let listener = tokio::net::TcpListener::bind(listen).await?;

    log::info!("[proxy] listening on {}/tcp", listen);
    tokio::task::spawn_local(tcp_acceptor(data.net.clone(), data.routes_tcp.clone(), listen, listener));

    Ok(actix_web::HttpResponse::Ok().json(TcpUdpConnectResponse { id: data.prev_id }))
}

#[derive(serde::Serialize)]
struct TcpDisconnectResponse {
    // TODO
}

#[delete("/tcp/{id}")]
async fn tcp_delete(data: web::Data<Mutex<WebData>>, path: web::Path<(u32,)>) -> actix_web::Result<actix_web::HttpResponse> {
    let data = data.lock().unwrap();
    let (id,) = path.into_inner();

    log::debug!("close: {:?}", id);

    Ok(actix_web::HttpResponse::Ok().json(TcpDisconnectResponse {}))
}

#[post("/udp")]
async fn udp_post(
    data: web::Data<Mutex<WebData>>, req: web::Json<TcpUdpConnectRequest>
) -> actix_web::Result<actix_web::HttpResponse> {
    let mut data = data.lock().unwrap();
    data.prev_id += 1;

    let listen: SocketAddr;
    match req.listen.parse() {
        Ok(l) => listen = l,
        Err(e) => return Ok(actix_web::HttpResponse::BadRequest().body(format!("listen: {e}"))),
    };

    let remote: SocketAddr;
    match req.remote.parse() {
        Ok(r) => remote = r,
        Err(e) => return Ok(actix_web::HttpResponse::BadRequest().body(format!("remote: {e}"))),
    };

    log::debug!("listen: {listen:?} remote: {remote:?}");

    // XXX this badly needs error handling in case `listen` is duplicated
    data.routes_udp.add(listen, remote).await;

    let socket = tokio::net::UdpSocket::bind(listen).await?;

    log::info!("[proxy] listening on {}/udp", listen);
    tokio::task::spawn_local(udp_forwarder(data.net.clone(), data.routes_udp.clone(), listen, socket));

    Ok(actix_web::HttpResponse::Ok().json(TcpUdpConnectResponse { id: data.prev_id }))
}

#[get("/metrics")]
async fn metrics(data: web::Data<Mutex<WebData>>) -> String {
    format!("\
        TODO_metrics{{iface=\"1\"}} {}\n\
        TODO_metrics{{iface=\"2\"}} {}\n\
    ", 1, 2)
}


#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    const DEFAULT_LOG: &str = "trace,mio=info,smoltcp=info,ya_relay_stack=info,actix_http=info";
    const NET_SPAWN_TIMEOUT: Duration = Duration::from_millis(2000);

    std::env::set_var(
        RUST_LOG_VAR,
        std::env::var(RUST_LOG_VAR).unwrap_or_else(|_| DEFAULT_LOG.to_string()),
    );
    env_logger::init();

    let _args: Args = Args::parse();

    log::info!("Starting {NAME} v{VERSION}");

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

    let data = web::Data::new(Mutex::new(WebData {
		prev_id: 0,
        routes_tcp: Routes::default(),
        routes_udp: Routes::default(),
        net: tokio::time::timeout(NET_SPAWN_TIMEOUT, rx_addr).await??,
    }));

    actix_web::HttpServer::new(move || {
        actix_web::App::new()
            .app_data(data.clone())
            .service(tcp_post)
            .service(tcp_delete)
            .service(udp_post)
            .service(metrics)
    })
    .bind(("127.0.0.1", 8000)).expect("failet do bind()")
    .run()
    .await.expect("await failed");

    Ok(())
}
