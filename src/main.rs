mod device;
mod wifi;

use std::net::SocketAddr;
use std::path::PathBuf;
use usage_rs::complete::{Candidate, CandidateKind, CompleteCtx};
use usage_rs::completion::Completion;
use usage_rs::{Args, Cli, Subcommands};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A friendly companion for adb: inspect devices, move files, pair over WiFi
#[derive(Cli)]
#[usage(
    bin = "adbee",
    version = env!("CARGO_PKG_VERSION"),
    about,
    completion,
    example("adbee devices", help = "List connected devices with model, Android version, battery and storage"),
    example("adbee push photo.jpg", help = "Copy a file to the phone's Download folder"),
    example("adbee pull /sdcard/DCIM/Camera/IMG_0001.jpg", help = "Copy a file from the phone to this folder"),
    example("adbee push ./photos /sdcard/Pictures/", help = "Copy a whole folder"),
    example("adbee", help = "Pair a phone over WiFi by scanning a QR code (same as `adbee pair`)"),
    example("adbee pair --code", help = "Pair by typing the 6-digit code shown on the phone")
)]
struct Adbee {
    #[usage(flatten)]
    device: DeviceArg,
    #[usage(subcommand)]
    command: Option<Command>,
}

/// Kept in its own struct because usage-rs attaches completers to `Args` fields only.
#[derive(Args)]
struct DeviceArg {
    /// Device to use when several are connected (see `adbee devices`)
    #[usage(short, long, global, env = "ANDROID_SERIAL", complete = complete_serial)]
    serial: Option<String>,
}

#[derive(Subcommands)]
enum Command {
    /// List connected devices with their details
    Devices,
    Push(Push),
    Pull(Pull),
    Ls(Ls),
    Screenshot(Screenshot),
    Pair(Pair),
    Connect(Connect),
    Disconnect(Disconnect),
    /// Generate shell completions
    Completion(Completion),
}

/// Copy a file or folder from this computer to the device
#[derive(Args)]
struct Push {
    /// File or folder on this computer
    #[usage(value_hint = usage_rs::ValueHint::AnyPath)]
    local: PathBuf,
    /// Folder or file path on the device
    #[usage(default = "/sdcard/Download/", complete = complete_remote)]
    remote: String,
}

/// Copy a file or folder from the device to this computer
#[derive(Args)]
struct Pull {
    /// File or folder on the device
    #[usage(complete = complete_remote)]
    remote: String,
    /// Folder or file path on this computer
    #[usage(default = ".", value_hint = usage_rs::ValueHint::AnyPath)]
    local: PathBuf,
}

/// List files in a folder on the device
#[derive(Args)]
struct Ls {
    /// Folder on the device
    #[usage(default = "/sdcard", complete = complete_remote)]
    path: String,
}

/// Save a screenshot of the device
#[derive(Args)]
struct Screenshot {
    /// Where to save it (default: screenshot-<time>.png)
    #[usage(value_hint = usage_rs::ValueHint::FilePath)]
    output: Option<PathBuf>,
}

/// Pair a phone over WiFi (QR code by default), then connect
#[derive(Args)]
struct Pair {
    /// Type the 6-digit code from "Pair device with pairing code" instead of scanning a QR code
    #[usage(long)]
    code: bool,
    /// IP:port from the phone's pairing screen, for networks where discovery fails (implies --code)
    addr: Option<SocketAddr>,
}

/// Reconnect to an already-paired phone over WiFi
#[derive(Args)]
struct Connect {
    /// An IP address to pick that phone, or IP:port to connect without discovery
    target: Option<String>,
}

/// Disconnect a wireless device (all of them if none is named)
#[derive(Args)]
struct Disconnect {
    /// Serial (from `adbee devices`) or IP:port; defaults to --serial/ANDROID_SERIAL
    target: Option<String>,
}

/// `-s/--serial` as typed on the line being completed, else ANDROID_SERIAL.
fn serial_on_line(ctx: &CompleteCtx<'_>) -> Option<String> {
    let words = ctx.words;
    words
        .iter()
        .enumerate()
        .find_map(|(i, word)| match word.as_str() {
            "-s" | "--serial" => words.get(i + 1).cloned(),
            _ => word.strip_prefix("--serial=").map(str::to_string),
        })
        .or_else(|| std::env::var("ANDROID_SERIAL").ok())
}

fn complete_remote<P>(_: &P, ctx: &CompleteCtx<'_>) -> Vec<Candidate<'static>> {
    device::complete_remote_paths(serial_on_line(ctx), ctx.prefix)
        .into_iter()
        .map(|(path, dir)| Candidate::new(path).with_kind(if dir { CandidateKind::Directory } else { CandidateKind::File }))
        .collect()
}

fn complete_serial<P>(_: &P, _: &CompleteCtx<'_>) -> Vec<Candidate<'static>> {
    device::complete_serials().into_iter().map(|(serial, model)| Candidate::described(serial, model)).collect()
}

/// Run `adb`. It doesn't reliably exit non-zero on failure, so success means its output contains `ok`.
// ponytail: the adb CLI where adb_client falls short: its pair/connect take IPv4 only, and its
// `adb devices` parser rejects serials with spaces
pub fn adb(args: &[&str], ok: &str) -> Result<String> {
    let out = std::process::Command::new("adb")
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

fn main() {
    if let Err(e) = run(Adbee::parse()) {
        eprintln!("adbee: {e}");
        std::process::exit(1);
    }
}

fn run(Adbee { device: DeviceArg { serial }, command }: Adbee) -> Result<()> {
    // No subcommand: QR pairing, the thing people reach for most.
    let Some(command) = command else { return wifi::pair_qr() };
    match command {
        Command::Devices => device::devices(),
        Command::Push(a) => device::push(&mut device::pick(serial, true)?, &a.local, &a.remote),
        Command::Pull(a) => device::pull(&mut device::pick(serial, true)?, &a.remote, &a.local),
        Command::Ls(a) => device::ls(&mut device::pick(serial, true)?, &a.path),
        Command::Screenshot(a) => device::screenshot(&mut device::pick(serial, true)?, a.output.as_deref()),
        Command::Pair(a) if a.code || a.addr.is_some() => wifi::pair_code(a.addr),
        Command::Pair(_) => wifi::pair_qr(),
        Command::Connect(a) => wifi::connect(a.target.as_deref()),
        Command::Disconnect(a) => wifi::disconnect(a.target.as_deref().or(serial.as_deref())),
        Command::Completion(c) => Ok(c.run("adbee", &mut std::io::stdout())?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_defaults() {
        let cli = Adbee::try_parse_from(&["adbee", "-s", "abc", "push", "a.txt"].map(std::ffi::OsStr::new)).unwrap();
        assert_eq!(cli.device.serial.as_deref(), Some("abc"));
        let Some(Command::Push(p)) = cli.command else { panic!("expected push") };
        assert_eq!((p.local.to_str(), p.remote.as_str()), (Some("a.txt"), "/sdcard/Download/"));
    }
}
