use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use actix::{Actor, ActorResponse, Addr, AsyncContext, Context, Handler, Message, WrapFuture};
use futures::channel::oneshot;
use futures::future::Either;
use tokio::sync::RwLock;

use ya_relay_stack::{Protocol, SocketDesc};

use crate::error::Result;
use crate::network::virt::{
    ConnectionChannels, NetworkChannels, VirtualNetwork, VirtualNetworkConfig,
};

pub struct Network {
    vnet: VirtualNetwork,
    ready_tx: Option<oneshot::Sender<Addr<Self>>>,
}

impl Network {
    pub fn spawn(
        conf: VirtualNetworkConfig,
        ready_tx: oneshot::Sender<Addr<Self>>,
    ) -> (Self, NetworkChannels) {
        let (vnet, channels) = VirtualNetwork::spawn(conf);
        let this = Self {
            vnet,
            ready_tx: Some(ready_tx),
        };
        (this, channels)
    }

    // FIXME: remove - required for this example only
    pub fn bind(&self, protocol: Protocol, addr: IpAddr, port: u16) -> Result<()> {
        self.vnet.bind(protocol, addr, port)
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
        let fut = match self.vnet.open_channel(msg.desc) {
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
pub struct Register {
    pub desc: SocketDesc,
}

#[derive(Message)]
#[rtype(result = "Result<()>")]
pub struct Unregister {
    pub desc: SocketDesc,
}

#[derive(Message)]
#[rtype(result = "Result<()>")]
pub struct Connect {
    pub desc: SocketDesc,
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
