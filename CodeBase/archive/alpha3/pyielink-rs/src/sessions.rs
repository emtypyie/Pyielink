use crate::token;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SESSION_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
#[allow(dead_code)] // user/peer/created are consumed by the Phase 2 data layer
pub struct Session {
    pub user: String,
    pub peer: IpAddr,
    pub created: Instant,
    /// refreshed on every heartbeat round-trip
    pub last_seen: Instant,
}

struct Registry {
    map: HashMap<String, Session>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGISTRY.lock().unwrap();
    if guard.is_none() {
        *guard = Some(Registry { map: HashMap::new() });
    }
    f(guard.as_mut().unwrap())
}

/// Issue a fresh single-session promotion ticket.
pub fn open_session(user: &str, peer: IpAddr) -> String {
    let key = token::generate();
    with_registry(|r| {
        r.map.insert(
            key.clone(),
            Session {
                user: user.to_string(),
                peer,
                created: Instant::now(),
                last_seen: Instant::now(),
            },
        );
    });
    sweep();
    key
}

/// Look up a session key (does not consume it; heartbeat keeps it alive).
#[allow(dead_code)] // Phase 2 data-layer validation entry point
pub fn get(key: &str) -> Option<Session> {
    with_registry(|r| r.map.get(key).cloned())
}

/// Refresh liveness for a session; returns false if unknown/expired.
pub fn touch(key: &str) -> bool {
    with_registry(|r| match r.map.get_mut(key) {
        Some(s) => {
            s.last_seen = Instant::now();
            true
        }
        None => false,
    })
}

pub fn close(key: &str) {
    with_registry(|r| {
        r.map.remove(key);
    });
}

fn sweep() {
    with_registry(|r| {
        let cutoff = Instant::now() - SESSION_TTL;
        r.map.retain(|_, s| s.last_seen > cutoff);
    });
}

#[allow(dead_code)] // surfaced via status reporting in Phase 2
pub fn active_count() -> usize {
    with_registry(|r| r.map.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn fake_peer() -> IpAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 1)).ip()
    }

    #[test]
    fn session_lifecycle() {
        let key = open_session("bob", fake_peer());
        assert_eq!(get(&key).unwrap().user, "bob");
        assert!(touch(&key));
        close(&key);
        assert!(get(&key).is_none());
        assert!(!touch(&key));
    }

    #[test]
    fn keys_are_unique_and_wellformed() {
        let a = open_session("a", fake_peer());
        let b = open_session("b", fake_peer());
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }
}
