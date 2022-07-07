use crate::{web::WebClient, Result};
use ya_net_gateway_model::{
    CreateTcpUdp, TcpUdp,
};

#[derive(Clone)]
pub struct ManagementApi {
    client: WebClient,
}

impl ManagementApi {
    pub fn new(client: WebClient) -> Self {
        Self { client }
    }

    pub async fn create_tcp(&self, cs: &CreateTcpUdp) -> Result<TcpUdp> {
        self.client.post("tcp", cs).await
    }

    pub async fn delete_tcp(&self, id: u32) -> Result<()> {
        let url = format!("tcp/{}", id);
        self.client.delete(&url).await
    }

    pub async fn create_udp(&self, cs: &CreateTcpUdp) -> Result<TcpUdp> {
        self.client.post("udp", cs).await
    }

    pub async fn delete_udp(&self, id: u32) -> Result<()> {
        let url = format!("udp/{}", id);
        self.client.delete(&url).await
    }
}
