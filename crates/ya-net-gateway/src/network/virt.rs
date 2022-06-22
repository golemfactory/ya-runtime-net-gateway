use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::channel::mpsc;
use futures::future::{Either, LocalBoxFuture, Shared};
use futures::stream::BoxStream;
use futures::{FutureExt, SinkExt, StreamExt};
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_stack::connection::ConnectionMeta;
use ya_relay_stack::smoltcp::wire;
use ya_relay_stack::smoltcp::wire::IpCidr;
use ya_relay_stack::{IngressEvent, IngressReceiver, Network, Protocol, SocketDesc};

use crate::error::{Error, Result};

pub type MacAddr = [u8; 6];

#[derive(Clone, Debug)]
pub struct VirtualNetworkConfig {
    pub mtu: usize,
    pub hw_addr: wire::HardwareAddress,
    pub ip4_addr: Ipv4Addr,
    pub ip6_addr: Ipv6Addr,
    pub capture_ingress: bool,
}

#[derive(Clone)]
pub struct VirtualNetwork {
    net: Network,
    conns: Rc<RefCell<HashMap<SocketDesc, Connection>>>,
}

impl VirtualNetwork {
    pub fn spawn(conf: VirtualNetworkConfig) -> (Self, NetworkChannels) {
        let capture_ingress = conf.capture_ingress;
        let net = create_network(conf);
        net.spawn_local();

        let ingress_rx = net.ingress_receiver().unwrap();
        let egress_rx = net.egress_receiver().unwrap();
        let egress_rx =
            StreamExt::boxed(UnboundedReceiverStream::new(egress_rx).map(|e| e.payload.into_vec()));

        let conns = Default::default();
        let this = Self { net, conns };

        let (receive_tx, receive_rx) = mpsc::channel(1);
        let mut channels = NetworkChannels {
            ingress: None,
            egress: egress_rx,
            receive: receive_tx,
        };

        if capture_ingress {
            let stream = UnboundedReceiverStream::new(ingress_rx).filter_map(|e| async move {
                if let IngressEvent::Packet { payload, .. } = e {
                    return Some(payload);
                }
                None
            });
            channels.ingress = Some(StreamExt::boxed(stream));
        } else {
            tokio::task::spawn_local(this.clone().handle_stack_ingress(ingress_rx));
        }
        tokio::task::spawn_local(this.clone().handle_stack_receive(receive_rx));

        (this, channels)
    }

    // channels are required for passing any traffic required for TCP (i.e. ARP)
    // first, we create a channel. When the channel is resolved, we can establish a connection.
    pub fn open_channel(
        &self,
        desc: SocketDesc,
    ) -> Either<impl Future<Output = Result<ConnectionChannels>>, Result<ConnectionChannels>> {
        let conn = { self.conns.borrow().get(&desc).cloned() };
        match conn {
            Some(conn) => match (*conn.state.borrow()).clone() {
                ConnectionState::Pending(PendingConnection { ready, .. }, _) => {
                    Either::Left(ready.boxed_local())
                }
                ConnectionState::Established(_, channels) => {
                    Either::Left(futures::future::ok(channels).boxed_local())
                }
                ConnectionState::Poisoned => panic!("Programming error: poisoned channel"),
            },
            None => Either::Right(self.create_channel(desc)),
        }
    }

    pub fn close_channel(&self, _desc: SocketDesc) -> Result<()> {
        todo!()
    }

    fn create_channel(&self, desc: SocketDesc) -> Result<ConnectionChannels> {
        let channels = ConnectionChannels::default();
        let (ready_tx, mut ready_rx) = mpsc::channel(1);
        let ready = async move {
            match ready_rx.next().await {
                Some(Ok(channels)) => Ok(channels),
                Some(Err(err)) => Err(err),
                None => Err(Error::Network("Failed to finalize connection".to_string())),
            }
        }
        .boxed_local()
        .shared();

        let pending = PendingConnection { ready_tx, ready };
        let state_pending = ConnectionState::Pending(pending, channels.clone());
        let state = Rc::new(RefCell::new(state_pending));

        let mut conns = self.conns.borrow_mut();
        conns.insert(desc, Connection { state });

        Ok(channels)
    }

