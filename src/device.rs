use crate::Result;
use adb_client::server::DeviceState;
use adb_client::server_device::ADBServerDevice;
use adb_client::{ADBDeviceExt, ADBListItemType};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;

/// How long a device may go without replying before adbee gives up. adb itself waits forever on
/// a phone that vanished (wireless debugging switched off, WiFi asleep), and adb_client exposes
/// no timeout for requests through the adb server, so a watchdog thread does it.
const STALL: Duration = Duration::from_secs(30);
static LAST_REPLY: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// The device just answered.
fn alive() {
    LAST_REPLY.store(now(), Ordering::Relaxed);
}

/// From here on, exit if the device stops answering. Call once adbee no longer waits on the user.
fn watch() {
    static STARTED: Once = Once::new();
    alive();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            if now().saturating_sub(LAST_REPLY.load(Ordering::Relaxed)) > STALL.as_secs() {
                eprintln!(
                    "adbee: the device stopped responding (nothing for {}s). On WiFi, check that \
                     wireless debugging is still on; adb may keep listing it until it notices.",
                    STALL.as_secs()
                );
                std::process::exit(1);
            }
        });
    });
}

/// Marks the device alive on every chunk of a transfer, so slow-but-moving copies never time out.
struct Alive<T>(T);

impl<R: Read> Read for Alive<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        alive();
        Ok(n)
    }
}

impl<W: Write> Write for Alive<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.0.write(buf)?;
        alive();
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Run a shell command on the device and return its output.
fn sh(dev: &mut ADBServerDevice, cmd: &str) -> Result<String> {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = dev.shell_command(&cmd, Some(&mut out), Some(&mut err))?;
    let err = String::from_utf8_lossy(&err).trim().to_string();
    // adb_client 3.2 always reports the exit status as None, so "only stderr" counts as failure too.
    if status.map_or(out.is_empty() && !err.is_empty(), |s| s != 0) {
        return Err(err.into());
    }
    if !err.is_empty() {
        eprintln!("{err}");
    }
    alive();
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Quote a path for the device's shell.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `dir/name` for device paths.
fn join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// One shell round trip for every detail, as `key=value` lines. Missing tools just leave a key empty.
const DETAILS: &str = r#"{ echo "name=$(settings get global device_name)"; echo "maker=$(getprop ro.product.manufacturer)"; echo "model=$(getprop ro.product.model)"; echo "serial=$(getprop ro.serialno)"; echo "android=$(getprop ro.build.version.release)"; echo "sdk=$(getprop ro.build.version.sdk)"; echo "patch=$(getprop ro.build.version.security_patch)"; echo "abi=$(getprop ro.product.cpu.abi)"; echo "battery=$(dumpsys battery | tr '\n' ';')"; echo "storage=$(df -h /data | tail -n1)"; echo "ram=$(grep MemTotal /proc/meminfo)"; echo "screen=$(wm size | tail -n1)"; echo "ip=$(ip -f inet addr show wlan0 | grep -m1 inet)"; echo "wifi=$(cmd wifi status | grep -m1 'connected to')"; } 2>/dev/null"#;

type Details = HashMap<String, String>;

fn parse_details(out: &str) -> Details {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(_, v)| !v.is_empty() && v != "null" && v != "unknown")
        .collect()
}

/// `key: value` out of `dumpsys battery` (lines joined with `;`).
fn battery_field<'a>(battery: &'a str, key: &str) -> Option<&'a str> {
    battery.split(';').find_map(|p| p.trim().strip_prefix(key)?.strip_prefix(':').map(str::trim))
}

