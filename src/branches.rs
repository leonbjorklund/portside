//! Current checkout labels for project folders, read off the UI thread.

use std::{
    env,
    io::Read,
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Command, ExitStatus, Stdio},
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_BROKEN_PIPE, GetLastError},
    System::{Pipes::PeekNamedPipe, Threading::CREATE_NO_WINDOW},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);
const MAX_OUTPUT: usize = 64 * 1024;

type Entries = Vec<(String, String)>;

pub struct Branches {
    entries: Entries,
    requests: Option<Sender<Vec<String>>>,
    results: Receiver<Entries>,
    pending: bool,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Branches {
    pub fn new() -> Self {
        let (requests, folders) = mpsc::channel::<Vec<String>>();
        let (results, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while let Ok(dirs) = folders.recv() {
                let mut entries = Vec::new();
                for dir in dirs {
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(branch) = read(&dir, &cancelled) {
                        entries.push((dir, branch));
                    }
                }
                if results.send(entries).is_err() {
                    break;
                }
            }
        });
        Self {
            entries: Vec::new(),
            requests: Some(requests),
            results: receiver,
            pending: false,
            stop,
            worker: Some(worker),
        }
    }

    /// Takes completed reads and requests another batch without waiting.
    pub fn refresh(&mut self, mut dirs: Vec<String>) -> bool {
        dirs.sort_by_cached_key(|dir| dir.to_ascii_lowercase());
        dirs.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let tracked = |dir: &str| dirs.iter().any(|d| d.eq_ignore_ascii_case(dir));
        let before = self.entries.len();
        self.entries.retain(|(dir, _)| tracked(dir));
        let mut changed = before != self.entries.len();
        if let Ok(mut entries) = self.results.try_recv() {
            self.pending = false;
            entries.retain(|(dir, _)| tracked(dir));
            changed |= entries != self.entries;
            self.entries = entries;
        }
        if !self.pending && !dirs.is_empty() {
            self.pending = self
                .requests
                .as_ref()
                .is_some_and(|requests| requests.send(dirs).is_ok());
        }
        changed
    }

    pub fn get(&self, dir: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(folder, _)| folder.eq_ignore_ascii_case(dir))
            .map(|(_, branch)| branch.as_str())
    }
}

