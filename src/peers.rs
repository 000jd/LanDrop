//! Peer store: mDNS-discovered peers plus persisted manual peers.
//!
//! All mutation happens through an `Arc<PeerStore>`; reads return owned
//! snapshots. Expiry is explicit ([`PeerStore::sweep`]) so callers can emit
//! removal events.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::Serialize;
use tokio::sync::RwLock;

/// How long a peer stays in the list without being re-seen.
pub const PEER_TTL: Duration = Duration::from_secs(60);

/// Address ranks at or above this are degraded (loopback / link-local /
/// unspecified) and never displace a real LAN address.
const DEGRADED_RANK: u8 = 8;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub source: String, // "mdns" | "lan" | "manual"
}

#[derive(Debug)]
struct Entry {
    peer: Peer,
    last_seen: Instant,
    manual: bool,
}

#[derive(Default)]
struct Inner {
    by_id: HashMap<String, Entry>,
    by_addr: HashMap<(String, u16), String>,
}

/// Concurrent-safe peer store with TTL-based expiry.
pub struct PeerStore {
    inner: RwLock<Inner>,
    /// Last TCP-probe result per peer id, filled in by the periodic probe in
    /// `app.rs`. A device that announces itself but never answers is nearly
    /// always blocked by a firewall, and the UI says so instead of leaving the
    /// user staring at a device that silently fails.
    reachable: RwLock<HashMap<String, bool>>,
}

impl Default for PeerStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerStore {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            reachable: RwLock::new(HashMap::new()),
        }
    }

    /// Record whether `peer_id` answered the last readiness probe.
    pub async fn set_reachable(&self, peer_id: &str, ok: bool) {
        self.reachable.write().await.insert(peer_id.to_string(), ok);
    }

    /// Last probe result for `peer_id` (`None` = not probed yet).
    pub async fn is_reachable(&self, peer_id: &str) -> Option<bool> {
        self.reachable.read().await.get(peer_id).copied()
    }

    /// Number of peers currently known (discovered + manual). Used by the
    /// beacon to notice when the LAN settles down or someone joins/leaves.
    pub async fn count(&self) -> usize {
        self.inner.read().await.by_id.len()
    }

    /// Insert or refresh a peer. Re-announcements refresh `last_seen` and
    /// update name / id / address when the device moved or TXT records arrived.
    /// Returns true when this was a new (or visibly changed) peer.
    pub async fn upsert(&self, peer: Peer) -> bool {
        let mut inner = self.inner.write().await;
        let key = (peer.addr.clone(), peer.port);

        if inner.by_id.contains_key(&peer.id) {
            let old_key = {
                let old = inner.by_id.get(&peer.id).expect("contains_key");
                (old.peer.addr.clone(), old.peer.port)
            };
            // Keep the old address only when the new one is degraded
            // (loopback/link-local/unspecified). A real move between LANs or a
            // port change must be honoured, otherwise the peer stays pinned to
            // a dead address forever.
            let keep_addr =
                addr_rank(&peer.addr) >= DEGRADED_RANK && addr_rank(&old_key.0) < DEGRADED_RANK;
            let new_addr = if keep_addr {
                old_key.0.clone()
            } else {
                peer.addr.clone()
            };
            let new_port = if keep_addr { old_key.1 } else { peer.port };
            let new_key = (new_addr.clone(), new_port);

            if old_key != new_key {
                inner.by_addr.remove(&old_key);
            }
            if let Some(other_id) = inner.by_addr.get(&new_key).cloned() {
                if other_id != peer.id {
                    inner.by_id.remove(&other_id);
                }
            }
            let e = inner.by_id.get_mut(&peer.id).expect("contains_key");
            let changed =
                e.peer.name != peer.name || e.peer.addr != new_addr || e.peer.port != new_port;
            e.last_seen = Instant::now();
            e.manual = e.manual || peer.source == "manual";
            e.peer.name = peer.name.clone();
            e.peer.addr = new_addr;
            e.peer.port = new_port;
            let skip_lan_over_mdns = keep_addr && e.peer.source == "mdns" && peer.source == "lan";
            if !peer.source.is_empty() && !skip_lan_over_mdns {
                e.peer.source = peer.source.clone();
            }
            inner.by_addr.insert(new_key, peer.id);
            return changed;
        }

        if let Some(existing_id) = inner.by_addr.get(&key).cloned() {
            if let Some(mut e) = inner.by_id.remove(&existing_id) {
                let changed = e.peer.id != peer.id || e.peer.name != peer.name;
                e.last_seen = Instant::now();
                e.manual = e.manual || peer.source == "manual";
                e.peer = peer.clone();
                inner.by_addr.insert(key, peer.id.clone());
                inner.by_id.insert(peer.id, e);
                return changed;
            }
        }

        inner.by_addr.insert(key, peer.id.clone());
        inner.by_id.insert(
            peer.id.clone(),
            Entry {
                last_seen: Instant::now(),
                manual: peer.source == "manual",
                peer,
            },
        );
        true
    }

    /// Forget a peer by id (used when a manual peer is removed).
    pub async fn remove(&self, id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(e) = inner.by_id.remove(id) {
            inner.by_addr.remove(&(e.peer.addr.clone(), e.peer.port));
        }
    }

    /// Drop peers whose TTL expired; returns them so callers can emit events.
    /// Manual peers never expire.
    pub async fn sweep(&self) -> Vec<Peer> {
        let now = Instant::now();
        let mut inner = self.inner.write().await;
        let mut removed = Vec::new();
        inner.by_id.retain(|_, e| {
            let keep = e.manual || now.duration_since(e.last_seen) < PEER_TTL;
            if !keep {
                removed.push(e.peer.clone());
            }
            keep
        });
        let live: HashSet<String> = inner.by_id.keys().cloned().collect();
        inner.by_addr.retain(|_, id| live.contains(id));
        removed
    }

    /// Snapshot of live peers.
    pub async fn list(&self) -> Vec<Peer> {
        let inner = self.inner.read().await;
        let mut peers: Vec<Peer> = inner.by_id.values().map(|e| e.peer.clone()).collect();
        peers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        peers
    }

    /// Live peers excluding this device (by id, or by our own listen address).
    pub async fn list_others(&self, my_id: &str, my_port: u16, my_ips: &[String]) -> Vec<Peer> {
        self.list()
            .await
            .into_iter()
            .filter(|p| !is_self(p, my_id, my_port, my_ips))
            .collect()
    }

    /// Look up a peer by id, or by `ip:port`/`ip` address.
    pub async fn find(&self, target: &str) -> Option<Peer> {
        let target = target.trim();
        let peers = self.list().await;
        if let Some(p) = peers.iter().find(|p| p.id == target) {
            return Some(p.clone());
        }
        peers
            .iter()
            .find(|p| format!("{}:{}", p.addr, p.port) == target || p.addr == target)
            .cloned()
    }
}

