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
//!
//! Multi-account: `account_proxies.<ign>` in config.toml overrides the global
//! `proxy_*` fields per ingame name (empty address = forced direct). The
//! override is resolved once at startup for the account this process started
//! for — account switches restart the process, which re-resolves it.

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

/// What [`resolve_settings`] decided and why, for the startup log line.
#[derive(PartialEq, Eq, Debug)]
enum ProxySource {
    /// The `account_proxies.<ign>` entry for the active account.
    PerAccount,
    /// The account has no entry; the global `proxy_*` fields apply.
    GlobalDefault,
    /// A per-account entry exists with an empty address: this account is
    /// explicitly forced DIRECT even though a global proxy may be configured.
    PerAccountOff,
}

/// Effective proxy settings for `active_account`: the `account_proxies.<ign>`
/// override when one exists (empty address = explicitly off), else the global
/// `proxy_*` fields. Unparseable addresses degrade to no-proxy with a warn.
fn resolve_settings(config: &Config, active_account: &str) -> (Option<ProxySettings>, ProxySource) {
    if let Some(entry) = config.account_proxies.get(active_account) {
        let addr = entry.address.as_deref().and_then(parse_proxy_address);
        if entry.address.is_some() && addr.is_none() {
            warn!(
                "account_proxies.{active_account}: address {:?} is not a usable host:port — running WITHOUT a proxy for this account",
                entry.address
            );
        }
        if let Some(addr) = addr {
            return (
                Some(ProxySettings {
                    addr,
                    username: entry.username(),
                    password: entry.password(),
                }),
                ProxySource::PerAccount,
            );
        }
        // Either an explicit empty address (deliberate off) or a garbage
        // address (already warned). Both mean: no proxy for this account.
        return (None, ProxySource::PerAccountOff);
    }

    if config.proxy_enabled {
        match config
            .proxy_address
            .as_deref()
            .and_then(parse_proxy_address)
        {
            Some(addr) => {
                return (
                    Some(ProxySettings {
                        addr,
                        username: config.proxy_username().map(|s| s.to_string()),
                        password: config.proxy_password().map(|s| s.to_string()),
                    }),
                    ProxySource::GlobalDefault,
                )
            }
            None => {
                warn!(
                    "proxy_enabled = true but proxy_address {:?} is not a usable host:port — running WITHOUT a proxy",
                    config.proxy_address
                );
            }
        }
    }
    (None, ProxySource::GlobalDefault)
}

/// Parse the proxy out of the config and store it process-wide.
/// Call ONCE at startup, before any connection is made, and only once the
/// active account is known — `account_proxies.<ign>` overrides the global
/// `proxy_*` fields for that account.
pub fn init(config: &Config, active_account: &str) {
    let (parsed, source) = resolve_settings(config, active_account);
    match &parsed {
        Some(p) => {
            let via = match source {
                ProxySource::PerAccount => "per-account override",
                ProxySource::GlobalDefault => "global default",
                ProxySource::PerAccountOff => unreachable!("a parsed proxy is never PerAccountOff"),
            };
            info!(
                "Proxy: ENABLED — SOCKS5 {} for {} ({}) (Minecraft + session auth + Mojang/Hypixel HTTP)",
                p.addr, active_account, via
            );
        }
        None => {
            if source == ProxySource::PerAccountOff && config.proxy_enabled {
                info!(
                    "Proxy: disabled for {} (per-account override; the global proxy would otherwise apply)",
                    active_account
                );
            }
        }
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

    fn config_from_toml(toml_str: &str) -> Config {
        toml::from_str(toml_str).expect("config should parse")
    }

    #[test]
    fn per_account_entry_overrides_the_global_proxy() {
        let config = config_from_toml(
            r#"
            proxy_enabled = true
            proxy_address = "10.0.0.1:1080"
            proxy_credentials = "globaluser:globalpass"

            [account_proxies.cxwsi]
            address = "51.10.22.33:7791"
            credentials = "user:pass"
        "#,
        );
        let (settings, source) = resolve_settings(&config, "cxwsi");
        assert_eq!(source, ProxySource::PerAccount);
        let s = settings.expect("per-account proxy wins over the global one");
        assert_eq!(s.addr, "51.10.22.33:7791".parse().unwrap());
        assert_eq!(s.username.as_deref(), Some("user"));
        assert_eq!(s.password.as_deref(), Some("pass"));

        // A different account (no entry) still gets the global default.
        let (settings, source) = resolve_settings(&config, "OtherAlt");
        assert_eq!(source, ProxySource::GlobalDefault);
        let s = settings.expect("global proxy applies");
        assert_eq!(s.addr, "10.0.0.1:1080".parse().unwrap());
        assert_eq!(s.username.as_deref(), Some("globaluser"));
        assert_eq!(s.password.as_deref(), Some("globalpass"));
    }

    #[test]
    fn per_account_empty_address_forces_direct_despite_global_proxy() {
        let config = config_from_toml(
            r#"
            proxy_enabled = true
            proxy_address = "10.0.0.1:1080"

            [account_proxies.NoProxyAlt]
            address = ""
        "#,
        );
        let (settings, source) = resolve_settings(&config, "NoProxyAlt");
        assert_eq!(source, ProxySource::PerAccountOff);
        assert!(
            settings.is_none(),
            "explicit off must not fall back to global"
        );
    }

    #[test]
    fn global_fallback_when_disabled_means_no_proxy() {
        let config = config_from_toml(r#"proxy_enabled = false"#);
        let (settings, source) = resolve_settings(&config, "cxwsi");
        assert_eq!(source, ProxySource::GlobalDefault);
        assert!(settings.is_none());
    }

    #[test]
    fn per_account_entry_applies_even_when_global_proxy_disabled() {
        let config = config_from_toml(
            r#"
            proxy_enabled = false

            [account_proxies.cxwsi]
            address = "51.10.22.33:7791"
        "#,
        );
        let (settings, source) = resolve_settings(&config, "cxwsi");
        assert_eq!(source, ProxySource::PerAccount);
        assert!(settings.is_some(), "the override must win over global off");
    }

    #[test]
    fn per_account_garbage_address_degrades_to_direct() {
        let config = config_from_toml(
            r#"
            proxy_enabled = true
            proxy_address = "10.0.0.1:1080"

            [account_proxies.cxwsi]
            address = "not-a-usable-address"
        "#,
        );
        let (settings, source) = resolve_settings(&config, "cxwsi");
        assert_eq!(source, ProxySource::PerAccountOff);
        assert!(
            settings.is_none(),
            "garbage address must not fall back to global"
        );
    }
}
