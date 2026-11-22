use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::{SftpAuthMode, SftpConfig};
use crate::storage::StorageClient;

/// Options controlling the local network scan for MiSTer FPGA devices.
#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    /// Optional subnet/CIDR (e.g. `192.168.1.0/24`). Detected automatically when omitted.
    pub subnet: Option<String>,
    /// SSH port to probe.
    pub port: u16,
    /// Per-host TCP connect/read timeout.
    pub timeout: Duration,
    /// Try to authenticate over SFTP and check `/media/fat` to confirm it is a MiSTer.
    pub verify: bool,
    pub sftp_user: String,
    pub sftp_password: Option<String>,
    pub sftp_key: Option<PathBuf>,
    pub sftp_passphrase: Option<String>,
    /// Number of hosts probed in parallel.
    pub concurrency: usize,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        Self {
            subnet: None,
            port: 22,
            timeout: Duration::from_millis(400),
            verify: true,
            sftp_user: "root".to_string(),
            sftp_password: None,
            sftp_key: None,
            sftp_passphrase: None,
            concurrency: 128,
        }
    }
}

/// A host that looks like a MiSTer FPGA on the local network.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredMister {
    pub ip: String,
    pub port: u16,
    pub latency_ms: u128,
    pub ssh_banner: Option<String>,
    /// True when SFTP authentication succeeded and `/media/fat` could be listed.
    pub verified: bool,
    pub verification_note: Option<String>,
}

/// Progress emitted while scanning the subnet.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryProgress {
    pub scanned: usize,
    pub total: usize,
    pub found: usize,
}

/// Scans the local network for hosts with an open SSH port and (optionally) verifies
/// that they expose a MiSTer-like SFTP filesystem.
pub async fn discover<F>(
    opts: &DiscoverOptions,
    cancel: &AtomicBool,
    on_progress: F,
) -> Result<Vec<DiscoveredMister>>
where
    F: Fn(DiscoveryProgress) + Send + Sync,
{
    let targets = resolve_targets(opts.subnet.as_deref())?;
    let total = targets.len();
    if total == 0 {
        bail!("No hay direcciones que escanear en la subred indicada.");
    }

    let mut iter = targets.into_iter();
    let concurrency = opts.concurrency.clamp(1, 512);
    let mut set: JoinSet<Option<DiscoveredMister>> = JoinSet::new();

    for _ in 0..concurrency {
        let Some(ip) = iter.next() else { break };
        spawn_probe(&mut set, ip, opts.port, opts.timeout);
    }

    let mut candidates = Vec::new();
    let mut scanned = 0usize;

    while let Some(joined) = set.join_next().await {
        scanned += 1;
        if let Ok(Some(host)) = joined {
            candidates.push(host);
        }
        on_progress(DiscoveryProgress {
            scanned,
            total,
            found: candidates.len(),
        });

        if cancel.load(Ordering::Relaxed) {
            set.abort_all();
            break;
        }

        if let Some(ip) = iter.next() {
            spawn_probe(&mut set, ip, opts.port, opts.timeout);
        }
    }

    if opts.verify && !candidates.is_empty() && !cancel.load(Ordering::Relaxed) {
        verify_candidates(&mut candidates, opts).await;
    }

    candidates.sort_by(|a, b| {
        b.verified
            .cmp(&a.verified)
            .then_with(|| a.latency_ms.cmp(&b.latency_ms))
            .then_with(|| a.ip.cmp(&b.ip))
    });

    Ok(candidates)
}

fn spawn_probe(
    set: &mut JoinSet<Option<DiscoveredMister>>,
    ip: Ipv4Addr,
    port: u16,
    timeout: Duration,
) {
    set.spawn(async move { probe_host(ip, port, timeout).await });
}

async fn probe_host(ip: Ipv4Addr, port: u16, timeout: Duration) -> Option<DiscoveredMister> {
    let addr = SocketAddr::from((ip, port));
    let started = Instant::now();
    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;
    let latency_ms = started.elapsed().as_millis();
    let ssh_banner = read_ssh_banner(&mut stream, timeout).await;

    Some(DiscoveredMister {
        ip: ip.to_string(),
        port,
        latency_ms,
        ssh_banner,
        verified: false,
        verification_note: None,
    })
}