/// True when `peer` is this machine's own LanDrop node.
pub fn is_self(peer: &Peer, my_id: &str, my_port: u16, my_ips: &[String]) -> bool {
    if !my_id.is_empty() && peer.id == my_id {
        return true;
    }
    peer.port == my_port && my_ips.iter().any(|ip| ip == &peer.addr)
}

/// Parse a user-supplied address like `192.168.1.25`, `192.168.1.25:8420` or
/// `[fe80::1]:8420` into a `SocketAddr` with the default port applied when
/// missing.
///
/// Strict: refuses everything that is not a host we could talk to — unspecified,
/// multicast, broadcast, subnet-directed broadcast (`x.y.z.255`, the broadcast of
/// the usual /24) and link-local addresses (cloud metadata). Loopback is refused
/// as well; call [`parse_peer_addr_with`] when the request came from this
/// machine (the local UI, tests, the two-instance dev script).
pub fn parse_peer_addr(input: &str, default_port: u16) -> Result<SocketAddr> {
    parse_peer_addr_with(input, default_port, false)
}

/// Like [`parse_peer_addr`], with `allow_loopback` keeping `127.0.0.0/8` (and
/// `::1`) usable. Only pass `true` for requests that arrived over loopback or
/// from one of this machine's own addresses — a peer on the LAN must never be
/// able to point us at itself or at another local service.
pub fn parse_peer_addr_with(
    input: &str,
    default_port: u16,
    allow_loopback: bool,
) -> Result<SocketAddr> {
    let input = input.trim();
    if input.is_empty() {
        bail!("empty address");
    }
    // Handle `host:port`, with IPv6 only in brackets.
    let s = if let Some(rest) = input.strip_prefix('[') {
        // Bracketed IPv6: append the default port only when none is given.
        if rest.contains("]:") {
            input.to_string()
        } else {
            format!("{input}:{default_port}")
        }
    } else if input.matches(':').count() > 1 {
        bail!("invalid address (IPv6 must use [addr]:port): {input}");
    } else if input.contains(':') {
        input.to_string()
    } else {
        format!("{input}:{default_port}")
    };
    let addr = s.parse::<SocketAddr>().map_err(|e| {
        if looks_like_device_id(input) {
            anyhow::anyhow!(
                "device '{input}' is not on the network right now — pick it again from the list"
            )
        } else {
            anyhow::anyhow!("invalid address '{input}': {e}")
        }
    })?;
    if addr.port() == 0 {
        bail!("invalid address '{input}': port 0");
    }
    match addr.ip() {
        IpAddr::V4(v) => {
            // `0.0.0.0/8` is "this network" (RFC 1122): never a real host.
            if v.octets()[0] == 0 || v.is_unspecified() || v.is_multicast() || v.is_broadcast() {
                bail!("refusing unsafe target address '{input}'");
            }
            // `x.y.z.255` is the broadcast address of a /24 — i.e. of almost
            // every home and office LAN — and never a host we can talk to.
            if v.octets()[3] == 255 {
                bail!("refusing broadcast-form address '{input}'");
            }
            if v.is_link_local() {
                bail!("refusing link-local target address '{input}'");
            }
            if v.is_loopback() && !allow_loopback {
                bail!(
                    "refusing loopback target address '{input}' (only reachable from this machine)"
                );
            }
        }
        IpAddr::V6(v) => {
            if v.is_unspecified() || v.is_multicast() || v.is_unicast_link_local() {
                bail!("refusing unsafe target address '{input}'");
            }
            if v.is_loopback() && !allow_loopback {
                bail!(
                    "refusing loopback target address '{input}' (only reachable from this machine)"
                );
            }
        }
    }
    Ok(addr)
}

