//! Background updates. A copy the installer put in place asks GitHub for newer
//! stable releases, checks the installer against the SHA-256 GitHub recorded
//! when it was uploaded, and runs it silently. The installer stops Portside,
//! upgrades it and starts it again. Nothing shows, and the strip and the UI
//! thread never wait on any of it.

use crate::{
    fetch,
    json::{self, Json},
    wide,
};
use std::{
    env, fmt, fs,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::null_mut,
    sync::{Mutex, PoisonError},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::System::{
    Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW},
    Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW},
};

/// Where releases come from and how often to look. The `test-update-server`
/// feature swaps in a local server (named at build time by
/// `PORTSIDE_TEST_UPDATE_URL`) and a fast schedule, for tests only.
#[cfg(not(feature = "test-update-server"))]
mod config {
    use std::time::Duration;
    pub const RELEASE_URL: &str =
        "https://api.github.com/repos/leonbjorklund/portside/releases/latest";
    pub const DOWNLOAD_URL: &str = "https://github.com/leonbjorklund/portside/releases/download";
    pub const FIRST_CHECK: Duration = Duration::from_secs(60);
    pub const PERIOD: Duration = Duration::from_secs(6 * 3600);
    pub const JITTER_SECS: u64 = 1800;
    pub const RETRY: Duration = Duration::from_secs(3600);
    pub const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(30), Duration::from_secs(300)];
    pub const MIN_WAIT: Duration = Duration::from_secs(60);
    pub const BACKOFF: Duration = Duration::from_secs(24 * 3600);
}

#[cfg(feature = "test-update-server")]
mod config {
    use std::time::Duration;
    pub const RELEASE_URL: &str = concat!(env!("PORTSIDE_TEST_UPDATE_URL"), "/latest");
    pub const DOWNLOAD_URL: &str = concat!(env!("PORTSIDE_TEST_UPDATE_URL"), "/download");
    pub const FIRST_CHECK: Duration = Duration::from_secs(2);
    pub const PERIOD: Duration = Duration::from_secs(4);
    pub const JITTER_SECS: u64 = 2;
    pub const RETRY: Duration = Duration::from_secs(4);
    pub const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(1)];
    pub const MIN_WAIT: Duration = Duration::from_secs(1);
    pub const BACKOFF: Duration = Duration::from_secs(30);
}

/// The longest wait a rate limit can ask for.
const MAX_WAIT: Duration = Duration::from_secs(24 * 3600);
const MAX_RELEASE: u64 = 1 << 20;
const MAX_INSTALLER: u64 = 64 << 20;
/// The installer's uninstall registration. It exists only while Portside is installed.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{AFF72E51-A6A3-43E6-A6A4-71C15C264409}_is1";
/// Names the version last handed to the installer, and when.
const MARKER: &str = "attempt";
const INSTALLER_ARGS: [&str; 4] = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u32, u32, u32);

impl Version {
    /// `major.minor.patch` and nothing else, so prereleases never parse.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.').map(|part| {
            let plain = !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            plain.then(|| part.parse().ok()).flatten()
        });
        let version = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// A newer release whose metadata passed every check.
#[derive(Debug, PartialEq)]
struct Update {
    version: Version,
    name: String,
    /// Built from the tag and file name, never taken from the response.
    url: String,
    size: u64,
    sha256: [u8; 32],
}

fn hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

