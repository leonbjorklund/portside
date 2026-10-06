//! Dev servers: TCP ports listening on localhost (or on every address) whose
//! process is a dev runtime, named by that process's working directory.
//! Stopping one sends Ctrl+C to its console, then forces it.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr},
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    ptr::null_mut,
    sync::Mutex,
    thread,
};
use windows_sys::{
    Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation},
    Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR, WAIT_OBJECT_0},
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
            TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::{AF_INET, AF_INET6},
        System::{
            Console::{
                AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, GetStdHandle,
                STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
                SetStdHandle,
            },
            Diagnostics::Debug::ReadProcessMemory,
            Threading::{
                CREATE_NO_WINDOW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, PROCESS_VM_READ, QueryFullProcessImageNameW,
                WaitForSingleObject,
            },
        },
    },
};

/// Executable name prefixes, so `pythonw` and `python3.12` count too.
const RUNTIMES: [&str; 4] = ["node", "bun", "deno", "python"];
/// How long a server gets to exit after Ctrl+C before it is forced.
const STOP_WAIT_MS: u32 = 2000;

/// Held while Portside is attached to a server's console, since a process can
/// be attached to only one console at a time.
static CONSOLE: Mutex<()> = Mutex::new(());

/// Ordered by port, then name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Server {
    pub port: u16,
    pub name: String,
    /// Listening on IPv6, which decides its URL when another server holds
    /// the same port on IPv4.
    pub ipv6: bool,
}

/// Remembers each (pid, port) pair's lookup, so a poll only opens new processes.
#[derive(Default)]
pub struct Scanner {
    known: HashMap<(u32, u16), Option<String>>,
}

impl Scanner {
    pub fn scan(&mut self) -> Vec<Server> {
        let listeners = listeners();
        self.known
            .retain(|&(pid, port), _| listeners.iter().any(|l| (l.0, l.1) == (pid, port)));
        let mut servers: Vec<Server> = listeners
            .into_iter()
            .filter_map(|(pid, port, ipv6)| {
                let name = self
                    .known
                    .entry((pid, port))
                    .or_insert_with(|| project(pid));
                Some(Server {
                    port,
                    name: name.clone()?,
                    ipv6,
                })
            })
            .collect();
        servers.sort();
        // One project listening on both IPv4 and IPv6 from two processes.
        servers.dedup_by(|a, b| (a.port, &a.name) == (b.port, &b.name));
        servers
    }

    /// Stops a server from the last scan, each of its processes on its own
    /// thread: one project can listen on IPv4 and IPv6 from two.
    pub fn stop(&self, server: &Server) {
        for (&(pid, port), name) in &self.known {
            if port == server.port && name.as_ref() == Some(&server.name) {
                thread::spawn(move || stop_process(pid, port));
            }
        }
    }
}

