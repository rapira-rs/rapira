use anyhow::Context;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

pub(crate) fn resolve_listen(
    value: Option<&str>,
    table: &str,
    default_port: u16,
    config_dir: Option<&Path>,
) -> anyhow::Result<Listen> {
    let listen = match value {
        Some(value) => value
            .parse::<Listen>()
            .with_context(|| format!("invalid {table}.listen `{value}`"))?,
        None => Listen::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, default_port))),
    };
    match listen {
        Listen::Unix(path) => Ok(Listen::Unix(std::path::absolute(
            config_dir.unwrap_or_else(|| Path::new(".")).join(path),
        )?)),
        tcp => Ok(tcp),
    }
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Listen::Tcp(addr) => write!(f, "{addr}"),
            Listen::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

#[derive(Debug)]
pub struct ListenParseError(String);

impl fmt::Display for ListenParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ListenParseError {}

impl FromStr for Listen {
    type Err = ListenParseError;

    /// The `:port` check runs before the `SocketAddr` parse: an IPv6 literal contains ':' but never leads with one.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(ListenParseError("unix socket path is empty".into()));
            }
            return Ok(Listen::Unix(PathBuf::from(path)));
        }
        if !s.contains(':') {
            return Err(ListenParseError(format!(
                "`{s}` is not a listen address: use host:port, :port, or unix:<path>"
            )));
        }
        if let Some(port) = s.strip_prefix(':') {
            let port: u16 = port
                .parse()
                .map_err(|_| ListenParseError(format!("`{s}` has an invalid port")))?;
            return Ok(Listen::Tcp(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))));
        }
        s.parse::<SocketAddr>().map(Listen::Tcp).map_err(|_| {
            ListenParseError(format!(
                "`{s}` is not host:port (expected an IP literal, e.g. 127.0.0.1:8000)"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_parses_all_forms() {
        assert_eq!(
            "127.0.0.1:8000".parse::<Listen>().unwrap(),
            Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 8000)))
        );
        assert_eq!(
            ":8080".parse::<Listen>().unwrap(),
            Listen::Tcp(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8080)))
        );
        assert!(matches!("[::1]:8000".parse::<Listen>(), Ok(Listen::Tcp(_))));
        let l: Listen = "unix:/run/rapira.sock".parse().unwrap();
        assert_eq!(l, Listen::Unix(PathBuf::from("/run/rapira.sock")));
        assert_eq!(l.to_string(), "unix:/run/rapira.sock");
    }

    #[test]
    fn listen_rejects_invalid() {
        for bad in ["8080", "", ":", "unix:", "localhost:8000"] {
            assert!(bad.parse::<Listen>().is_err(), "`{bad}` should not parse");
        }
    }
}
