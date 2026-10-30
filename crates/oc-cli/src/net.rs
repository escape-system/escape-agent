//! Listener binding with an incrementing-port fallback.

use std::io;

use tokio::net::TcpListener;

pub(crate) async fn bind_with_fallback(hostname: &str, port: u16) -> io::Result<TcpListener> {
    match TcpListener::bind((hostname, port)).await {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse && port != 0 => {
            for candidate in port.saturating_add(1)..=u16::MAX {
                match TcpListener::bind((hostname, candidate)).await {
                    Ok(listener) => return Ok(listener),
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}