async fn read_ssh_banner(stream: &mut TcpStream, timeout: Duration) -> Option<String> {
    let mut buf = [0u8; 256];
    let read = tokio::time::timeout(timeout, stream.read(&mut buf))
        .await
        .ok()?
        .ok()?;
    if read == 0 {
        return None;
    }
    let banner = String::from_utf8_lossy(&buf[..read]).trim().to_string();
    if banner.is_empty() {
        None
    } else {
        Some(banner)
    }
}

async fn verify_candidates(candidates: &mut [DiscoveredMister], opts: &DiscoverOptions) {
    let semaphore = Arc::new(Semaphore::new(8));
    let mut set: JoinSet<(usize, bool, String)> = JoinSet::new();

    for (idx, host) in candidates.iter().enumerate() {
        let Ok(permit) = semaphore.clone().acquire_owned().await else {
            break;
        };
        let ip = host.ip.clone();
        let port = host.port;
        let user = opts.sftp_user.clone();
        let password = opts.sftp_password.clone();
        let key = opts.sftp_key.clone();
        let passphrase = opts.sftp_passphrase.clone();

        set.spawn(async move {
            let _permit = permit;
            let (verified, note) = verify_host(ip, port, user, password, key, passphrase).await;
            (idx, verified, note)
        });
    }

    while let Some(joined) = set.join_next().await {
        if let Ok((idx, verified, note)) = joined
            && let Some(host) = candidates.get_mut(idx)
        {
            host.verified = verified;
            host.verification_note = Some(note);
        }
    }
}

async fn verify_host(
    ip: String,
    port: u16,
    user: String,
    password: Option<String>,
    key: Option<PathBuf>,
    passphrase: Option<String>,
) -> (bool, String) {
    let auth_mode = if key.is_some() {
        SftpAuthMode::PrivateKey
    } else {
        SftpAuthMode::Password
    };
    let cfg = SftpConfig {
        host: ip,
        port,
        username: user,
        auth_mode,
        password: password.unwrap_or_else(|| "1".to_string()),
        private_key_path: key
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        passphrase: passphrase.unwrap_or_default(),
        remote_path: "/media/fat".to_string(),
    };

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let storage = StorageClient::connect_sftp(&cfg).await?;
        let check = storage.list_subdirs("/media/fat").await;
        storage.close().await;
        check
    })
    .await;

    match result {
        Ok(Ok(_)) => (true, "SFTP /media/fat accesible".to_string()),
        Ok(Err(err)) => (
            false,
            format!("SSH abierto; la verificación SFTP falló: {err:#}"),
        ),
        Err(_) => (
            false,
            "SSH abierto; la verificación SFTP agotó el tiempo".to_string(),
        ),
    }
}

fn resolve_targets(spec: Option<&str>) -> Result<Vec<Ipv4Addr>> {
    match spec.map(str::trim).filter(|s| !s.is_empty()) {
        Some(spec) => parse_subnet(spec),
        None => {
            let local = local_ipv4().context(
                "No se pudo detectar la IP local. Indica una subred con --subnet (ej. 192.168.1.0/24).",
            )?;
            let prefix = local_prefix_len(local).unwrap_or(24);
            expand_cidr(local, prefix)
        }
    }
}

fn parse_subnet(spec: &str) -> Result<Vec<Ipv4Addr>> {
    let spec = spec.trim();
    if let Some((base, prefix)) = spec.split_once('/') {
        let base: Ipv4Addr = base
            .trim()
            .parse()
            .with_context(|| format!("Subred inválida: {spec}"))?;
        let prefix: u8 = prefix
            .trim()
            .parse()
            .with_context(|| format!("Prefijo CIDR inválido: {spec}"))?;
        return expand_cidr(base, prefix);
    }

    let trimmed = spec.trim_end_matches('.');
    let octets: Vec<&str> = trimmed.split('.').collect();
    match octets.as_slice() {
        [a, b, c] => {
            let base: Ipv4Addr = format!("{a}.{b}.{c}.0")
                .parse()
                .with_context(|| format!("Subred inválida: {spec}"))?;
            expand_cidr(base, 24)
        }
        [a, b, c, d] => {
            let ip: Ipv4Addr = format!("{a}.{b}.{c}.{d}")
                .parse()
                .with_context(|| format!("Dirección IP inválida: {spec}"))?;
            Ok(vec![ip])
        }
        _ => bail!("Subred inválida: {spec}. Usa CIDR (192.168.1.0/24) o una IP."),
    }
}