/// One card's labelled lines; a line whose data is missing is left out.
fn detail_lines(d: &Details) -> Vec<(&'static str, String)> {
    let get = |k: &str| d.get(k).map(String::as_str);
    let join = |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let mut lines = Vec::new();

    if let Some(v) = get("android") {
        let api = get("sdk").map(|s| format!("API {s}"));
        let patch = get("patch").map(|p| format!("patch {p}"));
        lines.push(("Android", join(vec![Some(v.to_string()), api, patch])));
    }
    if let Some(b) = get("battery") {
        if let Some(level) = battery_field(b, "level").and_then(|l| l.parse::<u8>().ok()) {
            let filled = (usize::from(level) + 5) / 10;
            let bar = format!("{}{}", "▰".repeat(filled), "▱".repeat(10 - filled));
            let color = match level {
                0..20 => console::Color::Red,
                20..50 => console::Color::Yellow,
                _ => console::Color::Green,
            };
            let state = match battery_field(b, "status") {
                Some("2") => Some("charging".to_string()),
                Some("5") => Some("full".to_string()),
                _ => None,
            };
            let temp = battery_field(b, "temperature")
                .and_then(|t| t.parse::<f32>().ok())
                .map(|t| format!("{:.1}°C", t / 10.0));
            let level = format!("{} {level}%", console::style(bar).fg(color));
            lines.push(("Battery", join(vec![Some(level), state, temp])));
        }
    }
    // df columns: Filesystem Size Used Avail Use% Mounted-on
    if let Some(df) = get("storage").map(|s| s.split_whitespace().collect::<Vec<_>>()) {
        if df.len() >= 4 {
            lines.push(("Storage", format!("{} free of {}", df[3], df[1])));
        }
    }
    let ram = get("ram")
        .and_then(|r| r.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map(|kb| format!("{:.0} GB RAM", kb * 1024.0 / 1e9));
    let screen = get("screen").and_then(|s| s.rsplit(": ").next()).map(str::to_string);
    let hardware = join(vec![ram, screen, get("abi").map(str::to_string)]);
    if !hardware.is_empty() {
        lines.push(("Hardware", hardware));
    }
    let ip = get("ip").and_then(|l| l.split_whitespace().nth(1)?.split('/').next()).map(str::to_string);
    let ssid = get("wifi").and_then(|w| w.split('"').nth(1)).filter(|s| !s.starts_with('<'));
    match (ip, ssid) {
        (Some(ip), Some(ssid)) => lines.push(("Network", format!("{ip} on \"{ssid}\""))),
        (Some(ip), None) => lines.push(("Network", ip)),
        _ => {}
    }
    lines
}

/// How adb reaches the device, from its adb serial.
fn via(identifier: &str) -> String {
    if identifier.starts_with("emulator-") {
        "emulator".into()
    } else if identifier.contains("._adb-tls-connect.") {
        "wifi (auto-connected by adb)".into()
    } else if identifier.contains(':') {
        format!("wifi {identifier}")
    } else {
        "usb".into()
    }
}

/// Everything adb knows about one physical device, possibly reached over several connections.
struct Card {
    state: DeviceState,
    details: Details,
    serials: Vec<String>,
}

impl Card {
    fn render(&self) -> String {
        let get = |k: &str| self.details.get(k).map(String::as_str);
        let maker_model = match (get("maker"), get("model")) {
            (Some(maker), Some(model)) => Some(format!("{maker} {model}")),
            (maker, model) => maker.or(model).map(str::to_string),
        };
        let title = get("name").map(str::to_string).or(maker_model.clone()).unwrap_or_else(|| self.serials[0].clone());
        let (dot, state) = match self.state {
            DeviceState::Device => (console::style("●").green(), "ready".to_string()),
            DeviceState::Unauthorized => {
                (console::style("●").yellow(), "unauthorized: allow debugging on the phone".to_string())
            }
            DeviceState::Authorizing | DeviceState::Connecting => (console::style("●").yellow(), self.state.to_string()),
            _ => (console::style("●").red(), self.state.to_string()),
        };
        let mut out = format!("{dot} {}  {}\n", console::style(title).bold(), console::style(state).dim());
        let subtitle = [maker_model.filter(|_| get("name").is_some()), get("serial").map(str::to_string)];
        let subtitle = subtitle.into_iter().flatten().collect::<Vec<_>>().join(" · ");
        if !subtitle.is_empty() {
            out += &format!("  {}\n", console::style(subtitle).dim());
        }
        let mut lines = detail_lines(&self.details);
        lines.push(("Via", self.serials.iter().map(|s| via(s)).collect::<Vec<_>>().join(", ")));
        for (label, value) in lines {
            out += &format!("  {:<9} {value}\n", console::style(label).dim());
        }
        out
    }
}

/// `adb devices` as (serial, state). Parsed here because adb_client's parser rejects serials
/// containing spaces, which adb uses after an mDNS name clash (`adb-XXXX (2)._adb-tls-connect._tcp`),
/// and then fails every call that lists devices.
pub fn list() -> Result<Vec<(String, DeviceState)>> {
    Ok(parse_devices(&crate::adb(&["devices"], "List of devices attached")?))
}

fn parse_devices(out: &str) -> Vec<(String, DeviceState)> {
    out.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(serial, state)| {
            let state = state.trim();
            let parsed = state.parse().unwrap_or(if state.starts_with("no permissions") {
                DeviceState::NoPerm
            } else {
                DeviceState::Offline
            });
            (serial.to_string(), parsed)
        })
        .collect()
}

