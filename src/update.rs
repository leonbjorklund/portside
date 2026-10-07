//! Background updates. A copy the installer put in place asks GitHub for newer
//! stable releases, checks the installer against the SHA-256 GitHub recorded
//! when it was uploaded, and runs it silently. The installer stops Portside,
//! upgrades it and starts it again. The UI thread never waits on any of it.

use crate::{
    fetch,
    json::{self, Json},
};
use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Where releases come from and how often to look. The `test-update-server`
// feature swaps in a local server (named at build time by
// `PORTSIDE_TEST_UPDATE_URL`) and a fast schedule, for tests only.
#[cfg(not(feature = "test-update-server"))]
const RELEASE_URL: &str = "https://api.github.com/repos/leonbjorklund/portside/releases/latest";
#[cfg(not(feature = "test-update-server"))]
const DOWNLOAD_URL: &str = "https://github.com/leonbjorklund/portside/releases/download";
#[cfg(feature = "test-update-server")]
const RELEASE_URL: &str = concat!(env!("PORTSIDE_TEST_UPDATE_URL"), "/latest");
#[cfg(feature = "test-update-server")]
const DOWNLOAD_URL: &str = concat!(env!("PORTSIDE_TEST_UPDATE_URL"), "/download");
const TEST_SERVER: bool = cfg!(feature = "test-update-server");
const FIRST_CHECK: Duration = Duration::from_secs(if TEST_SERVER { 2 } else { 60 });
const PERIOD: Duration = Duration::from_secs(if TEST_SERVER { 4 } else { 6 * 3600 });
const RETRY: Duration = Duration::from_secs(if TEST_SERVER { 4 } else { 3600 });
const BACKOFF: Duration = Duration::from_secs(if TEST_SERVER { 30 } else { 24 * 3600 });

/// Caps how long a `Retry-After` header can delay the next check.
const MAX_WAIT: Duration = Duration::from_secs(24 * 3600);
/// Names the version last handed to the installer, and when.
const MARKER: &str = "attempt";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u32, u32, u32);

impl Version {
    /// `major.minor.patch` and nothing else, so prereleases never parse.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.').map(|part| {
            let digits = part.bytes().all(|b| b.is_ascii_digit());
            digits.then(|| part.parse().ok()).flatten()
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
    /// Built from the version, never the asset's `browser_download_url`.
    url: String,
    /// Lowercase hex.
    sha256: String,
}

