use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CreateTcpUdp {
    // XXX this probably needs only port! allowing IP address could be an oracle if the real IP
    // address (internal, after NAT) needs to remain confidential
	pub listen: String,

    // XXX rename to "forward"?
    pub remote: String,
}


#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TcpUdp {
    pub id: u32,
}

pub fn next_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static ATOMIC_ID: AtomicU32 = AtomicU32::new(0);
    let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
    id
}

pub type ProbeToken = [u8; 32];

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CreateProbe {
    pub token: ProbeToken,
    pub listen: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Probe {
    pub id: u32,
}
