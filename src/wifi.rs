use crate::Result;
use adb_client::server::{ADBServer, DeviceState};
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
use rand::Rng;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::process::Command;
use std::time::{Duration, Instant};

const PAIRING: &str = "_adb-tls-pairing._tcp.local.";
const CONNECT: &str = "_adb-tls-connect._tcp.local.";
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to let adb auto-connect a freshly paired phone before connecting it ourselves.
const AUTO_CONNECT_WAIT: Duration = Duration::from_secs(5);

fn random_code() -> String {
    rand::thread_rng().gen_range(100_000..1_000_000).to_string()
}

/// Wait up to `timeout` for a resolved mDNS `service` that `want` accepts.
fn discover(
    service: &str,
    timeout: Duration,
    want: impl Fn(&ResolvedService) -> bool,
) -> Result<Box<ResolvedService>> {
    let mdns = ServiceDaemon::new()?;
    let events = mdns.browse(service)?;
    let deadline = Instant::now() + timeout;
    let found = loop {
        match events.recv_deadline(deadline) {
            Ok(ServiceEvent::ServiceResolved(svc)) if want(&svc) => break Ok(svc),
            Ok(_) => {}
            Err(_) => break Err(format!("no device found within {}s", timeout.as_secs()).into()),
        }
    };
    mdns.shutdown().ok();
    found
}

/// IPv4 first, then routable IPv6, then link-local IPv6.
fn rank(addr: &SocketAddr) -> u8 {
    match addr {
        SocketAddr::V4(_) => 0,
        SocketAddr::V6(v6) if v6.scope_id() == 0 => 1,
        SocketAddr::V6(_) => 2,
    }
}

fn best_addr(svc: &ResolvedService) -> Result<SocketAddr> {
    let port = svc.get_port();
    svc.get_addresses()
        .iter()
        .filter_map(|ip| match ip {
            ScopedIp::V4(v4) => Some(SocketAddr::from((*v4.addr(), port))),
            ScopedIp::V6(v6) => {
                // Link-local addresses only work together with the interface they were seen on.
                let scope = if v6.addr().is_unicast_link_local() { v6.scope_id().index } else { 0 };
                Some(SocketAddr::V6(SocketAddrV6::new(*v6.addr(), port, 0, scope)))
            }
            _ => None,
        })
        .min_by_key(rank)
        .ok_or_else(|| "the phone didn't announce an IP address".into())
}

/// Run `adb`. It doesn't reliably exit non-zero on failure, so success means its output contains `ok`.
// ponytail: adb CLI instead of adb_client here, because adb_client's pair/connect take IPv4 only
fn adb(args: &[&str], ok: &str) -> Result<String> {
    let out = Command::new("adb")
        .args(args)
        .output()
        .map_err(|e| format!("couldn't run adb (is it on your PATH?): {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = text.trim().to_string();
    if text.contains(ok) { Ok(text) } else { Err(text.into()) }
}

fn pair_at(addr: SocketAddr, code: &str) -> Result<()> {
    println!("Pairing with {addr}...");
    adb(&["pair", &addr.to_string(), code], "Successfully paired")?;
    println!("Paired: {addr}");
    Ok(())
}

/// Show a QR code, pair with the phone that scans it, then connect.
pub fn pair_qr() -> Result<()> {
    let name = format!("adbee-{}", random_code());
    let password = random_code();
    qr2term::print_qr(format!("WIFI:T:ADB;S:{name};P:{password};;"))?;
    eprintln!("On your phone: Developer options → Wireless debugging → Pair device with QR code");

    // The phone advertises its pairing service under the name from the QR,
    // so other phones with pairing open on the same network are ignored.
    let prefix = format!("{name}.");
    let svc = discover(PAIRING, PAIR_TIMEOUT, |svc| svc.get_fullname().starts_with(&prefix))?;
    let addr = best_addr(&svc)?;
    pair_at(addr, &password)?;
    connect_discovered(Some(addr.ip()), AUTO_CONNECT_WAIT)
}

/// Pair by typing the code shown on the phone. `addr` skips discovery, for networks that block mDNS.
pub fn pair_code(addr: Option<SocketAddr>) -> Result<()> {
    let addr = match addr {
        Some(addr) => addr,
        None => {
            eprintln!("On your phone: Developer options → Wireless debugging → Pair device with pairing code");
            best_addr(&*discover(PAIRING, PAIR_TIMEOUT, |_| true)?)?
        }
    };
    let code = demand::Input::new(format!("Pairing code for {addr}"))
        .validation(|c| match c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()) {
            true => Ok(()),
            false => Err("the code is 6 digits"),
        })
        .run()?;
    pair_at(addr, &code)?;
    connect_discovered(Some(addr.ip()), AUTO_CONNECT_WAIT)
}

/// Connect to a paired phone over WiFi.
/// `target`: nothing (first paired phone found), an IP (that phone), or IP:port (directly, no discovery).
pub fn connect(target: Option<&str>) -> Result<()> {
    if let Some(Ok(addr)) = target.map(str::parse::<SocketAddr>) {
        return connect_at(addr);
    }
    let ip = target
        .map(str::parse::<IpAddr>)
        .transpose()
        .map_err(|_| "expected an IP address or IP:port")?;
    connect_discovered(ip, Duration::ZERO)
}

/// Find the phone on the network and connect it, unless adb does so itself within `wait`.
///
/// Recent adb servers auto-connect paired phones under their mDNS name. Connecting by IP as
/// well lists the same phone twice, so give adb the first chance; right after pairing it needs
/// a moment, which is what `wait` is for. Connecting by IP stays as the fallback for adb
/// servers without auto-connect.
fn connect_discovered(ip: Option<IpAddr>, wait: Duration) -> Result<()> {
    let svc = discover(CONNECT, CONNECT_TIMEOUT, |svc| {
        ip.is_none_or(|ip| svc.get_addresses().iter().any(|a| a.to_ip_addr() == ip))
    })?;
    let name = svc.get_fullname().trim_end_matches(".local.");
    let deadline = Instant::now() + wait;
    loop {
        if ADBServer::default().devices()?.iter().any(|d| d.identifier == name && matches!(d.state, DeviceState::Device)) {
            println!("Connected: {name}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    connect_at(best_addr(&svc)?)
}

fn connect_at(addr: SocketAddr) -> Result<()> {
    // "connected to" also matches "already connected to", not "failed to connect to".
    let out = adb(&["connect", &addr.to_string()], "connected to").map_err(|e| {
        // Most often the phone forgot this computer (wireless debugging toggled, pairings
        // revoked); adb only logs that as a TLS error, so say what to do.
        format!("{e}\nIf the phone was paired before, it may have forgotten this computer: pair again with `adbee`")
    })?;
    println!("{out}");
    Ok(())
}

/// Disconnect one wireless device (serial or IP:port), or all of them.
pub fn disconnect(target: Option<&str>) -> Result<()> {
    let mut args = vec!["disconnect"];
    args.extend(target);
    // Prints "disconnected <target>" or "disconnected everything".
    println!("{}", adb(&args, "disconnected")?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_ipv4_then_routable_ipv6() {
        let mut addrs: Vec<SocketAddr> = ["[fe80::1%4]:5555", "[2001:db8::1]:5555", "192.168.1.5:5555"]
            .map(|a| a.parse().unwrap())
            .into();
        addrs.sort_by_key(rank);
        assert_eq!(addrs[0].to_string(), "192.168.1.5:5555");
        assert_eq!(addrs[1].to_string(), "[2001:db8::1]:5555");
    }
}
