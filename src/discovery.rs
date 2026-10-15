//! LAN discovery of LanDrop peers.
//!
//! Two independent channels:
//! - mDNS/DNS-SD (`_landrop._tcp`) with a **per-device** hostname and instance
//!   name (a shared `landrop.local.` hostname makes every node collide).
//! - UDP broadcast + multicast beacon on [`DISCOVERY_PORT`], so devices still
//!   find each other when mDNS is blocked (common on guest Wi-Fi, some
//!   routers, and mixed OS setups).
//!
//! Manual peers are handled by [`crate::peers`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::events::EventBus;
use crate::peers::{Peer, PeerStore};
use crate::protocol::PROTOCOL_VERSION;

/// Service type advertised and browsed on the LAN.
pub const SERVICE_TYPE: &str = "_landrop._tcp.local.";

/// Well-known UDP port for LanDrop beacons (independent of the HTTP listen port).
pub const DISCOVERY_PORT: u16 = 8420;

/// Site-local multicast group used in addition to 255.255.255.255.
const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 21);

/// Beacon magic so we ignore unrelated UDP on the port.
const BEACON_MAGIC: &[u8] = b"LD1";

/// How often we shout our identity on the LAN.
const BEACON_INTERVAL: Duration = Duration::from_secs(2);

/// Full-address length limit for mdns-sd (name + addresses must fit a packet).
const MAX_MDNS_ADDR: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Beacon {
    v: u32,
    id: String,
    name: String,
    port: u16,
}

/// Run mDNS and UDP discovery until `shutdown` fires.
pub async fn run(
    store: Arc<PeerStore>,
    my_id: Arc<RwLock<String>>,
    my_name: Arc<RwLock<String>>,
    port: Arc<RwLock<u16>>,
    event_bus: EventBus,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mdns = run_mdns(
        store.clone(),
        my_id.clone(),
        my_name.clone(),
        port.clone(),
        event_bus.clone(),
        shutdown.clone(),
    );
    let udp = run_udp_beacon(store, my_id, my_name, port, event_bus, shutdown);
    tokio::join!(mdns, udp);
}