/// Device ids are UUIDs (hyphens), not socket addresses.
pub fn looks_like_device_id(input: &str) -> bool {
    uuid::Uuid::parse_str(input.trim()).is_ok()
}

/// Lower is better. Loopback and link-local must lose to a real LAN address;
/// unspecified addresses are worst.
fn addr_rank(addr: &str) -> u8 {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v)) if v.is_unspecified() => 10,
        Ok(std::net::IpAddr::V4(v)) if v.is_loopback() => 9,
        Ok(std::net::IpAddr::V4(v)) if v.is_link_local() => 8,
        Ok(std::net::IpAddr::V4(v)) if v.octets()[0] == 192 && v.octets()[1] == 168 => 0,
        Ok(std::net::IpAddr::V4(v)) if v.octets()[0] == 10 => 1,
        Ok(std::net::IpAddr::V4(v))
            if v.octets()[0] == 172 && (16..=31).contains(&v.octets()[1]) =>
        {
            2
        }
        Ok(std::net::IpAddr::V4(_)) => 3,
        Ok(std::net::IpAddr::V6(v)) if v.is_unspecified() || v.is_loopback() => 10,
        Ok(std::net::IpAddr::V6(_)) => 7,
        Err(_) => 6,
    }
}

/// Store wrapper so tests can build one easily.
pub fn new_store() -> Arc<PeerStore> {
    Arc::new(PeerStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, addr: &str, port: u16, source: &str) -> Peer {
        Peer {
            id: id.into(),
            name: id.into(),
            addr: addr.into(),
            port,
            source: source.into(),
        }
    }

    #[tokio::test]
    async fn upsert_refreshes_and_list_sorts() {
        let s = new_store();
        assert!(s.upsert(peer("a", "10.0.0.1", 8420, "mdns")).await);
        assert!(s.upsert(peer("b", "10.0.0.2", 8420, "mdns")).await);
        // Same address, new id: adopt the latest id (TXT uuid replacing fullname).
        assert!(s.upsert(peer("a2", "10.0.0.1", 8420, "mdns")).await);
        let list = s.list().await;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "a2");
        assert_eq!(list[1].id, "b");
    }

    #[tokio::test]
    async fn peers_expire_after_ttl() {
        let s = new_store();
        s.upsert(peer("a", "10.0.0.1", 8420, "mdns")).await;
        s.upsert(peer("m", "10.0.0.9", 8420, "manual")).await;
        // Fast-forward time: rewrite last_seen in the past for mdns peer only.
        {
            let mut inner = s.inner.write().await;
            let e = inner.by_id.get_mut("a").unwrap();
            e.last_seen = Instant::now() - PEER_TTL - Duration::from_secs(1);
        }
        let removed = s.sweep().await;
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "a");
        let list = s.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "m"); // manual peers never expire
    }

    #[tokio::test]
    async fn peer_move_updates_addr_and_port() {
        let s = new_store();
        s.upsert(peer("a", "192.168.1.20", 8420, "mdns")).await;
        // The device moved to a different network and port.
        assert!(s.upsert(peer("a", "10.0.0.7", 9000, "mdns")).await);
        let p = s.find("a").await.unwrap();
        assert_eq!(p.addr, "10.0.0.7");
        assert_eq!(p.port, 9000);
    }

    #[tokio::test]
    async fn find_by_id_or_addr() {
        let s = new_store();
        s.upsert(peer("a", "10.0.0.1", 8420, "mdns")).await;
        assert_eq!(s.find("a").await.unwrap().id, "a");
        assert_eq!(s.find("10.0.0.1:8420").await.unwrap().id, "a");
        assert_eq!(s.find("10.0.0.1").await.unwrap().id, "a");
        assert!(s.find("nope").await.is_none());
    }

    #[tokio::test]
    async fn list_others_drops_self() {
        let s = new_store();
        s.upsert(peer("me", "10.0.0.1", 8420, "mdns")).await;
        s.upsert(peer("you", "10.0.0.2", 8420, "mdns")).await;
        let others = s.list_others("me", 8420, &["10.0.0.1".into()]).await;
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].id, "you");
    }

    #[tokio::test]
    async fn lan_addr_not_replaced_by_loopback() {
        let s = new_store();
        s.upsert(peer("a", "192.168.1.20", 8420, "mdns")).await;
        assert!(!s.upsert(peer("a", "127.0.0.1", 8420, "lan")).await);
        let p = s.find("a").await.unwrap();
        assert_eq!(p.addr, "192.168.1.20");
        assert_eq!(p.source, "mdns");
    }

    #[test]
    fn parse_peer_addr_defaults_port() {
        let a = parse_peer_addr("192.168.1.25", 8420).unwrap();
        assert_eq!(a, "192.168.1.25:8420".parse().unwrap());
        let b = parse_peer_addr("192.168.1.25:9000", 8420).unwrap();
        assert_eq!(b, "192.168.1.25:9000".parse().unwrap());
        assert!(parse_peer_addr("", 8420).is_err());
        assert!(parse_peer_addr("not an ip", 8420).is_err());
        let err = parse_peer_addr("056daaae-d1e3-43ec-85fd-07bb48c20afa", 8420)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not on the network"), "{err}");
    }

    #[test]
    fn parse_peer_addr_ipv6() {
        // Bracketed IPv6 parses; loopback needs the explicit opt-in.
        let a = parse_peer_addr_with("[::1]:8420", 9999, true).unwrap();
        assert_eq!(a.port(), 8420);
        let b = parse_peer_addr_with("[::1]", 8420, true).unwrap();
        assert_eq!(b.port(), 8420);
        assert!(parse_peer_addr("[::1]:8420", 8420).is_err());
        assert!(parse_peer_addr("fe80::1", 8420).is_err());
        assert!(parse_peer_addr("2001:db8::1", 8420).is_err());
        // Link-local IPv6 is refused even when bracketed.
        assert!(parse_peer_addr("[fe80::1]:8420", 8420).is_err());
    }

    #[tokio::test]
    async fn reachability_is_tracked_per_peer() {
        let store = PeerStore::new();
        assert_eq!(store.is_reachable("p1").await, None, "not probed yet");
        store.set_reachable("p1", true).await;
        store.set_reachable("p2", false).await;
        assert_eq!(store.is_reachable("p1").await, Some(true));
        assert_eq!(store.is_reachable("p2").await, Some(false));
        store.set_reachable("p1", false).await;
        assert_eq!(store.is_reachable("p1").await, Some(false));
    }

    #[test]
    fn parse_peer_addr_refuses_unsafe_targets() {
        assert!(parse_peer_addr("0.0.0.0:8420", 8420).is_err());
        // The whole "this network" range, not just 0.0.0.0.
        assert!(parse_peer_addr("0.1.2.3:8420", 8420).is_err());
        assert!(parse_peer_addr("169.254.169.254:80", 8420).is_err());
        assert!(parse_peer_addr("224.0.0.1:8420", 8420).is_err());
        assert!(parse_peer_addr("255.255.255.255:8420", 8420).is_err());
        // Subnet-directed broadcast: the broadcast of a /24.
        assert!(parse_peer_addr("192.168.1.255:8420", 8420).is_err());
        // Loopback is refused unless the caller says the request is local.
        assert!(parse_peer_addr("127.0.0.1:8420", 8420).is_err());
        assert!(parse_peer_addr_with("127.0.0.1:8420", 8420, true).is_ok());
        assert!(parse_peer_addr("192.168.1.5", 8420).is_ok());
    }
}