fn open(serial: &str) -> ADBServerDevice {
    ADBServerDevice::new(serial.to_string(), None)
}

pub fn devices() -> Result<()> {
    watch();
    tidy_duplicates();
    let mut cards: Vec<Card> = Vec::new();
    for (serial, state) in list()? {
        let details = if matches!(state, DeviceState::Device) {
            // ponytail: details are best effort, a device that refuses shows fewer lines
            parse_details(&sh(&mut open(&serial), DETAILS).unwrap_or_default())
        } else {
            Details::new()
        };
        // adb can list one phone twice (USB + WiFi, or auto-connect + `adb connect`): merge by hardware serial.
        let same = details.get("serial").and_then(|s| cards.iter_mut().find(|c| c.details.get("serial") == Some(s)));
        match same {
            Some(card) => card.serials.push(serial),
            None => cards.push(Card { state, details, serials: vec![serial] }),
        }
    }
    if cards.is_empty() {
        println!("No devices. Plug one in over USB, or run `adbee pair`.");
    }
    let rendered: Vec<String> = cards.iter().map(Card::render).collect();
    print!("{}", rendered.join("\n"));
    Ok(())
}

/// adb lists a phone twice when adbee connected it by IP and adb's own auto-connect followed
/// later under the mDNS name. Drop the IP connection, keep adb's, and say so. Only connections
/// to the same hardware (same `ro.serialno`) count; nothing happens if any check fails.
fn tidy_duplicates() {
    let Ok(listed) = list() else { return };
    let ready: Vec<&str> = listed
        .iter()
        .filter(|(_, state)| matches!(state, DeviceState::Device))
        .map(|(serial, _)| serial.as_str())
        .collect();
    let by_ip: Vec<&str> = ready.iter().copied().filter(|s| s.parse::<std::net::SocketAddr>().is_ok()).collect();
    let by_name: Vec<&str> = ready.iter().copied().filter(|s| s.contains("._adb-tls-connect.")).collect();
    if by_ip.is_empty() || by_name.is_empty() {
        return;
    }
    let hardware = |serial: &str| {
        sh(&mut open(serial), "getprop ro.serialno").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let named: HashSet<String> = by_name.iter().filter_map(|s| hardware(s)).collect();
    for ip in by_ip {
        if hardware(ip).is_some_and(|h| named.contains(&h)) && crate::adb(&["disconnect", ip], "disconnected").is_ok() {
            eprintln!("Removed duplicate connection {ip} (adb also has this phone under its own name)");
        }
    }
}

/// The device to act on: `serial` if given, the only one connected, or ask (when `ask` and a terminal).
pub fn pick(serial: Option<String>, ask: bool) -> Result<ADBServerDevice> {
    watch();
    if ask {
        tidy_duplicates(); // not while completing: Tab shouldn't change connections
    }
    let listed = list()?;
    if let Some(serial) = serial {
        if !listed.iter().any(|(s, _)| *s == serial) {
            return Err(format!("no device {serial} (see `adbee devices`)").into());
        }
        return Ok(open(&serial));
    }
    let mut ids: Vec<String> = listed
        .into_iter()
        .filter(|(_, state)| matches!(state, DeviceState::Device))
        .map(|(serial, _)| serial)
        .collect();
    if ids.len() > 1 {
        // One phone can be listed twice (USB + WiFi, or adb's auto-connect + a manual
        // `adb connect`); keep the first entry per hardware serial.
        let mut seen = HashSet::new();
        ids.retain(|id| {
            let serial = sh(&mut open(id), "getprop ro.serialno").ok();
            serial.is_none_or(|s| seen.insert(s.trim().to_string()))
        });
    }
    let id = match ids.len() {
        0 => return Err("no device connected (try `adbee devices`)".into()),
        1 => ids.remove(0),
        _ if ask && std::io::stdin().is_terminal() => {
            LAST_REPLY.store(u64::MAX, Ordering::Relaxed); // waiting on you, not the device
            demand::Select::new("Which device?")
                .options(ids.into_iter().map(demand::DemandOption::new).collect())
                .run()?
        }
        _ => return Err("several devices connected; choose one with --serial or ANDROID_SERIAL".into()),
    };
    alive();
    Ok(open(&id))
}

/// Tab completion: connected devices and what they are.
pub fn complete_serials() -> Vec<(String, String)> {
    watch();
    let Ok(listed) = list() else { return vec![] };
    listed
        .into_iter()
        .map(|(serial, state)| {
            let model = matches!(state, DeviceState::Device)
                .then(|| sh(&mut open(&serial), "getprop ro.product.model").ok())
                .flatten()
                .map(|m| m.trim().to_string())
                .unwrap_or_else(|| state.to_string());
            (serial, model)
        })
        .collect()
}

/// Tab completion: what's in the device folder `prefix` is being typed into, as (path, is folder).
/// Never prompts: with several devices and no serial on the line, there is nothing to offer.
pub fn complete_remote_paths(serial: Option<String>, prefix: &str) -> Vec<(String, bool)> {
    let Some(slash) = prefix.rfind('/') else {
        return ["/sdcard/", "/storage/", "/data/local/tmp/"].map(|p| (p.to_string(), true)).into();
    };
    let (dir, typed) = prefix.split_at(slash + 1);
    let Ok(mut dev) = pick(serial, false) else { return vec![] };
    let Ok(items) = dev.list(dir) else { return vec![] };
    let show = |name: &str| name != "." && name != ".." && (!name.starts_with('.') || typed.starts_with('.'));
    items
        .into_iter()
        .filter_map(|item| match item {
            ADBListItemType::Directory(d) if show(&d.name) => Some((format!("{dir}{}/", d.name), true)),
            ADBListItemType::File(f) | ADBListItemType::Symlink(f) if show(&f.name) => {
                Some((format!("{dir}{}", f.name), false))
            }
            _ => None,
        })
        .collect()
}

fn is_dir(dev: &mut ADBServerDevice, path: &str) -> bool {
    dev.stat(path).is_ok_and(|s| s.file_perm & S_IFMT == S_IFDIR)
}

/// A byte-counting progress bar on stderr; indicatif hides it when stderr isn't a terminal.
fn progress(total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template("{bar:30} {bytes}/{total_bytes} {bytes_per_sec} {wide_msg}")
            .expect("valid template"),
    );
    bar
}