async fn run_mdns(
    store: Arc<PeerStore>,
    my_id: Arc<RwLock<String>>,
    my_name: Arc<RwLock<String>>,
    port: Arc<RwLock<u16>>,
    event_bus: EventBus,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS unavailable ({e}); UDP beacon still running");
            let _ = shutdown.changed().await;
            return;
        }
    };

    let mut last_fullname: Option<String> = None;
    loop {
        let id = my_id.read().await.clone();
        let name = my_name.read().await.clone();
        let http_port = *port.read().await;
        let instance = mdns_instance_name(&name, &id);
        let fullname = format!("{instance}.{SERVICE_TYPE}");

        if last_fullname.as_deref() != Some(fullname.as_str()) {
            if let Some(prev) = last_fullname.take() {
                let _ = daemon.unregister(&prev);
            }
        }

        if let Err(e) = advertise(&daemon, &id, &name, &instance, http_port) {
            warn!("mDNS advertise failed: {e}");
        } else {
            last_fullname = Some(fullname);
            info!("advertising as {name} ({id}) on port {http_port}");
        }

        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                warn!("mDNS browse failed: {e}");
                let mut shutdown = shutdown.clone();
                tokio::select! {
                    _ = shutdown.changed() => { let _ = daemon.shutdown(); return; }
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                continue;
            }
        };

        loop {
            let mut shutdown = shutdown.clone();
            tokio::select! {
                event = receiver.recv_async() => {
                    match event {
                        Ok(ServiceEvent::ServiceResolved(info)) => {
                            let Some(peer) = parse_peer(&info) else { continue; };
                            if peer.id == *my_id.read().await {
                                continue;
                            }
                            let changed = store.upsert(peer.clone()).await;
                            if changed {
                                event_bus.send("peer_updated", &peer);
                                info!("discovered {} at {}:{}", peer.name, peer.addr, peer.port);
                            } else {
                                debug!("refreshed peer {} at {}", peer.name, peer.addr);
                            }
                        }
                        Ok(ServiceEvent::ServiceRemoved(_, _)) => {}
                        Ok(ServiceEvent::SearchStopped(_)) => {
                            debug!("mDNS browse stopped; restarting");
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                _ = shutdown.changed() => {
                    info!("mDNS discovery shutting down");
                    let _ = daemon.shutdown();
                    return;
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn advertise(
    daemon: &ServiceDaemon,
    id: &str,
    name: &str,
    instance: &str,
    port: u16,
) -> anyhow::Result<()> {
    let txt: Vec<mdns_sd::TxtProperty> = vec![
        ("id", id.to_string()).into(),
        ("name", name.to_string()).into(),
        ("port", port.to_string()).into(),
        ("ver", PROTOCOL_VERSION.to_string()).into(),
    ];
    let hostname = mdns_hostname(id);
    let addrs = advertise_addrs();

    // Trim the address list so it fits in a single mDNS packet. Never publish
    // a loopback A record: other nodes would store it and send to themselves.
    let mut kept: Vec<IpAddr> = Vec::new();
    let mut len = 0usize;
    for addr in addrs {
        let l = addr.to_string().len();
        if !kept.is_empty() && len + l > MAX_MDNS_ADDR {
            break;
        }
        len += l;
        kept.push(addr);
    }
    if kept.is_empty() {
        anyhow::bail!("no usable LAN addresses to advertise");
    }

    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        &hostname,
        kept.as_slice(),
        port,
        txt,
    )?;
    daemon.register(info)?;
    Ok(())
}

/// Extract a [`Peer`] from a resolved mDNS service.
fn parse_peer(info: &mdns_sd::ServiceInfo) -> Option<Peer> {
    let addr = pick_best_ip(info.get_addresses().iter().copied())?;
    let port = info.get_port();
    if port == 0 {
        return None;
    }
    let txt = info.get_properties();
    let get = |k: &str| txt.get(k).map(|v| v.val_str().to_string());

    // Refuse peers speaking a different protocol version (missing = legacy).
    if let Some(v) = get("ver") {
        if v.trim().parse::<u32>().ok() != Some(PROTOCOL_VERSION) {
            debug!(
                "skipping {} (protocol version {v}, ours {PROTOCOL_VERSION})",
                info.get_fullname()
            );
            return None;
        }
    }

    let id = get("id").unwrap_or_else(|| info.get_fullname().to_string());
    let name = get("name").unwrap_or_else(|| {
        info.get_fullname()
            .split('.')
            .next()
            .unwrap_or("device")
            .to_string()
    });
    Some(Peer {
        id,
        name,
        addr: addr.to_string(),
        port,
        source: "mdns".to_string(),
    })
}

/// Unique DNS-SD instance label: `{sanitized-name}-{id-prefix}` (max 63 chars).
pub fn mdns_instance_name(name: &str, id: &str) -> String {
    let mut label: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while label.contains("--") {
        label = label.replace("--", "-");
    }
    let label = label.trim_matches('-');
    let label = if label.is_empty() { "landrop" } else { label };
    let short: String = id.chars().filter(|c| *c != '-').take(8).collect();
    if short.is_empty() {
        return label.chars().take(63).collect();
    }
    // Truncate the label so the unique id suffix survives the 63-char limit.
    let keep = 63usize.saturating_sub(short.len() + 1);
    let mut inst: String = label.chars().take(keep).collect();
    if inst.is_empty() {
        inst = "landrop".to_string();
    }
    inst.push('-');
    inst.push_str(&short);
    inst
}

/// Unique mDNS hostname so nodes do not overwrite each other's A records.
pub fn mdns_hostname(id: &str) -> String {
    let short: String = id.chars().filter(|c| *c != '-').take(12).collect();
    let short = if short.is_empty() {
        "device".to_string()
    } else {
        short
    };
    format!("landrop-{short}.local.")
}

fn advertise_addrs() -> Vec<IpAddr> {
    usable_ipv4_addrs()
}

fn pick_best_ip(addrs: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    let mut v: Vec<IpAddr> = addrs.collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by_key(|a| ip_pref(*a));
    v.into_iter().next()
}

fn ip_pref(a: IpAddr) -> u8 {
    match a {
        IpAddr::V4(v) if v.is_loopback() => 9,
        IpAddr::V4(v) if v.is_link_local() => 8,
        IpAddr::V4(v) if v.octets()[0] == 192 && v.octets()[1] == 168 => 0,
        IpAddr::V4(v) if v.octets()[0] == 10 => 1,
        IpAddr::V4(v) if v.octets()[0] == 172 && (16..=31).contains(&v.octets()[1]) => 2,
        IpAddr::V4(_) => 3,
        IpAddr::V6(v) if v.is_loopback() => 10,
        IpAddr::V6(_) => 7,
    }
}

/// Non-loopback IPv4 addresses on real (non-virtual) interfaces.
pub fn local_ipv4_addrs() -> Vec<IpAddr> {
    usable_ipv4_addrs()
}

fn usable_ipv4_addrs() -> Vec<IpAddr> {
    let Ok(ifaces) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    let mut out: Vec<IpAddr> = ifaces
        .filter_map(|iface| {
            if skip_iface(&iface.interface_name) {
                return None;
            }
            let addr = iface.address?;
            match addr.as_sockaddr_in() {
                Some(sa) => {
                    let ip = sa.ip();
                    if ip.is_loopback() || ip.is_link_local() {
                        None
                    } else {
                        Some(IpAddr::V4(ip))
                    }
                }
                None => None,
            }
        })
        .collect();
    out.sort_by_key(|a| ip_pref(*a));
    out.dedup();
    out
}

/// Virtual / container interfaces we should not advertise on.
pub fn skip_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "lo"
        || n.starts_with("lo:")
        || n.starts_with("docker")
        || n.starts_with("veth")
        || n.starts_with("br-")
        || n.starts_with("virbr")
        || n.starts_with("cni")
        || n.starts_with("flannel")
        || n.starts_with("tun")
        || n.starts_with("tap")
        || n.starts_with("wg")
        || n.starts_with("zt")
        || n.contains("vbox")
        || n.starts_with("vmnet")
        || n.starts_with("lxc")
        || n.starts_with("podman")
}

async fn run_udp_beacon(
    store: Arc<PeerStore>,
    my_id: Arc<RwLock<String>>,
    my_name: Arc<RwLock<String>>,
    port: Arc<RwLock<u16>>,
    event_bus: EventBus,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let sock = match bind_discovery_socket().await {
        Ok(s) => s,
        Err(e) => {
            warn!("UDP discovery unavailable ({e}); mDNS only");
            let _ = shutdown.changed().await;
            return;
        }
    };
    if let Err(e) = sock.join_multicast_v4(MULTICAST_GROUP, Ipv4Addr::UNSPECIFIED) {
        debug!("multicast join failed (broadcast still works): {e}");
    }
    for ip in usable_ipv4_addrs() {
        if let IpAddr::V4(v) = ip {
            let _ = sock.join_multicast_v4(MULTICAST_GROUP, v);
        }
    }
    info!("UDP discovery listening on :{DISCOVERY_PORT}");

    let mut buf = [0u8; 1500];
    let mut tick = tokio::time::interval(BEACON_INTERVAL);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tick.tick() => {
                let id = my_id.read().await.clone();
                let name = my_name.read().await.clone();
                let http_port = *port.read().await;
                if id.is_empty() || http_port == 0 {
                    continue;
                }
                let payload = encode_beacon(&Beacon {
                    v: PROTOCOL_VERSION,
                    id,
                    name,
                    port: http_port,
                });
                for dest in beacon_destinations() {
                    let _ = sock.send_to(&payload, dest).await;
                }
            }
            recv = sock.recv_from(&mut buf) => {
                match recv {
                    Err(e) => {
                        warn!("UDP discovery recv error: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    Ok((n, from)) => {
                let Some(beacon) = decode_beacon(&buf[..n]) else { continue; };
                if beacon.v != PROTOCOL_VERSION {
                    continue;
                }
                if beacon.id.is_empty() || beacon.name.trim().is_empty() {
                    continue;
                }
                if beacon.id == *my_id.read().await {
                    continue;
                }
                if beacon.port == 0 {
                    continue;
                }
                let ip = match from.ip() {
                    IpAddr::V4(v) if !v.is_unspecified() => v.to_string(),
                    other => other.to_string(),
                };
                let peer = Peer {
                    id: beacon.id,
                    name: beacon.name,
                    addr: ip,
                    port: beacon.port,
                    source: "lan".into(),
                };
                let changed = store.upsert(peer.clone()).await;
                if changed {
                    event_bus.send("peer_updated", &peer);
                    info!("discovered {} at {}:{}", peer.name, peer.addr, peer.port);
                }
                    }
                }
            }
        }
    }
}

fn encode_beacon(b: &Beacon) -> Vec<u8> {
    let mut out = BEACON_MAGIC.to_vec();
    if let Ok(js) = serde_json::to_vec(b) {
        out.extend_from_slice(&js);
    }
    out
}

fn decode_beacon(bytes: &[u8]) -> Option<Beacon> {
    if bytes.len() < BEACON_MAGIC.len() + 2 {
        return None;
    }
    if !bytes.starts_with(BEACON_MAGIC) {
        return None;
    }
    serde_json::from_slice(&bytes[BEACON_MAGIC.len()..]).ok()
}

fn beacon_destinations() -> Vec<SocketAddr> {
    let mut dests = vec![
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
        SocketAddr::V4(SocketAddrV4::new(MULTICAST_GROUP, DISCOVERY_PORT)),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, DISCOVERY_PORT)),
    ];
    for bcast in subnet_broadcasts() {
        dests.push(SocketAddr::V4(SocketAddrV4::new(bcast, DISCOVERY_PORT)));
    }
    dests.sort();
    dests.dedup();
    dests
}

fn subnet_broadcasts() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    ifaces
        .filter_map(|iface| {
            if skip_iface(&iface.interface_name) {
                return None;
            }
            let ip = iface.address?.as_sockaddr_in()?.ip();
            if ip.is_loopback() || ip.is_link_local() {
                return None;
            }
            iface
                .broadcast
                .and_then(|b| b.as_sockaddr_in().map(|s| s.ip()))
                .filter(|b| !b.is_unspecified() && !b.is_loopback())
        })
        .collect()
}

