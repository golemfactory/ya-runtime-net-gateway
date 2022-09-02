use futures::TryFutureExt;
use tokio;
use tokio::io::AsyncWriteExt;

use ya_net_gateway_model::ProbeToken;
use crate::error::Error;

pub async fn probe(
    token: ProbeToken,
    listen: std::net::SocketAddr,
    mut closed: tokio::sync::oneshot::Receiver<()>
) {
    let _ = async move {
        let listener = tokio::net::TcpListener::bind(listen).await?;
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((mut stream, _)) => {
                            dbg!("accepted");
                            stream.writable().await?;

                            /* write_all() is not cancel-safe, but we don't care, it's the user who
                            * cancelled */
                            stream.write_all(&token).await?;
                        }
                        Err(e) => {
                            log::error!("accept(): {e}");
                            break;
                        }
                    }
                }
                _ = &mut closed => {
                    break;
                }
            }
        }
        Ok::<_, Error>(())
    }
    .map_err(|e| log::error!("{e}"))
    .await;
}