/// Reads GitHub's "latest release" response. `Ok(None)` means nothing to
/// install: a draft, a prerelease, or a version that is not newer.
fn assess(current: Version, body: &str) -> Result<Option<Update>, &'static str> {
    let release = json::parse(body)?;
    let tag = release
        .get("tag_name")
        .and_then(Json::str)
        .ok_or("no tag")?;
    let version = tag
        .strip_prefix('v')
        .and_then(Version::parse)
        .ok_or("unexpected tag")?;
    let flag = |key| {
        release
            .get(key)
            .and_then(Json::bool)
            .ok_or("no release flags")
    };
    if flag("draft")? || flag("prerelease")? || version <= current {
        return Ok(None);
    }
    let name = format!("Portside-{version}-x64-setup.exe");
    let mut matches = release
        .get("assets")
        .and_then(Json::array)
        .ok_or("no assets")?
        .iter()
        .filter(|asset| asset.get("name").and_then(Json::str) == Some(&name));
    let (Some(asset), None) = (matches.next(), matches.next()) else {
        return Err("no single matching installer");
    };
    if asset.get("state").and_then(Json::str) != Some("uploaded") {
        return Err("installer is not uploaded");
    }
    let size = asset
        .get("size")
        .and_then(Json::u64)
        .filter(|size| (1..=MAX_INSTALLER).contains(size))
        .ok_or("bad installer size")?;
    let sha256 = asset
        .get("digest")
        .and_then(Json::str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .and_then(hex32)
        .ok_or("no SHA-256 digest")?;
    Ok(Some(Update {
        version,
        url: format!("{}/v{version}/{name}", config::DOWNLOAD_URL),
        name,
        size,
        sha256,
    }))
}

#[derive(Debug, PartialEq)]
enum Step {
    Release,
    Unchanged,
    Wait(Duration),
    Retry,
    Fail,
}

/// What a response from the release endpoint calls for. GitHub signals a spent
/// rate limit with 403 or 429 plus `x-ratelimit-remaining: 0` or `retry-after`.
fn classify(reply: &fetch::Reply, now: u64) -> Step {
    match reply.status {
        200 => Step::Release,
        304 => Step::Unchanged,
        403 | 429 => {
            let limited =
                reply.status == 429 || reply.retry_after.is_some() || reply.remaining == Some(0);
            if !limited {
                return Step::Fail;
            }
            let secs = reply
                .retry_after
                .or(reply.reset.map(|at| at.saturating_sub(now) + 1))
                .unwrap_or(0);
            Step::Wait(Duration::from_secs(secs).clamp(config::MIN_WAIT, MAX_WAIT))
        }
        500..=599 => Step::Retry,
        _ => Step::Fail,
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Done,
    Failed,
    Limited(Duration),
}

fn next_delay(outcome: &Outcome, jitter: u64) -> Duration {
    let jitter = Duration::from_secs(jitter);
    match outcome {
        Outcome::Done => config::PERIOD + jitter,
        Outcome::Failed => config::RETRY,
        Outcome::Limited(wait) => *wait + jitter,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn jitter() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    u64::from(nanos) % config::JITTER_SECS
}

fn read_attempt(dir: &Path) -> Option<(Version, u64)> {
    let text = fs::read_to_string(dir.join(MARKER)).ok()?;
    let (version, at) = text.trim().split_once(' ')?;
    Some((Version::parse(version)?, at.parse().ok()?))
}

/// A version the installer already got once waits out the backoff, so an
/// install that keeps failing cannot restart Portside in a loop.
fn due(version: Version, attempt: Option<(Version, u64)>, now: u64) -> bool {
    match attempt {
        Some((tried, at)) if tried == version => at > now || now - at >= config::BACKOFF.as_secs(),
        _ => true,
    }
}

/// The folder Portside runs from, when the installer put it there. A copy
/// built from source, or one left after an uninstall, never updates.
fn installed() -> Option<PathBuf> {
    let folder = env::current_exe().ok()?.parent()?.to_path_buf();
    let recorded = registry_string(UNINSTALL_KEY, "InstallLocation")?;
    let same = matches!(
        (fs::canonicalize(&folder), fs::canonicalize(recorded)),
        (Ok(a), Ok(b)) if a == b
    );
    same.then_some(folder)
}

fn registry_string(subkey: &str, name: &str) -> Option<String> {
    let (subkey, name) = (wide(subkey), wide(name));
    unsafe {
        let mut bytes = 0u32;
        let read = |data, bytes: &mut u32| {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                data,
                bytes,
            )
        };
        if read(null_mut(), &mut bytes) != 0 {
            return None;
        }
        let mut text = vec![0u16; bytes as usize / 2];
        if read(text.as_mut_ptr().cast(), &mut bytes) != 0 {
            return None;
        }
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        Some(String::from_utf16_lossy(&text[..end]))
    }
}

/// Downloads the installer next to its final name and gives it that name only
/// when its size and SHA-256 are the release's, so nothing partial or altered
/// can be run.
fn stage(
    update: &Update,
    dir: &Path,
    download: impl FnOnce(&Path) -> Result<(u64, [u8; 32]), String>,
) -> Result<PathBuf, String> {
    let part = dir.join(format!("{}.part", update.name));
    let ready = dir.join(&update.name);
    let result = download(&part).and_then(|(length, digest)| {
        if length != update.size {
            Err(format!("got {length} bytes, expected {}", update.size))
        } else if digest != update.sha256 {
            Err("the download does not match its SHA-256".into())
        } else {
            fs::rename(&part, &ready).map_err(|e| format!("could not finish {ready:?}: {e}"))
        }
    });
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result.map(|()| ready)
}

/// Set once Portside is closing. It also serializes starting the installer,
/// so an uninstall, which closes Portside first, is never followed by one.
static LAUNCH: Mutex<bool> = Mutex::new(false);

pub fn closing() {
    *LAUNCH.lock().unwrap_or_else(PoisonError::into_inner) = true;
}

/// Runs the installer and waits for it. When it upgrades Portside, it closes
/// this process first. Returning means it ended without doing that.
fn launch(app: &Path, dir: &Path, installer: &Path, version: Version) -> Result<(), String> {
    let mut child = {
        let closing = LAUNCH.lock().unwrap_or_else(PoisonError::into_inner);
        if *closing {
            return Err("Portside is closing".into());
        }
        if installed().as_deref() != Some(app) {
            return Err("Portside is no longer installed here".into());
        }
        fs::write(dir.join(MARKER), format!("{version} {}", unix_now()))
            .map_err(|e| format!("could not record the attempt: {e}"))?;
        // The installer must outlive Portside, so leave this process's job
        // when it allows that.
        let spawn = |flags| {
            Command::new(installer)
                .args(INSTALLER_ARGS)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(flags)
                .spawn()
        };
        spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn(CREATE_NO_WINDOW))
            .map_err(|e| format!("could not run the installer: {e}"))?
    };
    let _ = child.wait();
    let _ = fs::remove_file(installer);
    Ok(())
}