/// Every file under `local` (or `local` itself), paired with where it goes under `remote`.
// ponytail: empty folders aren't recreated on the device; adb creates parents of each file
fn local_files(local: &Path, remote: &str, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    if local.is_dir() {
        for entry in fs::read_dir(local)? {
            let entry = entry?;
            local_files(&entry.path(), &join(remote, &entry.file_name().to_string_lossy()), out)?;
        }
    } else {
        out.push((local.to_path_buf(), remote.to_string()));
    }
    Ok(())
}

pub fn push(dev: &mut ADBServerDevice, local: &Path, remote: &str) -> Result<()> {
    let local = local.canonicalize()?; // so `.` and `..` have a name
    let name = local.file_name().ok_or("nothing to push")?.to_string_lossy();
    let base = if remote.ends_with('/') || is_dir(dev, remote) {
        join(remote, &name)
    } else {
        remote.to_string()
    };
    let mut files = Vec::new();
    local_files(&local, &base, &mut files)?;
    let total = files.iter().map(|(p, _)| p.metadata().map_or(0, |m| m.len())).sum();

    let bar = progress(total);
    for (src, dst) in &files {
        bar.set_message(dst.clone());
        dev.push(&mut bar.wrap_read(Alive(File::open(src)?)), dst)?;
    }
    bar.finish_and_clear();
    println!("{} → {base} ({} file{})", local.display(), files.len(), if files.len() == 1 { "" } else { "s" });
    Ok(())
}

/// Every file under the device folder `remote`, paired with where it goes under `local`.
/// Creates the local folders as it goes, so empty ones come along too.
// ponytail: skips symlinks, sockets and device nodes
fn remote_files(dev: &mut ADBServerDevice, remote: &str, local: &Path, out: &mut Vec<(String, PathBuf, u64)>) -> Result<()> {
    fs::create_dir_all(local)?;
    let items = dev.list(remote)?;
    alive();
    for item in items {
        match item {
            ADBListItemType::Directory(d) if d.name != "." && d.name != ".." => {
                remote_files(dev, &join(remote, &d.name), &local.join(&d.name), out)?
            }
            ADBListItemType::File(f) => out.push((join(remote, &f.name), local.join(&f.name), f.size.into())),
            _ => {}
        }
    }
    Ok(())
}

