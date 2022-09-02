#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("Network error: {0}")]
    Stack(#[from] ya_relay_stack::Error),
    #[error("{0}")]
    Network(String),
    #[error("I/O: {0}")]
    Io(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self::Network(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