/// Reads GitHub's "latest release" response, which is never a draft or a
/// prerelease. `None` means it is not newer than this copy or failed a check.
fn assess(current: Version, body: &[u8]) -> Option<Update> {
    let release = json::parse(str::from_utf8(body).ok()?)?;
    let version = Version::parse(release.get("tag_name")?.str()?.strip_prefix('v')?)?;
    if version <= current {
        return None;
    }
    let name = format!("Portside-{version}-x64-setup.exe");
    let Json::Array(assets) = release.get("assets")? else {
        return None;
    };
    let asset = assets
        .iter()
        .find(|asset| asset.get("name").and_then(Json::str) == Some(&name))?;
    if asset.get("state")?.str()? != "uploaded" {
        return None;
    }
    let sha256 = asset.get("digest")?.str()?.strip_prefix("sha256:")?;
    let hex = sha256.len() == 64
        && sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then(|| Update {
        version,
        url: format!("{DOWNLOAD_URL}/v{version}/{name}"),
        name,
        sha256: sha256.into(),
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
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
        Some((tried, at)) if tried == version => at > now || now - at >= BACKOFF.as_secs(),
        _ => true,
    }
}

/// The updates folder, when the installer put this copy in place. A copy
/// built from source, or one left after an uninstall, never updates.
fn installed() -> Option<PathBuf> {
    let folder = env::current_exe().ok()?.parent()?.to_path_buf();
    folder
        .join("unins000.exe")
        .is_file()
        .then(|| folder.join("updates"))
}

/// Downloads and starts the installer, unless this version is still waiting
/// out its backoff.
fn install(dir: &Path, update: &Update) -> Option<()> {
    if !due(update.version, read_attempt(dir), unix_now()) {
        return Some(());
    }
    let reply = fetch::get(&update.url, "", 64 << 20)?;
    // Only the release's exact bytes are written, so nothing partial or
    // altered can run.
    if reply.status != 200 || fetch::sha256(&reply.body)? != update.sha256 {
        return None;
    }
    fs::create_dir_all(dir).ok()?;
    let installer = dir.join(&update.name);
    fs::write(&installer, &reply.body).ok()?;
    fs::write(
        dir.join(MARKER),
        format!("{} {}", update.version, unix_now()),
    )
    .ok()?;
    Command::new(installer)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
        .spawn()
        .ok()
        .map(drop)
}

/// One scheduled look at the latest release, installing it when it is newer.
/// Returns how long to wait before the next.
fn check(dir: &Path, current: Version) -> Duration {
    let headers = "Accept: application/vnd.github+json\r\n";
    let Some(reply) = fetch::get(RELEASE_URL, headers, 1 << 20) else {
        return RETRY;
    };
    if reply.status != 200 {
        // GitHub's unauthenticated rate limit resets within the hour, so
        // waiting RETRY respects it unless GitHub asks for longer.
        let asked = Duration::from_secs(reply.retry_after.unwrap_or(0).into());
        return RETRY.max(asked.min(MAX_WAIT));
    }
    match assess(current, &reply.body) {
        Some(update) if install(dir, &update).is_none() => RETRY,
        _ => PERIOD,
    }
}

/// Clears the installers an earlier run staged. The attempt record stays, so
/// a failed version's backoff survives a restart.
fn tidy(dir: &Path) {
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
    let Some(dir) = installed() else {
        return;
    };
    let Some(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        return;
    };
    let _ = thread::Builder::new()
        .name("updates".into())
        .spawn(move || {
            // Waiting first lets the installer that started this copy finish,
            // so its own file can go.
            thread::sleep(FIRST_CHECK);
            loop {
                tidy(&dir);
                thread::sleep(check(&dir, current));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: Version = Version(1, 0, 0);
    const HEX: &str = "27e1fc5bebb00238838c42c2ed1f4cd27445e958a41e9e89c7db9637657d4f25";

    fn release(tag: &str, asset: &str) -> Vec<u8> {
        format!(r#"{{"tag_name":"{tag}","assets":[{asset}]}}"#).into_bytes()
    }

    fn asset(name: &str, state: &str, digest: &str) -> String {
        format!(
            r#"{{"name":"{name}","state":"{state}","digest":"{digest}","browser_download_url":"https://evil.example/{name}"}}"#
        )
    }

    fn good_asset() -> String {
        asset(
            "Portside-1.0.1-x64-setup.exe",
            "uploaded",
            &format!("sha256:{HEX}"),
        )
    }

    #[test]
    fn versions_are_three_plain_numbers() {
        assert_eq!(Version::parse("1.0.10"), Some(Version(1, 0, 10)));
        for text in ["1.0", "1.0.0.0", "1.0.0-rc1", "+1.0.0"] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn accepts_a_newer_release_and_builds_its_own_url() {
        let name = "Portside-1.0.1-x64-setup.exe";
        assert_eq!(
            assess(CURRENT, &release("v1.0.1", &good_asset())),
            Some(Update {
                version: Version(1, 0, 1),
                name: name.into(),
                url: format!("{DOWNLOAD_URL}/v1.0.1/{name}"),
                sha256: HEX.into(),
            })
        );
    }

    #[test]
    fn ignores_older_equal_and_malformed_releases() {
        let name = "Portside-1.0.1-x64-setup.exe";
        let digest = format!("sha256:{HEX}");
        let upper = format!("sha256:{}", HEX.to_uppercase());
        for body in [
            release("v1.0.0", &good_asset()),
            release("v0.9.9", &good_asset()),
            release("v1.0.1", ""),
            release(
                "v1.0.1",
                &asset("Portside-1.0.1-arm64-setup.exe", "uploaded", &digest),
            ),
            release("v1.0.1", &asset(name, "open", &digest)),
            release("v1.0.1", &asset(name, "uploaded", "sha256:abc")),
            release("v1.0.1", &asset(name, "uploaded", &upper)),
            release("1.0.1", &good_asset()),
            release("v1.0.1-beta.1", &good_asset()),
            br#"{"tag_name":"v1.0.1"}"#.to_vec(),
            b"<html>Sign in to the network</html>".to_vec(),
        ] {
            assert_eq!(
                assess(CURRENT, &body),
                None,
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    #[test]
    fn a_version_is_handed_to_the_installer_once_per_backoff() {
        let backoff = BACKOFF.as_secs();
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
    fn tidy_clears_staged_files_but_keeps_the_attempt() {
        let dir = env::temp_dir().join(format!("portside-update-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Portside-1.0.1-x64-setup.exe"), b"x").unwrap();
        fs::write(dir.join(MARKER), "1.0.2 1000").unwrap();
        tidy(&dir);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(read_attempt(&dir), Some((Version(1, 0, 2), 1000)));
        fs::write(dir.join(MARKER), "garbage").unwrap();
        assert_eq!(read_attempt(&dir), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