    pub fn bind(&self, protocol: Protocol, addr: IpAddr, port: u16) -> Result<()> {
        let _ = self.net.bind(protocol, (addr, port))?;
        self.net.poll();
        Ok(())
    }

    pub fn connect(&self, desc: SocketDesc) -> LocalBoxFuture<'static, Result<()>> {
        let this = self.clone();
        let conns = self.conns.clone();
        let conn = {
            let conns = conns.borrow();
            conns.get(&desc).cloned()
        };

        async move {
            let mut conn = match conn {
                Some(conn) => conn,
                _ => {
                    let msg = format!("Connection unexpectedly removed: {:?}", desc);
                    return Err(Error::Network(msg));
                }
            };

            let vconn = this.connect_stack(desc).await?;
            conn.establish(vconn);

            let state = conn.state.borrow().clone();
            let (established, channels) = match state {
                ConnectionState::Established(established, channels) => (established, channels),
                _ => {
                    let msg = format!("Connection not established: {:?}", desc);
                    return Err(Error::Network(msg));
                }
            };

            tokio::task::spawn_local(this.clone().handle_conn_send(established, channels.clone()));

            if let Some(mut tx) = conn.ready_tx() {
                let _ = tx.send(Ok(channels)).await;
            }

            Ok(())
        }
        .then(move |result| async move {
            if let Err(ref e) = result {
                let conn = {
                    let mut conns_ = conns.borrow_mut();
                    conns_.remove(&desc)
                };
                if let Some(Some(mut tx)) = conn.map(|c| c.ready_tx()) {
                    let err = Error::Network(format!("Connection failed: {e}"));
                    let _ = tx.send(Err(err)).await;
                }
            }
            result
        })
        .boxed_local()
    }

    async fn connect_stack(&self, desc: SocketDesc) -> Result<ya_relay_stack::Connection> {
        let conn = if desc.protocol == Protocol::Tcp {
            self.net
                .connect(desc.remote.ip_endpoint()?, Duration::from_millis(2000))
                .await?
        } else if let Some(handle) = self.net.get_bound(desc.protocol, desc.local) {
            let meta = ConnectionMeta::try_from(desc)?;
            ya_relay_stack::Connection { handle, meta }
        } else {
            let handle = self.net.bind(desc.protocol, desc.local)?;
            let meta = ConnectionMeta::try_from(desc)?;
            ya_relay_stack::Connection { handle, meta }
        };

        Ok(conn)
    }

    fn get_connection<F, T>(&self, desc: &SocketDesc, f: F) -> Option<T>
    where
        F: Fn(&ConnectionChannels) -> T,
    {
        let conns = self.conns.borrow();
        match conns.get(desc) {
            Some(conn) => {
                let state = conn.state.borrow();
                match &*state {
                    ConnectionState::Established(_, channels) => return Some(f(channels)),
                    _ => log::info!("Connection to {:?} is in invalid state", desc),
                }
            }
            _ => log::error!("Unable to route ingress packet: no connection"),
        }
        None
    }

    // handle ingress packets emitted from the stack
    async fn handle_stack_ingress(self, mut rx: IngressReceiver) {
        while let Some(evt) = rx.recv().await {
            let (desc, payload) = match evt {
                IngressEvent::InboundConnection { desc } => {
                    // TODO: handle
                    log::info!("Ingress: connection from {:?}", desc);
                    continue;
                }
                IngressEvent::Disconnected { desc } => {
                    // TODO: handle
                    log::info!("Ingress: disconnected from {:?}", desc);
                    continue;
                }
                IngressEvent::Packet { desc, payload } => (desc, payload),
            };

            log::info!("Ingress: packet from {:?}: {:?}", desc, payload);

            if let Some(mut tx) = self.get_connection(&desc, |c| c.ingress.tx.clone()) {
                if let Err(e) = tx.send(payload).await {
                    log::error!("Ingress: unable to route packet: {e}");
                }
            } else {
                log::error!("Ingress: no connection to {:?}", desc);
            }
        }
    }

    // handle frame to be sent directly to iface receipt buffer
    async fn handle_stack_receive(self, mut rx: mpsc::Receiver<Vec<u8>>) {
        while let Some(data) = rx.next().await {
            self.net.receive(data);
            self.net.poll();
        }
    }

    // handle payload to be sent via a virtual socket;
    // most probably will generate an egress event
    async fn handle_conn_send(
        self,
        established: EstablishedConnection,
        channels: ConnectionChannels,
    ) {
        let mut rx = channels.send.receiver().unwrap();

        while let Some(data) = rx.next().await {
            match self.net.send(data, established.vconn) {
                Ok(send) => {
                    if let Err(e) = send.await {
                        log::warn!("Send via {:?} failed: {e}", established.vconn.meta);
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("Send via {:?} failed: {e}", established.vconn.meta);
                    break;
                }
            }
        }
        self.net.poll();
    }
}

