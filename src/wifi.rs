use crate::Result;
use adb_client::server::DeviceState;
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
use rand::Rng;
use std::net::{IpAddr, SocketAddr, SocketAddrV6, TcpStream};
use std::io::IsTerminal;
use std::time::{Duration, Instant};

const PAIRING: &str = "_adb-tls-pairing._tcp.local.";
const CONNECT: &str = "_adb-tls-connect._tcp.local.";
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait when checking that something listens on an announced port.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// After the first phone answers, how long to keep listening for others.
const GATHER: Duration = Duration::from_secs(2);

fn random_code() -> String {
    rand::thread_rng().gen_range(100_000..1_000_000).to_string()
}

/// Every phone announcing `service` that `want` accepts: waits up to `timeout` for the first,
/// then `gather` longer for any others.
fn discover_all(
    service: &str,
    timeout: Duration,
    gather: Duration,
    want: impl Fn(&ResolvedService) -> bool,
) -> Result<Vec<ResolvedService>> {
    let mdns = ServiceDaemon::new()?;
    let events = mdns.browse(service)?;
    let mut deadline = Instant::now() + timeout;
    let mut found: Vec<ResolvedService> = Vec::new();
    while let Ok(event) = events.recv_deadline(deadline) {
        if let ServiceEvent::ServiceResolved(svc) = event {
            if want(&svc) && !found.iter().any(|f| f.get_fullname() == svc.get_fullname()) {
                if found.is_empty() {
                    deadline = Instant::now() + gather;
                }
                found.push(*svc);
            }
        }
    }
    mdns.shutdown().ok();
    if found.is_empty() {
        return Err(format!("no device found within {}s", timeout.as_secs()).into());
    }
    Ok(found)
}

/// The first phone announcing `service` that `want` accepts, waiting up to `timeout`.
fn discover(
    service: &str,
    timeout: Duration,
    want: impl Fn(&ResolvedService) -> bool,
) -> Result<ResolvedService> {
    Ok(discover_all(service, timeout, Duration::ZERO, want)?.remove(0))
}

/// Whether something listens on the announced port. Phones keep announcing an old name for a
/// while after wireless debugging restarts (`adb-XXXX` on a closed port next to the live
/// `adb-XXXX (2)`); this skips those.
fn reachable(svc: &ResolvedService) -> bool {
    best_addr(svc).is_ok_and(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
}

/// Which of `items` to act on: all of them with `all`, the only one, or a checkbox list (space/x
/// toggles one, `a` toggles all). Without a terminal to ask on, several items are an error that
/// says what to pass instead.
fn choose<T>(items: Vec<T>, all: bool, title: &str, label: impl Fn(&T) -> String, hint: &str) -> Result<Vec<T>> {
    if all || items.len() <= 1 {
        return Ok(items);
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!("several found; {hint}").into());
    }
    crate::device::pause_watch();
    let options = items.iter().enumerate().map(|(i, item)| demand::DemandOption::new(i).label(&label(item))).collect();
    let picked = demand::MultiSelect::new(title)
        .description("space to tick, a for all, enter to confirm")
        .options(options)
        .min(1)
        .run()?;
    Ok(items.into_iter().enumerate().filter(|(i, _)| picked.contains(i)).map(|(_, item)| item).collect())
}

/// "SM-N986B · 192.168.1.66" for a phone's connect announcement.
fn phone_label(svc: &ResolvedService) -> String {
    let model = svc.get_property_val_str("name").unwrap_or("phone");
    match best_addr(svc) {
        Ok(addr) => format!("{model} · {}", addr.ip()),
        Err(_) => model.to_string(),
    }
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

fn pair_at(addr: SocketAddr, code: &str) -> Result<()> {
    println!("Pairing with {addr}...");
    crate::adb(&["pair", &addr.to_string(), code], "Successfully paired")?;
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
    connect_discovered(Some(addr.ip()))
}

/// Pair by typing the code shown on the phone. `addr` skips discovery, for networks that block mDNS.
pub fn pair_code(addr: Option<SocketAddr>) -> Result<()> {
    let addr = match addr {
        Some(addr) => addr,
        None => {
            eprintln!("On your phone: Developer options → Wireless debugging → Pair device with pairing code");
            best_addr(&discover(PAIRING, PAIR_TIMEOUT, |_| true)?)?
        }
    };
    let code = demand::Input::new(format!("Pairing code for {addr}"))
        .validation(|c| match c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()) {
            true => Ok(()),
            false => Err("the code is 6 digits"),
        })
        .run()?;
    pair_at(addr, &code)?;
    connect_discovered(Some(addr.ip()))
}

/// Connect paired phones over WiFi. `target`: nothing (every paired phone found: one is
/// connected, several are offered in a menu with "All"), an IP (that phone), or IP:port
/// (directly, no discovery). `all` connects every phone found without asking.
pub fn connect(target: Option<&str>, all: bool) -> Result<()> {
    if all && target.is_some() {
        return Err("pass either a phone or --all, not both".into());
    }
    if let Some(Ok(addr)) = target.map(str::parse::<SocketAddr>) {
        return connect_at(addr);
    }
    let ip = target
        .map(str::parse::<IpAddr>)
        .transpose()
        .map_err(|_| "expected an IP address or IP:port")?;
    let phones = discover_all(CONNECT, CONNECT_TIMEOUT, GATHER, |svc| {
        ip.is_none_or(|ip| svc.get_addresses().iter().any(|a| a.to_ip_addr() == ip)) && reachable(svc)
    })?;
    let mut todo = Vec::new();
    for svc in phones {
        match connected_as(svc.get_fullname().trim_end_matches(".local."))? {
            Some(serial) => println!("Already connected: {serial}"),
            None => todo.push(svc),
        }
    }
    let chosen = choose(todo, all, "Connect which phones?", phone_label, "name one (IP or IP:port) or pass --all")?;
    let mut failed = 0;
    for svc in chosen {
        // adb may still connect the phone by itself later, under its mDNS name, whenever its
        // retry timer fires (15-40s after a re-pair). There's no way to hurry or stop that, and
        // adb 37 can't connect by name, so connect by IP now; the next adbee command drops
        // whichever connection turns out to be the duplicate (device::tidy_duplicates).
        if let Err(e) = best_addr(&svc).and_then(connect_at) {
            eprintln!("adbee: {e}");
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!("{failed} phone(s) didn't connect").into());
    }
    Ok(())
}