/// Ctrl+C first, as if typed in the server's terminal. A server without a
/// console, or still running `STOP_WAIT_MS` later, is ended along with every
/// process it started.
fn stop_process(pid: u32, port: u16) {
    // The server may have exited since the last scan and its ID gone to
    // another process. The open handle keeps the ID from being reused.
    let Some(process) = Process::open(pid, PROCESS_SYNCHRONIZE) else {
        return;
    };
    if !listeners().iter().any(|l| (l.0, l.1) == (pid, port)) {
        return;
    }
    let exited =
        ctrl_c(pid) && unsafe { WaitForSingleObject(process.0, STOP_WAIT_MS) } == WAIT_OBJECT_0;
    if !exited {
        // No standard handles, which another stop may have swapped for its
        // server's console.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Sends Ctrl+C to every process in `pid`'s console, Portside included while
/// it is attached. Portside ignores Ctrl+C: attaching and detaching reset
/// handler functions, but not that setting.
fn ctrl_c(pid: u32) -> bool {
    let _attached = CONSOLE.lock();
    let ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    unsafe {
        SetConsoleCtrlHandler(None, 1);
        // Attaching swaps the standard handles for the console's and detaching
        // closes them, which would make every later process spawn fail.
        let saved = ids.map(|id| GetStdHandle(id));
        if AttachConsole(pid) == 0 {
            return false;
        }
        let sent = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
        FreeConsole();
        for (id, handle) in ids.into_iter().zip(saved) {
            SetStdHandle(id, handle);
        }
        sent
    }
}

/// Every (pid, port, IPv6) listening on a loopback or wildcard address. A
/// process listening on both families counts once, as IPv4.
fn listeners() -> Vec<(u32, u16, bool)> {
    let mut found = Vec::new();
    for row in rows::<MIB_TCPROW_OWNER_PID>(&table(AF_INET)) {
        let address = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
        if address.is_loopback() || address.is_unspecified() {
            found.push((row.dwOwningPid, u16::from_be(row.dwLocalPort as u16), false));
        }
    }
    for row in rows::<MIB_TCP6ROW_OWNER_PID>(&table(AF_INET6)) {
        let address = Ipv6Addr::from(row.ucLocalAddr);
        if address.is_loopback() || address.is_unspecified() {
            found.push((row.dwOwningPid, u16::from_be(row.dwLocalPort as u16), true));
        }
    }
    found.sort_unstable();
    found.dedup_by_key(|&mut (pid, port, _)| (pid, port));
    found
}

/// The listener table for one address family, as u32s so rows stay aligned.
fn table(family: u16) -> Vec<u32> {
    let mut buffer = vec![0u32; 1024];
    loop {
        let mut size = size_of_val(buffer.as_slice()) as u32;
        let status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &mut size,
                0,
                family as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match status {
            NO_ERROR => return buffer,
            ERROR_INSUFFICIENT_BUFFER => buffer.resize(size as usize / 4 + 64, 0),
            _ => return Vec::new(),
        }
    }
}

/// The rows after the table's leading entry count.
fn rows<T>(table: &[u32]) -> &[T] {
    match table.first() {
        Some(&count) => unsafe {
            std::slice::from_raw_parts(table[1..].as_ptr().cast(), count as usize)
        },
        None => &[],
    }
}

/// The project folder of a dev runtime, or None for any other process. A
/// server whose memory can't be read (an elevated one) shows its runtime name.
fn project(pid: u32) -> Option<String> {
    let image = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?.image()?;
    let exe = image.rsplit('\\').next()?.to_ascii_lowercase();
    if !RUNTIMES.iter().any(|runtime| exe.starts_with(runtime)) {
        return None;
    }
    let directory = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
        .and_then(|process| process.directory());
    Some(match directory {
        Some(path) => path.trim_end_matches('\\').rsplit('\\').next()?.to_owned(),
        None => exe.trim_end_matches(".exe").to_owned(),
    })
}

struct Process(HANDLE);

impl Process {
    fn open(pid: u32, access: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }

    fn image(&self) -> Option<String> {
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(self.0, 0, path.as_mut_ptr(), &mut length) };
        (ok != 0).then(|| String::from_utf16_lossy(&path[..length as usize]))
    }

    /// The current directory from the process's PEB: PEB+0x20 points to its
    /// parameters, whose CurrentDirectory UNICODE_STRING sits at +0x38.
    fn directory(&self) -> Option<String> {
        let mut basic = [0usize; 6];
        let status = unsafe {
            NtQueryInformationProcess(
                self.0,
                ProcessBasicInformation,
                basic.as_mut_ptr().cast(),
                size_of_val(&basic) as u32,
                null_mut(),
            )
        };
        if status != 0 {
            return None;
        }
        let [parameters] = self.read::<1>(basic[1] + 0x20)?;
        let [lengths, buffer] = self.read::<2>(parameters + 0x38)?;
        let mut path = vec![0u16; (lengths & 0xffff) / 2];
        let ok = unsafe {
            ReadProcessMemory(
                self.0,
                buffer as *const _,
                path.as_mut_ptr().cast(),
                path.len() * 2,
                null_mut(),
            )
        };
        (ok != 0).then(|| String::from_utf16_lossy(&path))
    }

    fn read<const N: usize>(&self, address: usize) -> Option<[usize; N]> {
        let mut value = [0usize; N];
        let ok = unsafe {
            ReadProcessMemory(
                self.0,
                address as *const _,
                value.as_mut_ptr().cast(),
                size_of_val(&value),
                null_mut(),
            )
        };
        (ok != 0).then_some(value)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    #[test]
    fn finds_loopback_and_wildcard_listeners() {
        let pid = std::process::id();
        for address in ["127.0.0.1:0", "0.0.0.0:0", "[::1]:0"] {
            let listener = TcpListener::bind(address).unwrap();
            let port = listener.local_addr().unwrap().port();
            let ipv6 = address.starts_with('[');
            assert!(super::listeners().contains(&(pid, port, ipv6)), "{address}");
        }
    }
}
