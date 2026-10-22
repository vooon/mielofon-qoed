//! Endpoint list handling: `host:port` entries, names or literal addresses,
//! each resolved to every address it stands for. A name that answers with
//! all hubs and a list of literal addresses are the same thing to the caller.

use std::net::{IpAddr, SocketAddr};

/// One configured endpoint: a DNS name or a literal address, plus port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    /// The TLS server name presented for this endpoint: the DNS name, or
    /// the literal address (rustls verifies IP SANs).
    pub fn server_name(&self) -> &str {
        &self.host
    }
}

/// Parse `host:port`, `[v6]:port`, `v4:port` or `name:port`. A bare name or
/// address takes `default_port`.
pub fn parse_endpoint(entry: &str, default_port: u16) -> Result<Endpoint, String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err("empty endpoint".into());
    }
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("{entry}: unterminated IPv6 literal"))?;
        host.parse::<IpAddr>()
            .map_err(|_| format!("{entry}: bad IPv6 literal"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("{entry}: bad port"))?,
            None if tail.is_empty() => default_port,
            None => return Err(format!("{entry}: junk after IPv6 literal")),
        };
        return Ok(Endpoint {
            host: host.to_string(),
            port,
        });
    }
    // A bare IPv6 literal without brackets has several colons.
    if entry.matches(':').count() > 1 {
        entry
            .parse::<IpAddr>()
            .map_err(|_| format!("{entry}: bad IPv6 literal (use [addr]:port)"))?;
        return Ok(Endpoint {
            host: entry.to_string(),
            port: default_port,
        });
    }
    match entry.rsplit_once(':') {
        Some((host, port)) => Ok(Endpoint {
            host: host.to_string(),
            port: port.parse().map_err(|_| format!("{entry}: bad port"))?,
        }),
        None => Ok(Endpoint {
            host: entry.to_string(),
            port: default_port,
        }),
    }
}

/// Resolve every endpoint to its addresses, in list order, deduplicated.
/// Each address keeps the server name it was reached through. A name that
/// fails to resolve is skipped (logged), never fatal: the next one may work.
pub async fn resolve_all(endpoints: &[Endpoint]) -> Vec<(SocketAddr, String)> {
    let mut out: Vec<(SocketAddr, String)> = Vec::new();
    for ep in endpoints {
        let addrs: Vec<SocketAddr> = match ep.host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, ep.port)],
            Err(_) => match tokio::net::lookup_host((ep.host.as_str(), ep.port)).await {
                Ok(it) => it.collect(),
                Err(e) => {
                    tracing::warn!("resolve {}: {e}", ep.host);
                    Vec::new()
                }
            },
        };
        for a in addrs {
            if !out.iter().any(|(x, _)| *x == a) {
                out.push((a, ep.host.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_literals() {
        let p = |s| parse_endpoint(s, 9552).unwrap();
        assert_eq!(p("hub-a.example.com:9552").host, "hub-a.example.com");
        assert_eq!(p("hub-a.example.com").port, 9552);
        assert_eq!(p("203.0.113.1:1234").port, 1234);
        assert_eq!(p("[2001:db8::1]:1234").host, "2001:db8::1");
        assert_eq!(p("[2001:db8::1]").port, 9552);
        assert_eq!(p("2001:db8::1").host, "2001:db8::1");
        assert!(parse_endpoint("[2001:db8::1", 1).is_err());
        assert!(parse_endpoint("host:port", 1).is_err());
        assert!(parse_endpoint("", 1).is_err());
    }

    #[tokio::test]
    async fn resolves_literals_in_order_without_duplicates() {
        let eps = vec![
            parse_endpoint("203.0.113.1:9552", 0).unwrap(),
            parse_endpoint("[2001:db8::1]:9552", 0).unwrap(),
            parse_endpoint("203.0.113.1:9552", 0).unwrap(),
        ];
        let got = resolve_all(&eps).await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.port(), 9552);
        assert_eq!(got[0].1, "203.0.113.1");
        assert_eq!(got[1].1, "2001:db8::1");
    }
}
