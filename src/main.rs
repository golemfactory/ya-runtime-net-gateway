/* SPDX-License-Identifier: GPL-3.0-or-later
 * (c) 2022  Wojtek Porczyk <woju@invisiblethingslab.com>
 */

#![allow(unused_imports)]

use std::error::Error;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;
use std::rc::Rc;


use actix_web;
use actix_web::{web, get, post, delete};

use anyhow;


use smoltcp;
use smoltcp::iface;
use smoltcp::phy;
use smoltcp::socket;
//use smoltcp::time;
use smoltcp::wire;

use tokio;
use tokio::io::Interest;
//use tokio::sync::mpsc;
use futures::StreamExt;
use futures::channel::mpsc;

//use tokio_smoltcp::device;

use ya_relay_stack;

struct Connection {
    acceptor: tokio::task::JoinHandle<()>,
    forwarders: Vec<tokio::task::JoinHandle<()>>,
}

struct ConnectionManager {
    connections: std::collections::HashMap::<u32, Connection>,
}

/*
impl ConnectionManager {
    fn new(net: ya_relay_stack::Network) -> ConnectionManager {
        ConnectionManager {
            connections: std::collections::HashMap::<u32, Connection>::new(),
            prev_id: 0,
            net: net,
        }
    }

    async fn delete(&mut self, id: u32) {
        todo!()
    }
}
*/

/*
 * VPN adapter
 */


//async fn sender(&iface) {
//  let tcp_handle = iface.add_socket(socket::TcpSocket::new(
//      socket::TcpSocketBuffer::new(vec![0; 64]),
//      socket::TcpSocketBuffer::new(vec![0; 128]),
//  ));

//  let (socket, cx) = iface.get_socket_and_context::<tcp::Socket>(tcp_handle);
//  socket.connect(cx, (10.5.6.254, 6789), 45678).unwrap();
//  let mut tcp_is_active = false;
//}


#[derive(Debug)]
enum SmoltcpRequest {
    Connect(mpsc::Receiver<Vec<u8>>), // (remote address+port, some id)
    Disconnect, // (id)
}

async fn network_thread(net_receiver: mpsc::Receiver<SmoltcpRequest>) {
    let hwaddr = wire::HardwareAddress::Ethernet(
        wire::EthernetAddress([0xb0, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5]));
    let iface = ya_relay_stack::interface::tap_iface(hwaddr, 1280);

    let netconfig = Rc::new(ya_relay_stack::NetworkConfig {
        max_transmission_unit: 1280,
        buffer_size_multiplier: 32,
    });

    let net = ya_relay_stack::Network::new(
        "proxy",
        netconfig.clone(),
        ya_relay_stack::Stack::new(iface, netconfig));

    net.spawn_local();

    tokio_stream::wrappers::ReceiverStream::new(net_receiver).for_each(move |request| {
        match request {
            SmoltcpRequest::Connect(inet2vpn_receiver) => {
                dbg!(inet2vpn_receiver);
            },
            SmoltcpRequest::Disconnect => todo!(),
        }
    });
}


async fn acceptor(listener: tokio::net::TcpListener, net_sender: mpsc::Sender<SmoltcpRequest>) {
//  let channel: ya_relay_stack::Channel::<Vec<u8>> = Default::default();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        dbg!("accepted"); //TODO metrics
        tokio::spawn(forwarder(stream, net_sender.clone())); //, channel.tx));
    }
}

// TODO jeszcze argument dokąd to będzie forwardować
async fn forwarder(stream: tokio::net::TcpStream, mut net_sender: mpsc::Sender<SmoltcpRequest>) {

    // TODO kanał w drugą stronę...
    let (inet2vpn_sender, inet2vpn_receiver) = mpsc::channel::<Vec<u8>>(10);
    net_sender.try_send(SmoltcpRequest::Connect(inet2vpn_receiver)).expect("failed to ::Connect()");

    loop {
        let ready = stream.ready(Interest::READABLE).await.unwrap(); // TODO | Interest::WRITABLE
        if ready.is_readable() {
            let mut data = vec![0; 1024];
            match stream.try_read(&mut data) {
                Ok(0) => break,
                Ok(n) => {
                    // TODO put into remote end
                    // TODO metrics
                    println!("stream read");
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => {
                    dbg!("error", e);
                    return;
                }
            }
        }
    }
//  inet2vpn_sender.disconnect(); // albo close()?
}


/*
 * control API
 */

struct WebData {
    net_sender: mpsc::Sender<SmoltcpRequest>,
    prev_id: u32,
}

#[post("/tcp")]
async fn tcp_post(data: web::Data<Mutex<WebData>>) -> String {
    let mut data = data.lock().unwrap();
    data.prev_id += 1;
    let id = data.prev_id;

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4567").await.unwrap();
    let acceptor = tokio::spawn(acceptor(listener, data.net_sender.clone()));
    format!("id {}", id)
}

/*
    async fn create(&mut self) -> u32 {
//      self.connections.insert(id, conn);
        id
    }
*/

#[delete("/tcp")]
async fn tcp_delete(data: web::Data<Mutex<WebData>>) -> String {
    let data = data.lock().unwrap();
    format!("Hello, world! {}", data.prev_id)
}

#[get("/metrics")]
async fn metrics(data: web::Data<Mutex<ConnectionManager>>) -> String {
    format!("\
        TODO_metrics{{iface=\"1\"}} {}\n\
        TODO_metrics{{iface=\"2\"}} {}\n\
    ", 1, 2)
}

// old get interface (plain smoltcp)
/*
    let ethernet_addr = wire::EthernetAddress([0xb0, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5]);
    let ip_addrs = [
        wire::IpCidr::new(wire::IpAddress::v4(10, 5, 6, 1), 24),
    ];

//  let mut routes_storage = [None; 1];
//  let mut routes = Routes::new(&mut routes_storage[..]);
//  routes.add_default_ipv4_route(wire::Ipv4Address::new(10, 5, 6, 254)).unwrap();

    let neighbor_cache = iface::NeighborCache::new(std::collections::BTreeMap::new());

    let device = phy::TunTapInterface::new("proxy", phy::Medium::Ethernet)
        .expect("TODO add error handling");
    let fd = device.as_raw_fd();

    let iface = iface::InterfaceBuilder::new(device, vec![])
        .ip_addrs(ip_addrs)
        .hardware_addr(ethernet_addr.into())
        .neighbor_cache(neighbor_cache)
        .finalize();
*/

#[actix_web::main]
async fn main() {
    let (net_sender, net_receiver) = mpsc::channel(100);
    let jh_smoltcp = tokio::task::spawn_local(network_thread(net_receiver));

    let data = web::Data::new(Mutex::new(WebData { net_sender: net_sender, prev_id: 0 }));

    actix_web::HttpServer::new(move || {
        actix_web::App::new()
            .app_data(data.clone())
            .service(tcp_post)
            .service(tcp_delete)
            .service(metrics)
    })
    .bind(("127.0.0.1", 8000)).expect("failet do bind()")
    .run()
    .await.expect("await failed");
}
