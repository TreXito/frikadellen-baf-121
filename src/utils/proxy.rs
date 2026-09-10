//! SOCKS5 proxy support, driven by the `proxy_*` config fields.
//!
//! The config fields have existed for a long time but were never consumed:
//! `proxy_enabled = true` logged a line and did nothing, so every connection
//! went out over the default interface. This module is the single place that
//! turns the config into real proxy settings, used by:
//!
//! - the Minecraft connection (game server + Mojang session server) via
//!   azalea's `ClientBuilder::proxy` (native SOCKS5),
//! - the direct Mojang / Hypixel HTTP lookups (auction ownership, panel
//!   auction list) via [`apply_to_client_builder`].
//!
//! Deliberately NOT proxied (latency on the flip path, and none of them need
//! the game account's IP): the COFL/finder flip websocket, Discord webhooks,
//! the GitHub updater and the web panel itself.
//!
//! The proxy is SOCKS5 (that is what azalea speaks). `proxy_address` is
//! `host:port`, credentials are optional `user:pass`.

use std::net::SocketAddr;
use std::sync::OnceLock;

use tracing::{info, warn};

use crate::config::Config;

static PROXY: OnceLock<Option<ProxySettings>> = OnceLock::new();

struct ProxySettings {
    addr: SocketAddr,
    username: Option<String>,
    password: Option<String>,
}

impl ProxySettings {
    /// `socks5h://user:pass@host:port` for reqwest (`socks5h` = the proxy also
    /// resolves DNS, matching proxychains' default behaviour).
    fn reqwest_url(&self) -> String {
        let auth = match (&self.username, &self.password) {
            (Some(u), Some(p)) => format!("{}:{}@", pct_encode(u), pct_encode(p)),
            (Some(u), None) => format!("{}@", pct_encode(u)),
            _ => String::new(),
        };
        format!("socks5h://{}{}", auth, self.addr)
    }
}

/// Parse `host:port` (or an IP:port literal, including IPv6 `[..]:port`) into
/// a `SocketAddr`, resolving a hostname through the system resolver.
fn parse_proxy_address(address: &str) -> Option<SocketAddr> {
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    // IP literals parse directly (covers IPv6 `[::1]:1080` too).
    if let Ok(addr) = address.parse::<SocketAddr>() {
        return Some(addr);
    }
    // Otherwise `host:port` with a hostname that needs resolving.
    let (host, port) = address.rsplit_once(':')?;
    let port: u16 = port.trim().parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    (host, port).to_socket_addrs_iter().next()
}

/// std's `ToSocketAddrs` iterator wrapper (hostname → first resolved address).
trait ToSocketAddrsIter {
    fn to_socket_addrs_iter(self) -> std::option::IntoIter<SocketAddr>;
}

impl ToSocketAddrsIter for (&str, u16) {
    fn to_socket_addrs_iter(self) -> std::option::IntoIter<SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs()
            .ok()
            .and_then(|mut i| i.next())
            .into_iter()
    }
}

/// Percent-encode a userinfo component so special characters survive URL
/// parsing (reqwest parses the proxy string as a URL).
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Parse the proxy out of the config and store it process-wide.
/// Call ONCE at startup, before any connection is made.
pub fn init(config: &Config) {
    let parsed = if config.proxy_enabled {
        match config
            .proxy_address
            .as_deref()
            .and_then(parse_proxy_address)
        {
            Some(addr) => Some(ProxySettings {
                addr,
                username: config.proxy_username().map(|s| s.to_string()),
                password: config.proxy_password().map(|s| s.to_string()),
            }),
            None => {
                warn!(
                    "proxy_enabled = true but proxy_address {:?} is not a usable host:port — running WITHOUT a proxy",
                    config.proxy_address
                );
                None
            }
        }
    } else {
        None
    };
    if let Some(p) = &parsed {
        info!(
            "Proxy: ENABLED — SOCKS5 {} (Minecraft + session auth + Mojang/Hypixel HTTP)",
            p.addr
        );
    }
    let _ = PROXY.set(parsed);
}

/// The azalea `Proxy` for the Minecraft server + Mojang session server
/// connections, or `None` when no proxy is configured.
pub fn azalea_proxy() -> Option<azalea_protocol::connect::Proxy> {
    let settings = PROXY.get()?.as_ref()?;
    let auth = settings.username.as_ref().map(|u| {
        socks5_impl::protocol::UserKey::new(
            u.clone(),
            settings.password.clone().unwrap_or_default(),
        )
    });
    Some(azalea_protocol::connect::Proxy::new(settings.addr, auth))
}

/// Apply the configured proxy to a reqwest client builder. Pass-through when
/// no proxy is configured.
pub fn apply_to_client_builder(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(url) = PROXY
        .get()
        .and_then(|p| p.as_ref())
        .map(|p| p.reqwest_url())
    else {
        return builder;
    };
    match reqwest::Proxy::all(&url) {
        Ok(proxy) => builder.proxy(proxy),
        Err(e) => {
            warn!(
                "Failed to parse proxy URL for HTTP clients: {} — going direct",
                e
            );
            builder
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ip_literal() {
        assert_eq!(
            parse_proxy_address("127.0.0.1:1080"),
            Some("127.0.0.1:1080".parse().unwrap())
        );
        assert_eq!(
            parse_proxy_address("[::1]:9050"),
            Some("[::1]:9050".parse().unwrap())
        );
    }

    #[test]
    fn parses_localhost_name() {
        let addr = parse_proxy_address("localhost:1080").expect("localhost resolves everywhere");
        assert_eq!(addr.port(), 1080);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_proxy_address(""), None);
        assert_eq!(parse_proxy_address("no-port-here"), None);
        assert_eq!(parse_proxy_address("host:notaport"), None);
    }

    #[test]
    fn percent_encodes_userinfo() {
        assert_eq!(pct_encode("user"), "user");
        assert_eq!(pct_encode("p@ss:w0rd"), "p%40ss%3Aw0rd");
    }

    #[test]
    fn reqwest_url_shapes() {
        let s = ProxySettings {
            addr: "1.2.3.4:1080".parse().unwrap(),
            username: None,
            password: None,
        };
        assert_eq!(s.reqwest_url(), "socks5h://1.2.3.4:1080");
        let s = ProxySettings {
            addr: "1.2.3.4:1080".parse().unwrap(),
            username: Some("u".into()),
            password: Some("p".into()),
        };
        assert_eq!(s.reqwest_url(), "socks5h://u:p@1.2.3.4:1080");
    }
}
