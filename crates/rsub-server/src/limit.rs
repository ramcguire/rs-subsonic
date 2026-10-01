//! Failed-login limiting: after [`MAX_FAILURES`] failed attempts from one
//! client address, or against one username, further attempts are refused
//! until [`WINDOW`] has passed since the first of them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

const MAX_FAILURES: u32 = 10;
const WINDOW: Duration = Duration::from_secs(5 * 60);
/// Entries kept at most; past it, expired ones are dropped and new usernames
/// aren't tracked (addresses still are).
const MAX_ENTRIES: usize = 100_000;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Ip(IpAddr),
    User(String),
}

struct Entry {
    first: Instant,
    failures: u32,
}

#[derive(Default)]
pub struct AuthLimiter(Mutex<HashMap<Key, Entry>>);

impl AuthLimiter {
    fn keys(ip: Option<IpAddr>, username: Option<&str>) -> impl Iterator<Item = Key> {
        ip.map(Key::Ip)
            .into_iter()
            .chain(username.map(|u| Key::User(rsub_store::username_key(u))))
    }

    /// Whether attempts from `ip` or for `username` are refused for now.
    pub fn blocked(&self, ip: Option<IpAddr>, username: Option<&str>) -> bool {
        let now = Instant::now();
        let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Self::keys(ip, username).any(|k| {
            m.get(&k)
                .is_some_and(|e| e.failures >= MAX_FAILURES && now - e.first < WINDOW)
        })
    }

    pub fn failed(&self, ip: Option<IpAddr>, username: Option<&str>) {
        let now = Instant::now();
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= MAX_ENTRIES {
            m.retain(|_, e| now - e.first < WINDOW);
        }
        for k in Self::keys(ip, username) {
            if m.len() >= MAX_ENTRIES && matches!(k, Key::User(_)) && !m.contains_key(&k) {
                continue;
            }
            let e = m.entry(k).or_insert(Entry {
                first: now,
                failures: 0,
            });
            if now - e.first >= WINDOW {
                *e = Entry {
                    first: now,
                    failures: 0,
                };
            }
            e.failures += 1;
        }
    }

    /// A successful login clears its username's failures, not its address's.
    pub fn succeeded(&self, username: &str) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.remove(&Key::User(rsub_store::username_key(username)));
    }
}

/// The client's address: the peer, or the last `X-Forwarded-For` hop when the
/// peer is a proxy on this machine or network. `None` without connection info
/// (tests).
pub struct ClientIp(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_canonical());
        let forwarded = || {
            parts
                .headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .next_back()
                .and_then(|ip| ip.trim().parse::<IpAddr>().ok())
        };
        Ok(ClientIp(match peer {
            Some(ip) if is_local(ip) => forwarded().or(Some(ip)),
            other => other,
        }))
    }
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max_failures_per_address_and_username() {
        let l = AuthLimiter::default();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        for _ in 0..MAX_FAILURES - 1 {
            l.failed(Some(ip), Some("Bob"));
        }
        assert!(!l.blocked(Some(ip), Some("bob")));
        l.failed(Some(ip), Some("bob"));
        assert!(l.blocked(Some(ip), None));
        assert!(
            l.blocked(Some(other), Some("BOB")),
            "the username is blocked anywhere"
        );
        assert!(!l.blocked(Some(other), Some("carol")));
        l.succeeded("bob");
        assert!(!l.blocked(Some(other), Some("bob")));
        assert!(
            l.blocked(Some(ip), Some("carol")),
            "the address stays blocked"
        );
    }
}