pub fn pull(dev: &mut ADBServerDevice, remote: &str, local: &Path) -> Result<()> {
    // Check first, so a typo doesn't leave an empty file behind.
    let stat = dev.stat(remote)?;
    if stat.file_perm == 0 {
        return Err(format!("{remote}: no such file or folder on the device").into());
    }
    let remote = remote.trim_end_matches('/');
    let name = remote.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("device");
    let base = if local.is_dir() { local.join(name) } else { local.to_path_buf() };
    let mut files = Vec::new();
    if stat.file_perm & S_IFMT == S_IFDIR {
        remote_files(dev, remote, &base, &mut files)?;
    } else {
        // ponytail: sizes come from adb's 32-bit stat, so the bar's total is off for files over 4 GB
        files.push((remote.to_string(), base.clone(), stat.file_size.into()));
    }
    let total = files.iter().map(|(_, _, size)| size).sum();

    let bar = progress(total);
    for (src, dst, _) in &files {
        bar.set_message(src.clone());
        dev.pull(src, &mut bar.wrap_write(Alive(File::create(dst)?)))?;
    }
    bar.finish_and_clear();
    println!("{remote} → {} ({} file{})", base.display(), files.len(), if files.len() == 1 { "" } else { "s" });
    Ok(())
}

pub fn ls(dev: &mut ADBServerDevice, path: &str) -> Result<()> {
    print!("{}", sh(dev, &format!("ls -laH {}", quote(path)))?);
    Ok(())
}

pub fn screenshot(dev: &mut ADBServerDevice, output: Option<&Path>) -> Result<()> {
    let output = match output {
        Some(p) => p.to_path_buf(),
        None => format!("screenshot-{}.png", SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()).into(),
    };
    dev.framebuffer(&output)?;
    println!("Saved {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_card() {
        console::set_colors_enabled(false);
        let out = "name=Sam's phone\nmaker=samsung\nmodel=SM-N986B\nserial=ABC123\nandroid=13\nsdk=33\n\
            patch=2025-08-01\nabi=arm64-v8a\n\
            battery=Current Battery Service state:;  AC powered: true;  status: 2;  level: 65;  temperature: 280;\n\
            storage=/dev/block/sda32 226G 183G   43G  82% /data/user/0\nram=MemTotal:       11162100 kB\n\
            screen=Override size: 1080x2316\nip=    inet 192.168.1.20/24 brd 192.168.1.255 scope global wlan0\n\
            wifi=Wifi is connected to \"Home\"\nmissing=\n";
        let card = Card {
            state: DeviceState::Device,
            details: parse_details(out),
            serials: vec!["192.168.1.20:37001".into(), "adb-ABC123-x._adb-tls-connect._tcp".into()],
        };
        assert_eq!(
            card.render(),
            "● Sam's phone  ready\n  samsung SM-N986B · ABC123\n\
             \x20 Android   13 · API 33 · patch 2025-08-01\n\
             \x20 Battery   ▰▰▰▰▰▰▰▱▱▱ 65% · charging · 28.0°C\n\
             \x20 Storage   43G free of 226G\n\
             \x20 Hardware  11 GB RAM · 1080x2316 · arm64-v8a\n\
             \x20 Network   192.168.1.20 on \"Home\"\n\
             \x20 Via       wifi 192.168.1.20:37001, wifi (auto-connected by adb)\n"
        );
        // A device that answers nothing still gets a card.
        let bare = Card { state: DeviceState::Unauthorized, details: parse_details(""), serials: vec!["ABC123".into()] };
        assert_eq!(bare.render(), "● ABC123  unauthorized: allow debugging on the phone\n  Via       usb\n");
    }

    #[test]
    fn parses_adb_devices_including_serials_with_spaces() {
        let out = "List of devices attached\n\
            adb-ABC123-4kKTKP (2)._adb-tls-connect._tcp\tdevice\n\
            emulator-5554\toffline\n\
            0123456789\tno permissions (missing udev rules); see [http://developer.android.com/tools/device.html]\n\n";
        let listed = parse_devices(out);
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].0, "adb-ABC123-4kKTKP (2)._adb-tls-connect._tcp");
        assert!(matches!(listed[0].1, DeviceState::Device));
        assert!(matches!(listed[1].1, DeviceState::Offline));
        assert!(matches!(listed[2].1, DeviceState::NoPerm));
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(quote("/sdcard/it's here"), r"'/sdcard/it'\''s here'");
    }

    #[test]
    fn maps_a_local_folder_onto_the_device() {
        let root = std::env::temp_dir().join(format!("adbee-test-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("sub/b.txt"), "b").unwrap();

        let mut files = Vec::new();
        local_files(&root, "/sdcard/Download/", &mut files).unwrap();
        let mut remotes: Vec<&str> = files.iter().map(|(_, r)| r.as_str()).collect();
        remotes.sort();
        assert_eq!(remotes, ["/sdcard/Download/a.txt", "/sdcard/Download/sub/b.txt"]);
        fs::remove_dir_all(root).unwrap();
    }
}
