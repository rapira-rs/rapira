#[cfg(target_os = "linux")]
mod accept_linux;

#[cfg(target_os = "linux")]
pub use accept_linux::{TcpListener, UnixListener};
#[cfg(not(target_os = "linux"))]
pub use tokio::net::{TcpListener, UnixListener};
