use std::fmt;

/// A Nozomi region. `Auto` resolves to the nearest region through Cloudflare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    Direct { tls: bool },
    Cloudflare,
}

impl Region {
    /// All fixed regions, in the order the docs list them.
    pub const ALL: [Region; 9] = [
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
    }
}