pub struct NetworkChannels {
    // stream of packets received by _any_ socket (here - on purpose)
    pub ingress: Option<BoxStream<'static, Vec<u8>>>,
    // stream of Ethernet frames emitted by the stack
    pub egress: BoxStream<'static, Vec<u8>>,
    // sink for Ethernet frames to be received by the stack
    pub receive: mpsc::Sender<Vec<u8>>,
}

#[derive(Clone, Default)]
pub struct ConnectionChannels {
    // channel for injecting packets into the virtual net stack
    pub send: Channel<Vec<u8>>,
    // channel for receiving incoming packets
    pub ingress: Channel<Vec<u8>>,
}

#[derive(Clone)]
pub struct Channel<T> {
    tx: mpsc::Sender<T>,
    rx: Arc<RwLock<Option<mpsc::Receiver<T>>>>,
}

impl<T> Channel<T> {
    pub fn sender(&self) -> mpsc::Sender<T> {
        self.tx.clone()
    }

    pub fn receiver(&self) -> Option<mpsc::Receiver<T>> {
        let mut rx = self.rx.write().unwrap();
        rx.take()
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(1);
        let rx = Arc::new(RwLock::new(Some(rx)));
        Self { tx, rx }
    }
}

#[derive(Clone)]
pub struct Connection {
    pub state: Rc<RefCell<ConnectionState>>,
}

impl Connection {
    pub fn channels(&self) -> ConnectionChannels {
        let state = self.state.borrow();
        match &*state {
            ConnectionState::Pending(_, channels) | ConnectionState::Established(_, channels) => {
                channels.clone()
            }
            ConnectionState::Poisoned => panic!("Programming error: poisoned connection"),
        }
    }

    pub fn ready_tx(&self) -> Option<mpsc::Sender<Result<ConnectionChannels>>> {
        let state = self.state.borrow();
        match &*state {
            ConnectionState::Pending(pending, _) => Some(pending.ready_tx.clone()),
            _ => None,
        }
    }

    pub fn establish(&mut self, vconn: ya_relay_stack::Connection) {
        let mut state = self.state.borrow_mut();
        *state = match std::mem::replace(&mut *state, ConnectionState::Poisoned) {
            ConnectionState::Pending(_, channels) => {
                ConnectionState::Established(EstablishedConnection { vconn }, channels)
            }
            s => s,
        };
    }
}

#[derive(Clone)]
pub enum ConnectionState {
    Pending(PendingConnection, ConnectionChannels),
    Established(EstablishedConnection, ConnectionChannels),
    Poisoned,
}

#[derive(Clone)]
pub struct PendingConnection {
    ready_tx: mpsc::Sender<Result<ConnectionChannels>>,
    ready: Shared<LocalBoxFuture<'static, Result<ConnectionChannels>>>,
}

#[derive(Clone)]
pub struct EstablishedConnection {
    pub vconn: ya_relay_stack::Connection,
}

fn create_network(conf: VirtualNetworkConfig) -> Network {
    let net_conf = Rc::new(ya_relay_stack::NetworkConfig {
        max_transmission_unit: conf.mtu,
        buffer_size_multiplier: 32,
    });

    let iface = ya_relay_stack::interface::tap_iface(conf.hw_addr, conf.mtu);
    let stack = ya_relay_stack::Stack::new(iface, net_conf.clone());

    stack.add_address(IpCidr::new(conf.ip4_addr.into(), 0));
    stack.add_address(IpCidr::new(conf.ip6_addr.into(), 0));

    Network::new("proxy", net_conf, stack)
}
