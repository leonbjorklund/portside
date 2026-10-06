use crate::wide;
use std::{
    env,
    os::windows::process::CommandExt,
    path::Path,
    process::{Child, Command},
    ptr, thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND,
        WAIT_OBJECT_0,
    },
    System::{
        JobObjects::{
            IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        },
        Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CreateMutexW, GetCurrentProcess,
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            QueryFullProcessImageNameW, WaitForSingleObject,
        },
    },
    UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE},
};

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

pub fn close_existing() -> Result<(), String> {
    let Some((hwnd, pid)) = running() else {
        return Instance::acquire()?
            .map(|_| ())
            .ok_or_else(|| "Portside is starting. Try again after it has started.".into());
    };
    unsafe {
        let process = OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if process.is_null() {
            return Err("Could not track the running Portside's shutdown.".into());
        }
        // A window class alone is not proof that the process is Portside.
        let mut image = [0u16; 32768];
        let mut length = image.len() as u32;
        let identified = QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length)
            != 0
            && Path::new(&String::from_utf16_lossy(&image[..length as usize]))
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("portside.exe"));
        if !identified {
            CloseHandle(process);
            return Err("Could not identify the running Portside.".into());
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

pub fn start() -> Result<(), String> {
    let target =
        env::current_exe().map_err(|e| format!("Could not locate this executable: {e}"))?;
    let mut child = spawn_detached(&target)?;
    if let Err(error) = verify_started(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(())
}

/// Tool hosts (terminals, agent shells) often run their children inside a job
/// object. Portside must outlive whatever launched the installer, so leave
/// that job when the host permits it. When breakaway is forbidden, a plain
/// spawn is fine unless the host's job kills its children on close: then
/// Portside would die with the shell, so refuse to launch it there.
fn spawn_detached(target: &Path) -> Result<Child, String> {
    match Command::new(target)
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            if job_kills_on_close() {
                return Err("This shell keeps its child processes in a job that closes them with the shell and forbids breakaway. Run Portside from a regular terminal.".into());
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
            return Err("The installed Portside did not create its window.".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