/// After pairing: find the phone at `ip` on the network and connect it, unless adb already has it.
fn connect_discovered(ip: Option<IpAddr>) -> Result<()> {
    let svc = discover(CONNECT, CONNECT_TIMEOUT, |svc| {
        ip.is_none_or(|ip| svc.get_addresses().iter().any(|a| a.to_ip_addr() == ip)) && reachable(svc)
    })?;
    let name = svc.get_fullname().trim_end_matches(".local.");
    if let Some(serial) = connected_as(name)? {
        println!("Already connected: {serial}");
        return Ok(());
    }
    // adb may still connect the phone by itself later, under its mDNS name, whenever its retry
    // timer fires (15-40s after a re-pair). There's no way to hurry or stop that, and adb 37
    // can't connect by name, so connect by IP now; the next adbee command drops whichever
    // connection turns out to be the duplicate (device::tidy_duplicates).
    connect_at(best_addr(&svc)?)
}

/// adb's serial for the phone announced as `name`, if adb has it connected.
fn connected_as(name: &str) -> Result<Option<String>> {
    Ok(crate::device::list()?
        .into_iter()
        .find(|(s, state)| matches!(state, DeviceState::Device) && same_phone(s, name))
        .map(|(serial, _)| serial))
}

/// The mDNS instance name without the ` (2)` a name clash adds: `adb-XXXX (2)._adb-tls-connect._tcp`
/// and `adb-XXXX._adb-tls-connect._tcp` are the same phone.
fn base_name(name: &str) -> &str {
    let instance = name.split("._adb-tls-connect.").next().unwrap_or(name);
    match instance.rfind(" (") {
        Some(i) if instance.ends_with(')') => &instance[..i],
        _ => instance,
    }
}

/// Whether adb's serial `serial` is the mDNS-connected phone announced as `name`.
fn same_phone(serial: &str, name: &str) -> bool {
    serial.contains("._adb-tls-connect.") && base_name(serial) == base_name(name)
}

fn connect_at(addr: SocketAddr) -> Result<()> {
    // "connected to" also matches "already connected to", not "failed to connect to".
    let out = crate::adb(&["connect", &addr.to_string()], "connected to").map_err(|e| {
        // Most often the phone forgot this computer (wireless debugging toggled, pairings
        // revoked); adb only logs that as a TLS error, so say what to do.
        format!("{e}\nIf the phone was paired before, it may have forgotten this computer: pair again with `adbee`")
    })?;
    println!("{out}");
    Ok(())
}

/// Disconnect a wireless device: `target` (serial or IP:port), every one with `all`, or else the
/// only one connected, or one picked from a menu that also offers "All".
pub fn disconnect(target: Option<&str>, all: bool) -> Result<()> {
    if all && target.is_some() {
        return Err("pass either a device or --all, not both".into());
    }
    if let Some(target) = target {
        println!("{}", crate::adb(&["disconnect", target], "disconnected")?);
        return Ok(());
    }
    if all {
        println!("{}", crate::adb(&["disconnect"], "disconnected")?); // "disconnected everything"
        return Ok(());
    }
    let wireless = crate::device::wireless()?;
    if wireless.is_empty() {
        return Err("no wireless devices connected".into());
    }
    let label = |(serial, model): &(String, String)| format!("{model} · {serial}");
    for (serial, _) in choose(wireless, false, "Disconnect which devices?", label, "name one or pass --all")? {
        println!("{}", crate::adb(&["disconnect", &serial], "disconnected")?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treats_a_renamed_announcement_as_the_same_phone() {
        let auto = "adb-ABC123-4kKTKP (2)._adb-tls-connect._tcp";
        assert!(same_phone(auto, "adb-ABC123-4kKTKP._adb-tls-connect._tcp"));
        assert!(same_phone(auto, "adb-ABC123-4kKTKP (2)._adb-tls-connect._tcp"));
        assert!(!same_phone(auto, "adb-OTHER-zzzzzz._adb-tls-connect._tcp"));
        assert!(!same_phone("192.168.1.20:37001", "adb-ABC123-4kKTKP._adb-tls-connect._tcp"));
        // `adb mdns services` lists bare instance names.
        assert_eq!(base_name("adb-ABC123-4kKTKP (2)"), base_name(auto));
    }

    #[test]
    fn chooses_without_asking_when_it_can() {
        let label = |n: &u8| n.to_string();
        assert_eq!(choose(vec![1, 2, 3], true, "", label, "").unwrap(), [1, 2, 3]);
        assert_eq!(choose(vec![7], false, "", label, "").unwrap(), [7]);
        assert!(choose(Vec::<u8>::new(), false, "", label, "").unwrap().is_empty());
        // `cargo test` has no terminal on stdin, so several can't be asked about.
        let err = choose(vec![1, 2], false, "", label, "pass --all").unwrap_err();
        assert_eq!(err.to_string(), "several found; pass --all");
    }

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
