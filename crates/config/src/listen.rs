use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddr {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListenAddr::Tcp(addr) => write!(f, "{addr}"),
            ListenAddr::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

impl FromStr for ListenAddr {
    type Err = anyhow::Error;

    /// The `:port` check runs before the `SocketAddr` parse: an IPv6 literal contains ':' but never leads with one.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix:") {
            if path.is_empty() {
                anyhow::bail!("unix socket path is empty");
            }
            return Ok(ListenAddr::Unix(PathBuf::from(path)));
        }
        if !s.contains(':') {
            anyhow::bail!("`{s}` is not a listen address: use host:port, :port, or unix:<path>");
        }
        if let Some(port) = s.strip_prefix(':') {
            let port: u16 = port
                .parse()
                .map_err(|_| anyhow::anyhow!("`{s}` has an invalid port"))?;
            return Ok(ListenAddr::Tcp(SocketAddr::from((
                Ipv4Addr::UNSPECIFIED,
                port,
            ))));
        }
        s.parse::<SocketAddr>().map(ListenAddr::Tcp).map_err(|_| {
            anyhow::anyhow!("`{s}` is not host:port (expected an IP literal, e.g. 127.0.0.1:8000)")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_parses_all_forms() {
        assert_eq!(
            "127.0.0.1:8000".parse::<ListenAddr>().unwrap(),
            ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], 8000)))
        );
        assert_eq!(
            ":8080".parse::<ListenAddr>().unwrap(),
            ListenAddr::Tcp(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080)))
        );
        assert!(matches!(
            "[::1]:8000".parse::<ListenAddr>(),
            Ok(ListenAddr::Tcp(_))
        ));
        let l: ListenAddr = "unix:/run/rapira.sock".parse().unwrap();
        assert_eq!(l, ListenAddr::Unix(PathBuf::from("/run/rapira.sock")));
        assert_eq!(l.to_string(), "unix:/run/rapira.sock");
    }

    #[test]
    fn listen_rejects_invalid() {
        for bad in ["8080", "", ":", "unix:", "localhost:8000"] {
            assert!(
                bad.parse::<ListenAddr>().is_err(),
                "`{bad}` should not parse"
            );
        }
    }
}