fn install(app: &Path, dir: &Path, update: &Update) -> Result<(), String> {
    if !due(update.version, read_attempt(dir), unix_now()) {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| format!("could not create {dir:?}: {e}"))?;
    let installer = stage(update, dir, |part| {
        fetch::download(&update.url, part, update.size)
    })?;
    launch(app, dir, &installer, update.version)
}

/// One scheduled look at the latest release, installing it when it is newer.
fn check(app: &Path, dir: &Path, current: Version, etag: &mut Option<String>) -> Outcome {
    let mut retries = config::RETRY_DELAYS.iter();
    loop {
        let mut body = Vec::new();
        let conditional = etag
            .as_ref()
            .map(|tag| format!("If-None-Match: {tag}\r\n"))
            .unwrap_or_default();
        let headers = format!(
            "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n{conditional}"
        );
        let result = fetch::get(config::RELEASE_URL, &headers, MAX_RELEASE, &mut |chunk| {
            body.extend_from_slice(chunk);
            Ok(())
        });
        let step = result
            .as_ref()
            .map_or(Step::Retry, |reply| classify(reply, unix_now()));
        match step {
            Step::Retry => match retries.next() {
                Some(delay) => thread::sleep(*delay),
                None => return Outcome::Failed,
            },
            Step::Wait(wait) => return Outcome::Limited(wait),
            Step::Fail => return Outcome::Failed,
            Step::Unchanged => return Outcome::Done,
            Step::Release => {
                // Only a check that found nothing to do may skip the next
                // download of the metadata.
                *etag = None;
                let release = String::from_utf8(body)
                    .map_err(|_| "response is not UTF-8")
                    .and_then(|text| assess(current, &text));
                return match release {
                    Ok(None) => {
                        let printable =
                            |tag: &String| tag.bytes().all(|b| (0x20..0x7f).contains(&b));
                        *etag = result.ok().and_then(|reply| reply.etag).filter(printable);
                        Outcome::Done
                    }
                    Ok(Some(update)) if install(app, dir, &update).is_ok() => Outcome::Done,
                    _ => Outcome::Failed,
                };
            }
        }
    }
}

