//! Per-address limits on anonymous writes. Posting needs no login, as in MeshChat, and
//! every message and file is copied to every site for good, so one client mustn't be
//! able to fill them all.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Instant;

use axum::Extension;
use axum::extract::ConnectInfo;

/// Addresses remembered at once; past this, ones that have refilled are forgotten.
const MAX_TRACKED: usize = 4096;

pub type Peer = Option<Extension<ConnectInfo<SocketAddr>>>;

/// The client's address; requests served without connection info share one bucket.
pub fn addr(peer: &Peer) -> IpAddr {
    peer.as_ref()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |p| p.0.0.ip())
}

pub struct Limiter {
    per_min: f64,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl Limiter {
    /// `per_min` writes a minute, and up to that many in a burst.
    pub fn new(per_min: u32) -> Self {
        Self {
            per_min: per_min as f64,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Spend `cost` from this address's allowance, if it has that much left.
    pub fn take(&self, ip: IpAddr, cost: f64) -> bool {
        self.take_at(ip, cost, Instant::now())
    }

    fn take_at(&self, ip: IpAddr, cost: f64, now: Instant) -> bool {
        let mut b = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let refill = |(left, at): (f64, Instant)| {
            (left + now.duration_since(at).as_secs_f64() * self.per_min / 60.0).min(self.per_min)
        };
        if b.len() >= MAX_TRACKED {
            b.retain(|_, v| refill(*v) < self.per_min);
        }
        let left = b.get(&ip).map_or(self.per_min, |v| refill(*v));
        let ok = left >= cost;
        b.insert(ip, (if ok { left - cost } else { left }, now));
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_burst_is_cut_off_and_refills_with_time() {
        let l = Limiter::new(6);
        let (a, b) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let t = Instant::now();
        assert!((0..6).all(|_| l.take_at(a, 1.0, t)));
        assert!(!l.take_at(a, 1.0, t));
        assert!(l.take_at(b, 1.0, t), "other addresses are not affected");
        assert!(l.take_at(a, 1.0, t + Duration::from_secs(10)));
        assert!(!l.take_at(a, 1.0, t + Duration::from_secs(10)));
    }
}
