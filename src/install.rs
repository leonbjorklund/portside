use crate::wide;
use std::{
    env, fs,
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    ptr, thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError,
        HANDLE, HWND, WAIT_OBJECT_0,
    },
    System::{
        JobObjects::{
            IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        },
        Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW},
        Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CreateMutexW, GetCurrentProcess,
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
    UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE},
};

unsafe extern "C" {
    fn portside_install_shortcut(executable: *const u16) -> i32;
}

pub struct Instance(HANDLE);
impl Instance {
    pub fn acquire() -> Result<Option<Self>, String> {
        // Freed only after GetLastError, which freeing could reset.
        let name = wide("Portside");
        unsafe {
            let handle = CreateMutexW(ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err("Could not create Portside's single-instance lock.".into());
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                Ok(None)
            } else {
                Ok(Some(Self(handle)))
            }
        }
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The running copy's menu window and process, found by class name.
fn running() -> Option<(HWND, u32)> {
    unsafe {
        let hwnd = FindWindowW(wide(crate::ui::CLASS_NAME).as_ptr(), ptr::null());
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        Some((hwnd, pid))
    }
}

fn close_existing() -> Result<(), String> {
    let Some((hwnd, pid)) = running() else {
        return Ok(());
    };
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return Err("Could not track the running Portside's shutdown.".into());
        }
        if PostMessageW(hwnd, WM_CLOSE, 0, 0) == 0 {
            CloseHandle(process);
            return Err("Could not ask the running Portside to exit.".into());
        }
        // The window disappears before the mutex is released. Wait for the
        // exact owner process, including same-path reinstalls.
        let exited = WaitForSingleObject(process, 5000) == WAIT_OBJECT_0;
        CloseHandle(process);
        if !exited {
            return Err("The running Portside did not exit. Close it and try again.".into());
        }
    }
    Ok(())
}

/// Packaged apps such as Codex and Claude Desktop redirect per-user app data
/// and HKCU writes into their own container. Installing from a shell inside
/// one of them puts the executable and the Run value where Windows sign-in
/// never sees them. The redirected shape is `Packages\<package>\LocalCache`.
fn package_redirected(path: &Path) -> bool {
    let names: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    names
        .windows(3)
        .any(|run| run[0] == "packages" && run[2] == "localcache")
}

/// Child shells of a packaged app can be redirected while LOCALAPPDATA still
/// names the real folder. A probe written there then lands in a package's
/// `LocalCache\Local` instead.
fn writes_redirected(local: &Path) -> bool {
    let probe = format!("portside-probe-{}", std::process::id());
    if fs::write(local.join(&probe), b"").is_err() {
        return false;
    }
    let found = fs::read_dir(local.join("Packages"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|package| {
            package
                .path()
                .join("LocalCache\\Local")
                .join(&probe)
                .exists()
        });
    let _ = fs::remove_file(local.join(&probe));
    found
}

pub fn install() -> Result<(), String> {
    let directory = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("Windows did not provide LOCALAPPDATA.")?;
    let roaming = env::var_os("APPDATA").map(PathBuf::from);
    if package_redirected(&directory)
        || roaming.as_deref().is_some_and(package_redirected)
        || writes_redirected(&directory)
    {
        return Err("This shell runs inside a packaged app (for example Codex or Claude Desktop), so Windows would redirect the installation into that app's container. Run --install from a regular terminal.".into());
    }
    let directory = directory.join("Portside");
    let source =
        env::current_exe().map_err(|e| format!("Could not locate this executable: {e}"))?;
    let target = directory.join("portside.exe");
    let command = format!("\"{}\"", target.display());
    // Validate the Run command before making any changes.
    if command.encode_utf16().count() > 260 {
        return Err("The installation path is too long for Windows startup.".into());
    }
    fs::create_dir_all(&directory)
        .map_err(|e| format!("Could not create the installation folder: {e}"))?;
    fs::write(
        directory.join("AtkinsonHyperlegible-OFL.txt"),
        include_bytes!("../assets/fonts/atkinsonhyperlegible/OFL.txt"),
    )
    .map_err(|e| format!("Could not install the bundled font license: {e}"))?;
    let same = match (fs::canonicalize(&source), fs::canonicalize(&target)) {
        (Ok(source), Ok(target)) => source == target,
        _ => false,
    };
    let staging = directory.join("portside.new.exe");
    if !same {
        fs::copy(&source, &staging).map_err(|e| format!("Could not stage Portside: {e}"))?;
    }
    close_existing()?;
    let reservation =
        Instance::acquire()?.ok_or("Another Portside is running. Exit it and try again.")?;
    if !same {
        // Windows can release the old image a moment after its window closes.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::rename(&staging, &target) {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(format!("Could not replace the installed Portside: {e}")),
            }
        }
    }
    // The installed child must acquire the shared mutex itself.
    drop(reservation);
    let mut child = spawn_detached(&target)?;
    if let Err(error) = verify_started(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    // Preserve the previous Run value if the new process cannot stay up.
    let data = wide(&command);
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run").as_ptr(),
            wide("Portside").as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "Portside is running, but Windows startup could not be registered ({status})."
        ));
    }
    let executable: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let status = unsafe { portside_install_shortcut(executable.as_ptr()) };
    if status < 0 {
        return Err(format!(
            "Portside is installed, but its Start menu shortcut could not be created (0x{:08X}).",
            status as u32
        ));
    }
    Ok(())
}