/// Clears what an earlier run staged: installers and partial downloads, and
/// the attempt record once that version, or a newer one, is running.
fn tidy(dir: &Path, current: Version) {
    if read_attempt(dir).is_some_and(|(tried, _)| tried <= current) {
        let _ = fs::remove_file(dir.join(MARKER));
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != MARKER {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Starts checking in the background, if this copy was installed.
pub fn start() {
    let Some(app) = installed() else {
        return;
    };
    let Some(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        return;
    };
    let _ = thread::Builder::new()
        .name("updates".into())
        .spawn(move || {
            let dir = app.join("updates");
            // Waiting first lets the installer that started this copy finish,
            // so its own file can go.
            thread::sleep(config::FIRST_CHECK);
            tidy(&dir, current);
            let mut etag = None;
            loop {
                let outcome = check(&app, &dir, current, &mut etag);
                thread::sleep(next_delay(&outcome, jitter()));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: Version = Version(1, 0, 0);
    const HEX: &str = "27e1fc5bebb00238838c42c2ed1f4cd27445e958a41e9e89c7db9637657d4f25";

    fn release(tag: &str, prerelease: bool, asset: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","draft":false,"prerelease":{prerelease},"assets":[{asset}]}}"#
        )
    }

    fn asset(name: &str, state: &str, size: &str, digest: &str) -> String {
        format!(
            r#"{{"name":"{name}","state":"{state}","size":{size},"digest":"{digest}","browser_download_url":"https://evil.example/{name}"}}"#
        )
    }

    fn good_asset() -> String {
        asset(
            "Portside-1.0.1-x64-setup.exe",
            "uploaded",
            "2361857",
            &format!("sha256:{HEX}"),
        )
    }

    fn reply(status: u32) -> fetch::Reply {
        fetch::Reply {
            status,
            etag: None,
            retry_after: None,
            remaining: None,
            reset: None,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("portside-update-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn versions_are_three_plain_numbers() {
        assert_eq!(Version::parse("1.0.10"), Some(Version(1, 0, 10)));
        assert!(Version(1, 0, 10) > Version(1, 0, 9));
        assert!(Version(2, 0, 0) > Version(1, 99, 99));
        for text in [
            "",
            "1",
            "1.0",
            "1.0.0.0",
            "1.0.0-rc1",
            "1.0.0+1",
            "01.0.0",
            "1.0.x",
            "+1.0.0",
            "-1.0.0",
            "1. 0.0",
            "1.0.1234567890",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn accepts_a_newer_stable_release_and_builds_its_own_url() {
        let update = assess(CURRENT, &release("v1.0.1", false, &good_asset()))
            .unwrap()
            .unwrap();
        assert_eq!(update.version, Version(1, 0, 1));
        assert_eq!(update.size, 2361857);
        assert_eq!(update.sha256, hex32(HEX).unwrap());
        assert_eq!(
            update.url,
            format!(
                "{}/v1.0.1/Portside-1.0.1-x64-setup.exe",
                config::DOWNLOAD_URL
            )
        );
    }

    #[test]
    fn installs_nothing_for_older_equal_draft_or_prerelease() {
        for tag in ["v1.0.0", "v0.9.9", "v0.1.1"] {
            assert_eq!(
                assess(CURRENT, &release(tag, false, &good_asset())),
                Ok(None),
                "{tag}"
            );
        }
        assert_eq!(
            assess(CURRENT, &release("v1.0.1", true, &good_asset())),
            Ok(None)
        );
        let draft =
            release("v1.0.1", false, &good_asset()).replace(r#""draft":false"#, r#""draft":true"#);
        assert_eq!(assess(CURRENT, &draft), Ok(None));
    }

    #[test]
    fn rejects_malformed_or_mismatched_metadata() {
        let name = "Portside-1.0.1-x64-setup.exe";
        let digest = format!("sha256:{HEX}");
        let upper = format!("sha256:{}", HEX.to_uppercase());
        let wrong_assets = [
            String::new(),
            asset("Portside-1.0.1-arm64-setup.exe", "uploaded", "10", &digest),
            asset("Portside-1.0.2-x64-setup.exe", "uploaded", "10", &digest),
            asset(
                "Portside-1.0.1-x64-setup.exe.zip",
                "uploaded",
                "10",
                &digest,
            ),
            asset(name, "starter", "10", &digest),
            asset(name, "uploaded", "0", &digest),
            asset(name, "uploaded", "-5", &digest),
            asset(name, "uploaded", "1.5", &digest),
            asset(name, "uploaded", "999999999999", &digest),
            asset(name, "uploaded", "10", "sha256:abc"),
            asset(name, "uploaded", "10", &format!("md5:{}", &HEX[..32])),
            asset(name, "uploaded", "10", HEX),
            asset(name, "uploaded", "10", &format!("sha256:+{}", &HEX[1..])),
            asset(name, "uploaded", "10", &format!("sha256:{HEX}00")),
            format!("{0},{0}", asset(name, "uploaded", "10", &digest)),
        ];
        for assets in wrong_assets {
            assert!(
                assess(CURRENT, &release("v1.0.1", false, &assets)).is_err(),
                "{assets}"
            );
        }
        // Hex digits of either case are the same digest.
        let upper = release("v1.0.1", false, &asset(name, "uploaded", "10", &upper));
        assert!(assess(CURRENT, &upper).unwrap().is_some());
        for tag in ["1.0.1", "v1.0", "v1.0.1-beta.1", "vx", "V1.0.1", ""] {
            assert!(
                assess(CURRENT, &release(tag, false, &good_asset())).is_err(),
                "{tag}"
            );
        }
        let no_flags = r#"{"tag_name":"v1.0.1","assets":[]}"#;
        for body in [
            "<html>Sign in to the network</html>",
            "",
            "null",
            "[]",
            "{}",
            no_flags,
            r#"{"tag_name":"v1.0.1","draft":false,"prerelease":false}"#,
            &release("v1.0.1", false, &good_asset())[..60],
        ] {
            assert!(assess(CURRENT, body).is_err(), "{body}");
        }
    }

    #[test]
    fn answers_each_response_as_github_documents_it() {
        let limited = |status, retry_after, remaining, reset| fetch::Reply {
            retry_after,
            remaining,
            reset,
            ..reply(status)
        };
        let wait = |secs| Step::Wait(Duration::from_secs(secs).clamp(config::MIN_WAIT, MAX_WAIT));
        assert_eq!(classify(&reply(200), 0), Step::Release);
        assert_eq!(classify(&reply(304), 0), Step::Unchanged);
        assert_eq!(classify(&reply(502), 0), Step::Retry);
        for status in [301, 400, 404, 410] {
            assert_eq!(classify(&reply(status), 0), Step::Fail, "{status}");
        }
        // A 403 that is not a rate limit is a plain refusal.
        assert_eq!(
            classify(&limited(403, None, Some(40), Some(500)), 100),
            Step::Fail
        );
        assert_eq!(
            classify(&limited(403, None, Some(0), Some(5000)), 1000),
            wait(4001)
        );
        assert_eq!(
            classify(&limited(403, Some(7200), Some(9), None), 0),
            wait(7200)
        );
        assert_eq!(classify(&limited(429, None, None, None), 0), wait(0));
        assert_eq!(
            classify(&limited(429, Some(u64::MAX), None, None), 0),
            Step::Wait(MAX_WAIT)
        );
        // A reset time already past still waits the minimum.
        assert_eq!(
            classify(&limited(403, None, Some(0), Some(10)), 1000),
            wait(1)
        );
    }

    #[test]
    fn schedules_the_next_check_by_outcome() {
        let limit = Duration::from_secs(5000);
        assert_eq!(next_delay(&Outcome::Done, 0), config::PERIOD);
        assert_eq!(
            next_delay(&Outcome::Done, 9),
            config::PERIOD + Duration::from_secs(9)
        );
        assert_eq!(next_delay(&Outcome::Failed, 9), config::RETRY);
        assert_eq!(next_delay(&Outcome::Limited(limit), 0), limit);
        assert!(jitter() < config::JITTER_SECS);
    }

    #[test]
    fn a_version_is_handed_to_the_installer_once_per_backoff() {
        let backoff = config::BACKOFF.as_secs();
        let tried = Some((Version(1, 0, 1), 1000));
        assert!(due(Version(1, 0, 1), None, 1000));
        assert!(!due(Version(1, 0, 1), tried, 1000));
        assert!(!due(Version(1, 0, 1), tried, 1000 + backoff - 1));
        assert!(due(Version(1, 0, 1), tried, 1000 + backoff));
        // A newer release is not held back by an older failure, and a clock
        // set back does not freeze updates.
        assert!(due(Version(1, 0, 2), tried, 1001));
        assert!(due(Version(1, 0, 1), tried, 10));
    }

    #[test]
    fn only_a_complete_matching_download_gets_the_installer_name() {
        let update = assess(CURRENT, &release("v1.0.1", false, &good_asset()))
            .unwrap()
            .unwrap();
        let dir = scratch("stage");
        let ready = dir.join(&update.name);
        let part = dir.join(format!("{}.part", update.name));
        // Right size, wrong hash.
        let altered = stage(&update, &dir, |path| {
            assert_eq!(path, part);
            fs::write(path, b"x").unwrap();
            Ok((update.size, [0u8; 32]))
        });
        assert!(altered.is_err());
        // Right hash, wrong size.
        let short = stage(&update, &dir, |path| {
            fs::write(path, b"x").unwrap();
            Ok((update.size - 1, update.sha256))
        });
        assert!(short.is_err());
        // The download fails halfway.
        let failed = stage(&update, &dir, |path| {
            fs::write(path, b"half").unwrap();
            Err("connection reset".into())
        });
        assert!(failed.is_err());
        assert!(!ready.exists() && !part.exists());
        // Everything matches.
        let done = stage(&update, &dir, |path| {
            fs::write(path, b"installer").unwrap();
            Ok((update.size, update.sha256))
        });
        assert_eq!(done.unwrap(), ready);
        assert!(ready.exists() && !part.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tidy_clears_staged_files_and_finished_attempts() {
        let dir = scratch("tidy");
        let marker = dir.join(MARKER);
        fs::write(dir.join("Portside-1.0.1-x64-setup.exe"), b"x").unwrap();
        fs::write(dir.join("Portside-1.0.2-x64-setup.exe.part"), b"x").unwrap();
        // The attempt for a version that now runs is finished.
        fs::write(&marker, "1.0.1 1000").unwrap();
        tidy(&dir, Version(1, 0, 1));
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        // One for a newer version stays, so its backoff survives the restart.
        fs::write(&marker, "1.0.2 1000").unwrap();
        tidy(&dir, Version(1, 0, 1));
        assert_eq!(read_attempt(&dir), Some((Version(1, 0, 2), 1000)));
        fs::write(&marker, "garbage").unwrap();
        assert_eq!(read_attempt(&dir), None);
        tidy(&dir, Version(1, 0, 1));
        tidy(&dir.join("missing"), Version(1, 0, 1));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn no_installer_starts_once_portside_is_closing() {
        let dir = scratch("closing");
        closing();
        let started = launch(&dir, &dir, &dir.join("missing.exe"), Version(1, 0, 1));
        assert_eq!(started, Err("Portside is closing".into()));
        assert!(!dir.join(MARKER).exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
