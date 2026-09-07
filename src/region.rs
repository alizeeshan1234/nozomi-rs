use std::fmt;

/// A Nozomi region. `Auto` resolves to the nearest region through Cloudflare.
///
/// Marked `#[non_exhaustive]`: Temporal adds regions, and a minor release will
/// add them here. Keep a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Region {
    /// `nozomi.temporal.xyz`, routed to the nearest region via Cloudflare. HTTPS only.
    Auto,
    Pittsburgh,
    Newark,
    Ashburn,
    LosAngeles,
    Frankfurt,
    Amsterdam,
    London,
    Tokyo,
    Singapore,
}

/// How to reach a region.
///
/// `Direct` talks to the region's own host and may use plain HTTP, which the docs
/// recommend for the lowest latency from a datacenter. `Cloudflare` goes through
/// the proxied host over HTTPS and is the better choice from residential or mobile
/// networks. `Region::Auto` is always Cloudflare.
///
/// `Direct { tls: false }` puts the API key on the wire in cleartext, in the
/// query string of every request. Use it from inside a datacenter with a
/// trusted path to the host, not across the open internet.
///
/// TLS on the direct hosts is unreliable. Measured 2026-09-07: ten fresh
/// handshakes to each host's `https://` port succeeded 10/10 on Ashburn and
/// London, 3/10 on Singapore, 2/10 on Frankfurt, 1/10 on Amsterdam, and 0/10
/// on Pittsburgh, Newark, Los Angeles, and Tokyo, with the failures being a
/// reset at the ClientHello; QUIC on the same hosts failed and recovered on
/// the same schedule. Plain `http://` and the Cloudflare route never failed.
/// Keep one of those as a fallback, and run
/// `cargo run --example smoke --all-features` to re-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    Direct { tls: bool },
    Cloudflare,
}

impl Region {
    /// All fixed regions, in the order the docs list them. A slice, not an
    /// array, so adding a region is not a breaking change.
    pub const ALL: &'static [Region] = &[
        Region::Pittsburgh,
        Region::Newark,
        Region::Ashburn,
        Region::LosAngeles,
        Region::Frankfurt,
        Region::Amsterdam,
        Region::London,
        Region::Tokyo,
        Region::Singapore,
    ];

    fn code(self) -> Option<&'static str> {
        Some(match self {
            Region::Auto => return None,
            Region::Pittsburgh => "pit",
            Region::Newark => "ewr",
            Region::Ashburn => "ash",
            Region::LosAngeles => "lax",
            Region::Frankfurt => "fra",
            Region::Amsterdam => "ams",
            Region::London => "lon",
            Region::Tokyo => "tyo",
            Region::Singapore => "sgp",
        })
    }

    /// Frankfurt's direct host is `fra2`; every other region is `<code>1`.
    fn direct_host(self) -> Option<String> {
        let code = self.code()?;
        let n = if self == Region::Frankfurt { 2 } else { 1 };
        Some(format!("{code}{n}.nozomi.temporal.xyz"))
    }

    fn cloudflare_host(self) -> String {
        match self.code() {
            None => "nozomi.temporal.xyz".to_string(),
            Some(code) => format!("{code}.nozomi.temporal.xyz"),
        }
    }

    /// Host for HTTP/3 over QUIC on port 443. Always a direct host; there is
    /// no Cloudflare route for QUIC. `Auto` is `edge.nozomi.temporal.xyz`,
    /// which geo-DNS resolves to the nearest region.
    pub fn quic_host(self) -> String {
        match self.direct_host() {
            Some(h) => h,
            None => "edge.nozomi.temporal.xyz".to_string(),
        }
    }

    /// Base URL (scheme + host, no trailing slash) for this region over `route`.
    ///
    /// `Region::Auto` ignores `route` and always uses the Cloudflare host over HTTPS,
    /// because that is the only way it is served.
    pub fn base_url(self, route: Route) -> String {
        match (self, route) {
            (Region::Auto, _) => "https://nozomi.temporal.xyz".to_string(),
            (r, Route::Cloudflare) => format!("https://{}", r.cloudflare_host()),
            (r, Route::Direct { tls }) => {
                let scheme = if tls { "https" } else { "http" };
                format!(
                    "{scheme}://{}",
                    r.direct_host().expect("fixed region has a direct host")
                )
            }
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Region::Auto => "auto",
            Region::Pittsburgh => "pittsburgh",
            Region::Newark => "newark",
            Region::Ashburn => "ashburn",
            Region::LosAngeles => "los-angeles",
            Region::Frankfurt => "frankfurt",
            Region::Amsterdam => "amsterdam",
            Region::London => "london",
            Region::Tokyo => "tokyo",
            Region::Singapore => "singapore",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_match_docs() {
        assert_eq!(
            Region::Auto.base_url(Route::Direct { tls: false }),
            "https://nozomi.temporal.xyz"
        );
        assert_eq!(
            Region::Pittsburgh.base_url(Route::Direct { tls: false }),
            "http://pit1.nozomi.temporal.xyz"
        );
        assert_eq!(
            Region::Pittsburgh.base_url(Route::Direct { tls: true }),
            "https://pit1.nozomi.temporal.xyz"
        );
        assert_eq!(
            Region::Pittsburgh.base_url(Route::Cloudflare),
            "https://pit.nozomi.temporal.xyz"
        );
        assert_eq!(
            Region::Frankfurt.base_url(Route::Direct { tls: false }),
            "http://fra2.nozomi.temporal.xyz"
        );
        assert_eq!(
            Region::Frankfurt.base_url(Route::Cloudflare),
            "https://fra.nozomi.temporal.xyz"
        );
        assert_eq!(
            Region::Singapore.base_url(Route::Direct { tls: true }),
            "https://sgp1.nozomi.temporal.xyz"
        );
        assert_eq!(Region::Auto.quic_host(), "edge.nozomi.temporal.xyz");
        assert_eq!(Region::Frankfurt.quic_host(), "fra2.nozomi.temporal.xyz");
        assert_eq!(Region::London.quic_host(), "lon1.nozomi.temporal.xyz");
    }
}
