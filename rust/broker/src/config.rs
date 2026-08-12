//! Broker binary server configuration.
//!
//! The drop-in env-driven config shared with the runtime lives in
//! [`shared::BrokerConfig`]; this module holds the small binary-only knobs
//! (listen address) that don't belong in the shared contract.

#![forbid(unsafe_code)]

use std::net::SocketAddr;

/// Default broker listen address (matches the chart's `containerPort` 8080 and
/// the `servers` URL in the `OpenAPI` spec).
pub const DEFAULT_ADDR: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
    std::net::Ipv4Addr::UNSPECIFIED,
    8080,
));

/// Binary server configuration (listen address only for now).
#[derive(Debug, Clone, Copy)]
pub struct ServerConfig {
    /// Address the broker binds and serves on.
    pub addr: SocketAddr,
}

impl ServerConfig {
    /// Load from the environment. `BROKER_BIND_ADDR` (e.g. `0.0.0.0:8080`)
    /// overrides the default; an unparseable value falls back to the default
    /// (logged), so a bad override never blocks boot.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_raw(std::env::var("BROKER_BIND_ADDR").ok().as_deref())
    }

    /// Pure core: resolve the address from an optional raw value (absent/empty
    /// or unparseable → default).
    pub(crate) fn from_raw(raw: Option<&str>) -> Self {
        let addr = match raw.filter(|v| !v.is_empty()) {
            Some(value) => match value.parse::<SocketAddr>() {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("ignoring malformed BROKER_BIND_ADDR={value:?}: {e}");
                    DEFAULT_ADDR
                }
            },
            None => DEFAULT_ADDR,
        };
        Self { addr }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_8080() {
        assert_eq!(ServerConfig::from_raw(None).addr, DEFAULT_ADDR);
        assert_eq!(ServerConfig::from_raw(Some("")).addr, DEFAULT_ADDR);
        assert_eq!(DEFAULT_ADDR.port(), 8080);
    }

    #[test]
    fn parses_explicit_address() {
        let cfg = ServerConfig::from_raw(Some("127.0.0.1:9090"));
        assert_eq!(cfg.addr.port(), 9090);
    }

    #[test]
    fn malformed_falls_back_to_default() {
        assert_eq!(
            ServerConfig::from_raw(Some("not-an-addr")).addr,
            DEFAULT_ADDR
        );
    }
}