fn expand_cidr(base: Ipv4Addr, prefix: u8) -> Result<Vec<Ipv4Addr>> {
    if prefix > 32 {
        bail!("Prefijo CIDR inválido: /{prefix}");
    }
    if prefix < 16 {
        bail!(
            "La subred /{prefix} es demasiado grande. Usa un prefijo /16 o más específico (máximo 65534 hosts)."
        );
    }

    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = u32::from(base) & mask;
    let broadcast = network | !mask;

    if prefix >= 31 {
        return Ok((network..=broadcast).map(Ipv4Addr::from).collect());
    }

    let mut hosts = Vec::new();
    for raw in (network + 1)..broadcast {
        hosts.push(Ipv4Addr::from(raw));
    }
    Ok(hosts)
}

fn local_ipv4() -> Option<Ipv4Addr> {
    // Connecting a UDP socket does not send packets; it only asks the OS which
    // local address would be used to reach that destination.
    for target in ["8.8.8.8:80", "1.1.1.1:80", "192.168.1.1:80", "10.0.0.1:80"] {
        let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if socket.connect(target).is_err() {
            continue;
        }
        if let Ok(addr) = socket.local_addr()
            && let IpAddr::V4(ip) = addr.ip()
            && !ip.is_loopback()
            && !ip.is_unspecified()
        {
            return Some(ip);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn local_prefix_len(local: Ipv4Addr) -> Option<u8> {
    let content = std::fs::read_to_string("/proc/net/route").ok()?;
    let mut best: Option<u8> = None;

    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let (Some(dest), Some(mask)) = (parse_route_ipv4(fields[1]), parse_route_ipv4(fields[7]))
        else {
            continue;
        };
        if mask == Ipv4Addr::UNSPECIFIED {
            continue;
        }
        if (u32::from(local) & u32::from(mask)) == u32::from(dest) {
            let prefix = u32::from(mask).count_ones() as u8;
            best = Some(best.map_or(prefix, |current| current.max(prefix)));
        }
    }

    best
}

#[cfg(not(target_os = "linux"))]
fn local_prefix_len(_local: Ipv4Addr) -> Option<u8> {
    None
}

#[cfg(target_os = "linux")]
fn parse_route_ipv4(hex: &str) -> Option<Ipv4Addr> {
    let raw = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(raw.swap_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_small_cidr() {
        let hosts = expand_cidr(Ipv4Addr::new(192, 168, 1, 0), 30).unwrap();
        assert_eq!(
            hosts,
            vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 2)]
        );
    }

    #[test]
    fn parses_three_octet_subnet() {
        let hosts = parse_subnet("192.168.1").unwrap();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts.first(), Some(&Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(hosts.last(), Some(&Ipv4Addr::new(192, 168, 1, 254)));
    }

    #[test]
    fn parses_single_ip() {
        assert_eq!(
            parse_subnet("192.168.1.50").unwrap(),
            vec![Ipv4Addr::new(192, 168, 1, 50)]
        );
    }

    #[test]
    fn rejects_too_large_subnet() {
        assert!(expand_cidr(Ipv4Addr::new(10, 0, 0, 0), 8).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_proc_route_hex() {
        assert_eq!(
            parse_route_ipv4("0002A8C0"),
            Some(Ipv4Addr::new(192, 168, 2, 0))
        );
        assert_eq!(
            parse_route_ipv4("00FFFFFF"),
            Some(Ipv4Addr::new(255, 255, 255, 0))
        );
    }
}
