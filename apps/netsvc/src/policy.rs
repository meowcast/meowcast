//! What a client of the network service may reach.
//!
//! appd passes the host list of the app's signed manifest with each client. The service
//! checks every request and every redirect against it, and refuses addresses that are not
//! public internet addresses whatever the name says: a name an app is allowed to reach must
//! not become a way to the router, the dongle itself or other devices at home.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};

#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with '/'.
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let (tls, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
            (Some(r), _) => (true, r),
            (_, Some(r)) => (false, r),
            _ => return Err("not an http(s) URL".to_string()),
        };
        let (hostport, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| "bad port".to_string())?),
            None => (hostport, if tls { 443 } else { 80 }),
        };
        let host_ok = !host.is_empty() && host.len() <= 253 && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
        if !host_ok || path.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err("bad URL".to_string());
        }
        Ok(Url { tls, host: host.to_ascii_lowercase(), port, path })
    }

    pub fn origin(&self) -> String {
        format!("{}://{}:{}", if self.tls { "https" } else { "http" }, self.host, self.port)
    }

    /// `Host` header value.
    pub fn authority(&self) -> String {
        if self.port == if self.tls { 443 } else { 80 } { self.host.clone() } else { format!("{}:{}", self.host, self.port) }
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    /// `example.com` (that host), `*.example.com` (it and every host below it), `*` (any).
    hosts: Vec<String>,
    /// Loopback and private addresses too (tests and the build host only; appd never asks).
    pub local: bool,
}

impl Policy {
    pub fn new(hosts: &str, local: bool) -> Policy {
        Policy { hosts: hosts.split(',').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect(), local }
    }

    pub fn allows_host(&self, host: &str) -> bool {
        self.hosts.iter().any(|pattern| match pattern.strip_prefix("*.") {
            _ if pattern == "*" => true,
            Some(domain) => host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.')),
            None => host == pattern,
        })
    }

    /// The address to connect to for `url`, or why not.
    pub fn resolve(&self, url: &Url) -> Result<SocketAddr, String> {
        if !self.allows_host(&url.host) {
            return Err(format!("{}: not one of the hosts this app may reach", url.host));
        }
        let addresses = (url.host.as_str(), url.port).to_socket_addrs().map_err(|e| format!("{}: {e}", url.host))?;
        let mut refused = false;
        for address in addresses {
            match address.ip() {
                IpAddr::V4(ip) if self.local || public(ip) => return Ok(address),
                _ => refused = true,
            }
        }
        Err(if refused { format!("{}: not a public internet address", url.host) } else { format!("{}: no address", url.host) })
    }
}

/// A unicast address on the public internet (not this device, the home network, or a
/// reserved range).
pub fn public(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && ip.octets()[2] == 0) // protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = Url::parse("https://RR2---sn-x.googlevideo.com/videoplayback?expire=1&sig=a%3D").unwrap();
        assert_eq!(url, Url { tls: true, host: "rr2---sn-x.googlevideo.com".into(), port: 443, path: "/videoplayback?expire=1&sig=a%3D".into() });
        assert_eq!(Url::parse("http://127.0.0.1:8787").unwrap().path, "/");
        assert_eq!(Url::parse("https://h.example?x=1").unwrap().path, "/?x=1");
        assert_eq!(Url::parse("http://h.example:8080/").unwrap().authority(), "h.example:8080");
        assert!(Url::parse("ftp://h/").is_err());
        assert!(Url::parse("https://h/a b").is_err());
        assert!(Url::parse("https://bad host/").is_err());
        assert!(Url::parse("https://user@host/").is_err());
    }

    #[test]
    fn host_patterns() {
        let policy = Policy::new("youtube.com, *.googlevideo.com", false);
        assert!(policy.allows_host("youtube.com"));
        assert!(!policy.allows_host("www.youtube.com"));
        assert!(policy.allows_host("googlevideo.com"));
        assert!(policy.allows_host("rr2---sn-x.googlevideo.com"));
        assert!(!policy.allows_host("evilgooglevideo.com"));
        assert!(!policy.allows_host("googlevideo.com.evil.example"));
        assert!(Policy::new("*", false).allows_host("anything.example"));
        assert!(!Policy::new("", false).allows_host("anything.example"));
    }

    #[test]
    fn only_public_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.1.1", "100.64.0.1", "0.0.0.0", "224.0.0.1", "255.255.255.255", "240.0.0.1"] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "142.250.74.14", "172.32.0.1", "100.128.0.1"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
        // A literal private address is refused even when every host is allowed.
        let any = Policy::new("*", false);
        assert!(any.resolve(&Url::parse("http://192.168.1.1/").unwrap()).unwrap_err().contains("not a public"));
        assert!(Policy::new("*", true).resolve(&Url::parse("http://127.0.0.1:1/").unwrap()).is_ok());
        assert!(Policy::new("a.example", true).resolve(&Url::parse("http://127.0.0.1:1/").unwrap()).unwrap_err().contains("may reach"));
    }
}