/// Tool hosts (terminals, agent shells) often run their children inside a job
/// object. Portside must outlive whatever launched the installer, so leave
/// that job when the host permits it. When breakaway is forbidden, a plain
/// spawn is fine unless the host's job kills its children on close: then
/// Portside would die with the shell, so refuse before startup is registered.
fn spawn_detached(target: &Path) -> Result<Child, String> {
    match Command::new(target)
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            if job_kills_on_close() {
                return Err("This shell keeps its child processes in a job that closes them with the shell and forbids breakaway. Run --install from a regular terminal.".into());
            }
            Command::new(target).creation_flags(CREATE_NO_WINDOW).spawn()
        }
        result => result,
    }
    .map_err(|e| format!("Installed, but could not start Portside: {e}"))
}

/// Whether the installer's own job has JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE set.
fn job_kills_on_close() -> bool {
    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &mut in_job) == 0 || in_job == 0 {
            return false;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        QueryInformationJobObject(
            ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
            ptr::null_mut(),
        ) != 0
            && info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0
    }
}

fn verify_started(child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut window_since = None;
    loop {
        if child
            .try_wait()
            .map_err(|e| format!("Could not verify the installed Portside: {e}"))?
            .is_some()
        {
            return Err("The installed Portside stopped before startup could be verified. Another copy may still be running.".into());
        }
        if running().is_some_and(|(_, pid)| pid == child.id()) {
            let since = window_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(500) {
                return Ok(());
            }
        } else {
            window_since = None;
        }
        if Instant::now() >= deadline {
            return Err("The installed Portside did not create its window. Startup registration was not changed.".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::package_redirected;
    use std::path::Path;

    #[test]
    fn detects_package_redirected_app_data() {
        assert!(package_redirected(Path::new(
            r"C:\Users\Leon\AppData\Local\Packages\OpenAI.Codex_2p2nqsd0c76g0\LocalCache\Local"
        )));
        assert!(package_redirected(Path::new(
            r"C:\Users\Leon\AppData\Local\packages\X\LocalCache\Roaming"
        )));
        assert!(!package_redirected(Path::new(
            r"C:\Users\Leon\AppData\Local"
        )));
        assert!(!package_redirected(Path::new(
            r"C:\Users\Leon\AppData\Roaming"
        )));
        assert!(!package_redirected(Path::new(
            r"C:\Users\Leon\MyPackages\Local"
        )));
        assert!(!package_redirected(Path::new(
            r"C:\Users\Leon\Packages\Local"
        )));
    }
}