async fn bind_discovery_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        let _ = socket.set_reuse_port(true);
    }
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT));
    socket.bind(&addr.into())?;
    let std_sock: std::net::UdpSocket = socket.into();
    std_sock.set_nonblocking(true)?;
    UdpSocket::from_std(std_sock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdns_sd::ServiceInfo;

    fn sample_info() -> ServiceInfo {
        let txt: Vec<mdns_sd::TxtProperty> = vec![
            ("id", "abc".to_string()).into(),
            ("name", "test-pc".to_string()).into(),
            ("ver", "1".to_string()).into(),
        ];
        ServiceInfo::new(
            SERVICE_TYPE,
            "test-pc-abc",
            "landrop-abc.local.",
            &[IpAddr::V4("10.0.0.9".parse().unwrap())] as &[IpAddr],
            8420,
            txt,
        )
        .unwrap()
    }

    #[test]
    fn parses_resolved_service() {
        let p = parse_peer(&sample_info()).unwrap();
        assert_eq!(p.id, "abc");
        assert_eq!(p.name, "test-pc");
        assert_eq!(p.addr, "10.0.0.9");
        assert_eq!(p.port, 8420);
        assert_eq!(p.source, "mdns");
    }

    #[test]
    fn falls_back_to_instance_when_no_txt() {
        let txt: Vec<mdns_sd::TxtProperty> = Vec::new();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "living-room",
            "landrop-x.local.",
            &[IpAddr::V4("10.0.0.9".parse().unwrap())] as &[IpAddr],
            8420,
            txt,
        )
        .unwrap();
        let p = parse_peer(&info).unwrap();
        assert_eq!(p.name, "living-room");
        assert!(p.id.contains("living-room"));
    }

    #[test]
    fn instance_and_hostname_are_unique_per_id() {
        let a = mdns_instance_name("My Laptop", "056daaae-d1e3-43ec-85fd-07bb48c20afa");
        let b = mdns_instance_name("My Laptop", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_ne!(a, b);
        assert!(a.starts_with("My-Laptop-") || a.starts_with("My-Laptop"));
        assert_ne!(
            mdns_hostname("056daaae-d1e3-43ec-85fd-07bb48c20afa"),
            mdns_hostname("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert!(mdns_hostname("abc").ends_with(".local."));
    }

    #[test]
    fn skips_incompatible_versions() {
        let txt: Vec<mdns_sd::TxtProperty> = vec![
            ("id", "abc".to_string()).into(),
            ("name", "test-pc".to_string()).into(),
            ("ver", "99".to_string()).into(),
        ];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "test-pc-abc",
            "landrop-abc.local.",
            &[IpAddr::V4("10.0.0.9".parse().unwrap())] as &[IpAddr],
            8420,
            txt,
        )
        .unwrap();
        assert!(parse_peer(&info).is_none());
    }

    #[test]
    fn long_names_keep_unique_suffix() {
        let long = "x".repeat(80);
        let a = mdns_instance_name(&long, "056daaae-d1e3-43ec-85fd-07bb48c20afa");
        let b = mdns_instance_name(&long, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_ne!(a, b);
        assert!(a.len() <= 63);
        assert!(a.ends_with("-056daaae"));
    }

    #[test]
    fn skip_virtual_nics() {
        assert!(skip_iface("lo"));
        assert!(skip_iface("docker0"));
        assert!(skip_iface("veth1234"));
        assert!(!skip_iface("wlp2s0"));
        assert!(!skip_iface("eth0"));
    }

    #[test]
    fn beacon_roundtrip() {
        let b = Beacon {
            v: 1,
            id: "id-1".into(),
            name: "kitchen".into(),
            port: 8420,
        };
        let enc = encode_beacon(&b);
        let back = decode_beacon(&enc).unwrap();
        assert_eq!(back.name, "kitchen");
        assert_eq!(back.port, 8420);
        assert!(decode_beacon(b"garbage").is_none());
    }

    #[test]
    fn prefers_rfc1918_over_loopback() {
        let picked = pick_best_ip(
            [
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4("192.168.1.20".parse().unwrap()),
                IpAddr::V4("172.17.0.2".parse().unwrap()),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(picked.to_string(), "192.168.1.20");
    }
}