impl Drop for Branches {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read(dir: &str, stop: &AtomicBool) -> Option<String> {
    let deadline = Instant::now() + TIMEOUT;
    let run = |args: &[&str]| {
        let (status, bytes) = output(git(dir).args(args), stop, deadline)?;
        Some((status, String::from_utf8(bytes).ok()?))
    };
    let (status, text) = run(&["symbolic-ref", "--quiet", "HEAD"])?;
    if status.success() {
        let branch = text
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("refs/heads/")?;
        return (!branch.is_empty()).then(|| branch.to_owned());
    }
    if status.code() != Some(1) {
        return None;
    }
    let (status, text) = run(&[
        "for-each-ref",
        "--points-at=HEAD",
        "--count=2",
        "--format=%(refname:strip=2)",
        "refs/heads/",
    ])?;
    if !status.success() {
        return None;
    }
    let mut names = text.lines();
    Some(match (names.next(), names.next()) {
        (Some(name), None) if !name.is_empty() => name.to_owned(),
        _ => "detached".to_owned(),
    })
}

fn git(dir: &str) -> Command {
    let mut command = Command::new("git");
    command.args(["-C", dir]);
    // An agent or terminal may have selected a different repository through
    // its environment. The project's folder is the sole source here.
    for (key, _) in env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
}

/// Drains available pipe bytes while checking both deadline and cancellation.
/// Waiting for exit before reading can deadlock if stdout fills the pipe.
fn output(
    command: &mut Command,
    stop: &AtomicBool,
    deadline: Instant,
) -> Option<(ExitStatus, Vec<u8>)> {
    if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return None;
    }
    let mut child = command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut bytes = Vec::new();
    let result = loop {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break None;
        }
        // Check exit before the pipe so the final read includes all output.
        let Ok(status) = child.try_wait() else {
            break None;
        };
        let mut available = 0;
        let peeked = unsafe {
            PeekNamedPipe(
                stdout.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        };
        if peeked == 0 && unsafe { GetLastError() } != ERROR_BROKEN_PIPE {
            break None;
        }
        if available > 0 {
            let mut buffer = [0; 4096];
            let length = (available as usize).min(buffer.len());
            if bytes.len() + length > MAX_OUTPUT
                || stdout.read_exact(&mut buffer[..length]).is_err()
            {
                break None;
            }
            bytes.extend_from_slice(&buffer[..length]);
        } else if let Some(status) = status {
            break Some((status, bytes));
        } else {
            thread::sleep(POLL);
        }
    };
    if result.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, time::SystemTime};

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let repo = Self(
                env::temp_dir().join(format!("portside-branches-{}-{unique}", std::process::id())),
            );
            fs::create_dir(&repo.0).unwrap();
            repo.git(&["init", "--initial-branch=main"]);
            repo
        }

        fn dir(&self) -> String {
            self.0.to_str().unwrap().to_owned()
        }

        fn git(&self, args: &[&str]) {
            let output = git(&self.dir())
                .env("GIT_CONFIG_GLOBAL", "NUL")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args([
                    "-c",
                    "user.name=Portside Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn reads_named_branches_in_nested_folders_and_linked_worktrees() {
        let repo = Repo::new();
        let stop = AtomicBool::new(false);
        let nested = repo.0.join("nested");
        fs::create_dir(&nested).unwrap();
        assert_eq!(
            read(nested.to_str().unwrap(), &stop).as_deref(),
            Some("main")
        );
        repo.git(&["commit", "--allow-empty", "-m", "initial"]);
        let worktree = repo.0.join("worktree");
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "feature/menü",
            worktree.to_str().unwrap(),
        ]);
        assert_eq!(
            read(worktree.to_str().unwrap(), &stop).as_deref(),
            Some("feature/menü")
        );
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("main"));
        repo.git(&["switch", "--detach"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("detached"));
        assert_eq!(read(repo.0.join("missing").to_str().unwrap(), &stop), None);
        assert_eq!(read(env::temp_dir().to_str().unwrap(), &stop), None);
    }

    #[test]
    fn names_detached_checkouts_only_for_one_exact_local_branch() {
        let repo = Repo::new();
        let stop = AtomicBool::new(false);
        repo.git(&["commit", "--allow-empty", "-m", "initial"]);
        repo.git(&["commit", "--allow-empty", "-m", "newer"]);
        repo.git(&["switch", "--detach", "HEAD~1"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("detached"));

        repo.git(&["update-ref", "refs/remotes/origin/design-work", "HEAD"]);
        repo.git(&["tag", "saved-version"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("detached"));

        repo.git(&["branch", "feature/menü"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("feature/menü"));
        repo.git(&["branch", "another"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("detached"));
        repo.git(&["branch", "-D", "another"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("feature/menü"));

        // Moving the matching branch changes the label without moving HEAD.
        repo.git(&["branch", "-f", "feature/menü", "main"]);
        assert_eq!(read(&repo.dir(), &stop).as_deref(), Some("detached"));
    }

    #[test]
    fn refreshes_branches_and_discards_removed_folders() {
        let repo = Repo::new();
        let mut branches = Branches::new();
        let dirs = vec![repo.dir(), repo.dir().to_ascii_uppercase()];
        let wait = |branches: &mut Branches, expected: Option<&str>| {
            let started = Instant::now();
            while branches.get(&repo.dir()) != expected {
                branches.refresh(dirs.clone());
                assert!(started.elapsed() < Duration::from_secs(10));
                thread::sleep(POLL);
            }
        };
        wait(&mut branches, Some("main"));
        repo.git(&["symbolic-ref", "HEAD", "refs/heads/feature/menu"]);
        wait(&mut branches, Some("feature/menu"));
        let head = repo.0.join(".git/HEAD");
        let backup = repo.0.join(".git/HEAD.before");
        fs::rename(&head, &backup).unwrap();
        wait(&mut branches, None);
        fs::rename(&backup, &head).unwrap();
        wait(&mut branches, Some("feature/menu"));
        assert!(branches.refresh(Vec::new()));
        assert_eq!(branches.get(&repo.dir()), None);
        // A result already in flight must not restore a removed folder.
        while branches.pending {
            branches.refresh(Vec::new());
            thread::sleep(POLL);
        }
        assert_eq!(branches.get(&repo.dir()), None);
    }

    #[test]
    fn bounds_output_and_cancels_slow_commands() {
        for length in [MAX_OUTPUT, MAX_OUTPUT + 1] {
            let mut command = Command::new("powershell.exe");
            command
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(format!("[Console]::Write('x' * {length})"));
            let bytes = output(
                &mut command,
                &AtomicBool::new(false),
                Instant::now() + TIMEOUT,
            )
            .map(|(_, bytes)| bytes.len());
            assert_eq!(bytes, (length == MAX_OUTPUT).then_some(MAX_OUTPUT));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 10",
            ]);
            output(&mut command, &cancel, Instant::now() + TIMEOUT)
        });
        thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let started = Instant::now();
        assert!(worker.join().unwrap().is_none());
        assert!(started.elapsed() < TIMEOUT);

        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 10",
        ]);
        let started = Instant::now();
        assert!(output(&mut command, &AtomicBool::new(false), started + TIMEOUT).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
