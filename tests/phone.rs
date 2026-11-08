//! End-to-end tests against a real phone. Skipped by default; run with:
//!
//!     cargo test -- --ignored
//!
//! Needs exactly one connected device (or ANDROID_SERIAL set). They never pair, connect or
//! disconnect. Anything written to the phone goes in a fresh `/sdcard/Download/adbee-test-*`
//! folder that is removed afterwards, even when a test fails.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TEST_DIR_PREFIX: &str = "/sdcard/Download/adbee-test-";

fn adbee(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_adbee")).args(args).output().expect("run adbee")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What the shell would offer on Tab after `line`.
fn complete(line: &str) -> Vec<String> {
    let out = adbee(&["__complete_word__", "--shell", "fish", "--line", line]);
    stdout(&out)
        .lines()
        .filter(|l| !l.starts_with('\x01')) // directives to fish, not candidates
        .map(|l| l.split('\t').next().unwrap_or_default().to_string())
        .collect()
}

fn unique() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{}-{nanos}", std::process::id())
}

/// Plain `adb` waits forever on a phone that went away; give up after 15s. True if it finished.
fn adb_with_timeout(args: &[&str]) -> bool {
    let Ok(mut child) = Command::new("adb").args(args).spawn() else { return false };
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// A folder on the phone that is deleted when dropped.
struct PhoneDir(String);

impl PhoneDir {
    fn new() -> Self {
        Self(format!("{TEST_DIR_PREFIX}{}", unique()))
    }
}

impl Drop for PhoneDir {
    fn drop(&mut self) {
        // Belt and braces: only ever delete a folder these tests made.
        if self.0.starts_with(TEST_DIR_PREFIX) && !self.0.contains("..") {
            let removed = adb_with_timeout(&["shell", &format!("rm -r '{}'", self.0)]);
            if !removed {
                eprintln!("couldn't remove {} from the phone (it stopped responding)", self.0);
            }
        }
    }
}

/// A local scratch folder that is deleted when dropped.
struct LocalDir(PathBuf);

impl LocalDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("adbee-test-{}", unique()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for LocalDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_same_tree(a: &Path, b: &Path) {
    for entry in fs::read_dir(a).unwrap() {
        let entry = entry.unwrap();
        let other = b.join(entry.file_name());
        if entry.path().is_dir() {
            assert_same_tree(&entry.path(), &other);
        } else {
            assert_eq!(fs::read(entry.path()).unwrap(), fs::read(&other).unwrap(), "{other:?} differs");
        }
    }
}

#[test]
#[ignore = "needs a connected phone"]
fn devices_shows_a_ready_card() {
    let out = adbee(&["devices"]);
    assert!(out.status.success());
    let text = stdout(&out);
    for line in ["ready", "Android", "Battery", "Storage", "Via"] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
}

#[test]
#[ignore = "needs a connected phone"]
fn folder_round_trip_is_byte_identical() {
    let local = LocalDir::new();
    let src = local.0.join("upload");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), "hello\n").unwrap();
    fs::write(src.join("sub/it's here.txt"), "quotes in names\n").unwrap();
    // 3 MB of non-repeating bytes, so a dropped or reordered chunk can't go unnoticed.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    fs::write(src.join("sub/big.bin"), &big).unwrap();

    let phone = PhoneDir::new();
    let out = adbee(&["push", src.to_str().unwrap(), &format!("{}/", phone.0)]);
    assert!(out.status.success(), "push: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).contains("(3 files)"));

    let listing = stdout(&adbee(&["ls", &format!("{}/upload/sub", phone.0)]));
    assert!(listing.contains("big.bin") && listing.contains("it's here.txt"), "{listing}");

    let back = local.0.join("back");
    fs::create_dir_all(&back).unwrap();
    let out = adbee(&["pull", &format!("{}/upload", phone.0), back.to_str().unwrap()]);
    assert!(out.status.success(), "pull: {}", String::from_utf8_lossy(&out.stderr));
    assert_same_tree(&src, &back.join("upload"));
}

#[test]
#[ignore = "needs a connected phone"]
fn pulling_a_missing_file_fails_without_leaving_one_behind() {
    let local = LocalDir::new();
    let target = local.0.join("nope.txt");
    let out = adbee(&["pull", "/sdcard/Download/adbee-does-not-exist.txt", target.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such file"));
    assert!(!target.exists());
}

#[test]
#[ignore = "needs a connected phone"]
fn ls_of_a_missing_folder_fails() {
    let out = adbee(&["ls", "/sdcard/adbee-does-not-exist"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No such file"));
}

#[test]
#[ignore = "needs a connected phone"]
fn screenshot_is_a_png() {
    let local = LocalDir::new();
    let shot = local.0.join("shot.png");
    let out = adbee(&["screenshot", shot.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Only the header is checked; the image is deleted with the folder.
    let bytes = fs::read(&shot).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
#[ignore = "needs a connected phone"]
fn completes_paths_on_the_phone() {
    assert_eq!(complete("adbee pull "), ["/data/local/tmp/", "/sdcard/", "/storage/"]);

    let sdcard = complete("adbee pull /sdcard/");
    assert!(sdcard.contains(&"/sdcard/Download/".to_string()), "{sdcard:?}");
    assert!(sdcard.iter().all(|p| !p.starts_with("/sdcard/.")), "hidden files offered unasked");

    let partial = complete("adbee ls /sdcard/Dow");
    assert_eq!(partial, ["/sdcard/Download/"]);

    let into_push = complete("adbee push Cargo.toml /sdcard/Dow");
    assert_eq!(into_push, ["/sdcard/Download/"]);
}

#[test]
#[ignore = "needs a connected phone"]
fn completes_device_serials() {
    let serials = complete("adbee -s ");
    assert!(!serials.is_empty());
    let adb = String::from_utf8_lossy(&Command::new("adb").arg("devices").output().unwrap().stdout).into_owned();
    for serial in &serials {
        assert!(adb.contains(serial.as_str()), "{serial} is not in `adb devices`");
    }
}
