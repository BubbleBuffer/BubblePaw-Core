//! What a forwarding device will connect to: public DNS names on port 443,
//! and only names the requesting side declared. Nothing the caller sends can
//! relax these checks; they are the line between a model-traffic forwarder and
//! an open proxy.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The only port a forwarder connects to.
pub const EGRESS_PORT: u16 = 443;
/// Most hostnames a caller may declare.
pub const MAX_DECLARED: usize = 16;

/// Why a connection was refused, in words the other side may be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub reason: String,
}

impl Refusal {
    pub fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }

    fn not_allowed(reason: impl Into<String>) -> Self {
        Self::new("not_allowed", reason)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Name suffixes that never name a public server.
const PRIVATE_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".internal",
    ".lan",
    ".home.arpa",
    ".localdomain",
    ".intranet",
    ".corp",
    ".private",
];

/// A hostname the forwarder may connect to, lowercased: ASCII letters, digits
/// and hyphens in dot-separated labels, at least two labels, a last label that
/// is not all digits (so no dotted-number address in any notation), and not a
/// local or private name. IP literals, `localhost` and single labels fail.
pub fn check_host(name: &str) -> Result<String, Refusal> {
    let host = name.to_ascii_lowercase();
    let bad = |why: &str| Refusal::not_allowed(format!("\"{name}\" is not an allowed host: {why}"));
    if host.is_empty() || host.len() > 253 {
        return Err(bad("the name has the wrong length"));
    }
    if host.parse::<IpAddr>().is_ok() || host.contains([':', '[', ']']) {
        return Err(bad("IP addresses are not allowed, only names"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(bad("a public name has at least two parts"));
    }
    for label in &labels {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            return Err(bad("it is not a valid DNS name"));
        }
    }
    let last = labels[labels.len() - 1];
    if last.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad("IP addresses are not allowed, only names"));
    }
    let dotted = format!(".{host}");
    if PRIVATE_SUFFIXES
        .iter()
        .any(|suffix| dotted.ends_with(suffix))
    {
        return Err(bad("local and private names are not allowed"));
    }
    Ok(host)
}

/// Validate a declared list: each name allowed, none repeated, a sane count.
pub fn check_declared(declared: &[String]) -> Result<Vec<String>, Refusal> {
    if declared.is_empty() || declared.len() > MAX_DECLARED {
        return Err(Refusal::not_allowed(
            "the request declares no hosts, or too many",
        ));
    }
    let mut hosts = Vec::new();
    for name in declared {
        let host = check_host(name)?;
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

/// Validate a request: the target must be an allowed name that the caller
/// declared. Returns the lowercased target.
pub fn check_request(target: &str, declared: &[String]) -> Result<String, Refusal> {
    let hosts = check_declared(declared)?;
    let target = check_host(target)?;
    if !hosts.contains(&target) {
        return Err(Refusal::not_allowed(format!(
            "\"{target}\" is not one of the hosts the request declared"
        )));
    }
    Ok(target)
}

/// Whether an address is a public, globally routable one. Loopback, private,
/// link-local, shared (carrier-grade NAT, which includes Tailscale), unique
/// local, documentation, benchmark, multicast and reserved ranges are not.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let segments = ip.segments();
    let embedded = |high: u16, low: u16| {
        public_v4(Ipv4Addr::new(
            (high >> 8) as u8,
            high as u8,
            (low >> 8) as u8,
            low as u8,
        ))
    };
    // NAT64 and 6to4 carry an IPv4 address that must itself be public.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return embedded(segments[6], segments[7]);
    }
    if segments[0] == 0x2002 {
        return embedded(segments[1], segments[2]);
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x2001 && segments[1] == 0)
        || segments[0] == 0x0100)
}

/// Keep the resolved addresses a forwarder may connect to: all of them must be
/// public, so a name that points inside the forwarder's network is refused
/// outright instead of being filtered down to whatever else it points at.
pub fn check_resolved(host: &str, addresses: &[SocketAddr]) -> Result<Vec<SocketAddr>, Refusal> {
    if addresses.is_empty() {
        return Err(Refusal::new(
            "unreachable",
            format!("\"{host}\" did not resolve to any address"),
        ));
    }
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(Refusal::not_allowed(format!(
            "\"{host}\" resolves to a non-public address"
        )));
    }
    Ok(addresses.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_names_pass_and_are_lowercased() {
        assert_eq!(check_host("API.Example.com").unwrap(), "api.example.com");
        assert_eq!(
            check_host("xn--e1afmkfd.example").unwrap(),
            "xn--e1afmkfd.example"
        );
        assert_eq!(check_host("a-b.c9.io").unwrap(), "a-b.c9.io");
    }

    #[test]
    fn ip_literals_localhost_and_private_names_are_refused() {
        for name in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "8.8.8.8",
            "::1",
            "[::1]",
            "2001:db8::1",
            "0x7f.1",
            "2130706433",
            "1.2.3.4.",
            "localhost",
            "foo.localhost",
            "printer.local",
            "nas.lan",
            "svc.internal",
            "router.home.arpa",
            "plainname",
            "",
            "-a.example.com",
            "a_b.example.com",
            "exa mple.com",
            "a..example.com",
            "example.com:443",
            "user@example.com",
            "ex\u{e4}mple.com",
        ] {
            assert!(check_host(name).is_err(), "{name:?} must be refused");
        }
        assert!(check_host(&format!("{}.com", "a".repeat(64))).is_err());
        assert!(check_host(&format!("{}example.com", "a.".repeat(130))).is_err());
    }

    #[test]
    fn only_declared_names_are_allowed() {
        let declared = vec!["api.example.com".to_owned()];
        assert_eq!(
            check_request("API.example.com", &declared).unwrap(),
            "api.example.com"
        );
        assert!(check_request("other.example.com", &declared).is_err());
        assert!(check_request("api.example.com", &[]).is_err());
        assert!(check_request("api.example.com", &["127.0.0.1".to_owned()]).is_err());
        let many: Vec<String> = (0..=MAX_DECLARED)
            .map(|n| format!("h{n}.example.com"))
            .collect();
        assert!(check_declared(&many).is_err());
    }

    #[test]
    fn non_public_addresses_are_refused() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.5",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.65.1.103",
            "100.127.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_public_ip(ip), "{ip} must not be public");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_public_ip(ip), "{ip} must be public");
        }
    }

    #[test]
    fn a_name_that_points_inside_is_refused_whole() {
        let public: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let private: SocketAddr = "10.0.0.5:443".parse().unwrap();
        assert!(check_resolved("a.example.com", &[public]).is_ok());
        assert_eq!(
            check_resolved("a.example.com", &[public, private])
                .unwrap_err()
                .code,
            "not_allowed"
        );
        assert_eq!(
            check_resolved("a.example.com", &[]).unwrap_err().code,
            "unreachable"
        );
    }
}
