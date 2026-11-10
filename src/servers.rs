//! Dev servers: TCP ports listening on localhost (or on every address) whose
//! process is a dev runtime, named by that process's working directory.
//! Stopping one sends Ctrl+C to its console, then forces it. A saved command
//! starts one again with no window.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::windows::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    ptr::null_mut,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
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
/// How long a start gets to listen before its processes are ended.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How far up the parents a server's command is looked for. A reused process
/// ID can form a cycle.
const MAX_ANCESTORS: usize = 8;

/// Held while Portside is attached to a server's console, since a process can
/// be attached to only one console at a time.
static CONSOLE: Mutex<()> = Mutex::new(());

/// A command that starts a server, run from its working directory.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Launch {
    pub dir: String,
    pub exe: String,
    pub args: String,
    /// The command belongs to a listening server that starts this one.
    pub nested: bool,
}

impl Launch {
    pub fn name(&self) -> &str {
        file_name(self.dir.trim_end_matches('\\'))
    }
}

/// Ordered by port, then name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Server {
    pub port: u16,
    pub name: String,
    pub address: IpAddr,
    /// None when the process's memory can't be read (an elevated one).
    pub launch: Option<Launch>,
}

impl Server {
    pub fn in_folder(&self, dir: &str) -> bool {
        self.launch
            .as_ref()
            .is_some_and(|launch| launch.dir.eq_ignore_ascii_case(dir))
    }

    /// Whether this is the server saved as `launch` on `port`: the same port,
    /// or the same command on another, since Vite moves ports. `nested` is part
    /// of the command, so Tina's own :4001 never counts as Astro's :4321.
    pub fn runs(&self, launch: &Launch, port: u16) -> bool {
        let same_command = |own: &Launch| {
            (&own.exe, &own.args, own.nested) == (&launch.exe, &launch.args, launch.nested)
        };
        self.in_folder(&launch.dir)
            && (self.port == port || self.launch.as_ref().is_some_and(same_command))
    }

    pub fn url(&self, shared: bool) -> String {
        if !shared && (self.address.is_ipv6() || self.address.is_unspecified()) {
            return format!("http://localhost:{}", self.port);
        }
        let address = match self.address {
            IpAddr::V4(address) if address.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(address) if address.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            address => address,
        };
        format!("http://{}", SocketAddr::new(address, self.port))
    }
}

/// Remembers each (pid, port) pair's lookup, so a poll only opens new processes.
#[derive(Default)]
pub struct Scanner {
    known: HashMap<(u32, u16), Option<Server>>,
}

impl Scanner {
    pub fn scan(&mut self) -> Vec<Server> {
        let listeners = listeners();
        self.known
            .retain(|&(pid, port), _| listeners.iter().any(|l| (l.0, l.1) == (pid, port)));
        let mut servers: Vec<Server> = listeners
            .iter()
            .filter_map(|&(pid, port, address)| {
                let known = self.known.entry((pid, port)).or_insert_with(|| {
                    let (name, launch) = project(pid, &listeners)?;
                    Some(Server {
                        port,
                        name,
                        address,
                        launch,
                    })
                });
                Some(Server {
                    address,
                    ..known.clone()?
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
        for (&(pid, port), known) in &self.known {
            if port == server.port && known.as_ref().is_some_and(|k| k.name == server.name) {
                thread::spawn(move || stop_process(pid, port));
            }
        }
    }
}

/// A saved command that has not listened yet.
pub struct Starting {
    pub launch: Launch,
    pub port: u16,
    child: Child,
    since: Instant,
}

impl Starting {
    pub fn expired(&self) -> bool {
        self.since.elapsed() >= START_TIMEOUT
    }

    /// Holding `child` through taskkill keeps its ID from being reused.
    pub fn end(self) {
        thread::spawn(move || taskkill(self.child.id()));
    }
}

/// Runs a saved command with no window. Its hidden console still takes Ctrl+C.
pub fn start(launch: &Launch, port: u16) -> Option<Starting> {
    let mut command = Command::new(&launch.exe);
    command
        .raw_arg(&launch.args)
        .current_dir(&launch.dir)
        .env("PATH", search_path(launch))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A child inherits Portside's ignoring of Ctrl+C. The lock keeps that
    // from lapsing while Portside is attached to a server's console.
    let _attached = CONSOLE.lock();
    unsafe { SetConsoleCtrlHandler(None, 0) };
    let child = command.spawn();
    unsafe { SetConsoleCtrlHandler(None, 1) };
    Some(Starting {
        launch: launch.clone(),
        port,
        child: child.ok()?,
        since: Instant::now(),
    })
}

/// PATH with every `node_modules\.bin` from the project folder upward and the
/// exe's folder in front, so tools run by name resolve as under `npm run`.
fn search_path(launch: &Launch) -> OsString {
    let path = env::var_os("PATH").unwrap_or_default();
    let folders = Path::new(&launch.dir)
        .ancestors()
        .map(|folder| folder.join(r"node_modules\.bin"))
        .chain(Path::new(&launch.exe).parent().map(Path::to_path_buf))
        .chain(env::split_paths(&path));
    env::join_paths(folders).unwrap_or(path)
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
        taskkill(pid);
    }
}

/// Ends `pid` and every process it started. No standard handles, which another
/// stop may have swapped for its server's console.
fn taskkill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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

/// Every (pid, port, address) listening on a loopback or wildcard address. A
/// process listening on both families counts once, as IPv4.
fn listeners() -> Vec<(u32, u16, IpAddr)> {
    let mut found = Vec::new();
    for row in rows::<MIB_TCPROW_OWNER_PID>(&table(AF_INET)) {
        let address = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
        if address.is_loopback() || address.is_unspecified() {
            found.push((
                row.dwOwningPid,
                u16::from_be(row.dwLocalPort as u16),
                address.into(),
            ));
        }
    }
    for row in rows::<MIB_TCP6ROW_OWNER_PID>(&table(AF_INET6)) {
        let address = Ipv6Addr::from(row.ucLocalAddr);
        if address.is_loopback() || address.is_unspecified() {
            found.push((
                row.dwOwningPid,
                u16::from_be(row.dwLocalPort as u16),
                address.into(),
            ));
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

/// The project folder of a dev runtime and its command, or None for any other
/// process. A server whose memory can't be read (an elevated one) shows its
/// runtime name.
fn project(pid: u32, listeners: &[(u32, u16, IpAddr)]) -> Option<(String, Option<Launch>)> {
    let image = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?.image()?;
    let exe = file_name(&image).to_ascii_lowercase();
    if !RUNTIMES.iter().any(|runtime| exe.starts_with(runtime)) {
        return None;
    }
    let launch = launch(pid, listeners);
    let name = launch
        .as_ref()
        .map_or(exe.trim_end_matches(".exe"), Launch::name)
        .to_owned();
    // A saved command is stored on one line.
    Some((
        name,
        launch.filter(|launch| !launch.args.contains(['\r', '\n'])),
    ))
}

/// The command that starts the server `pid`. A parent with the same exe name
/// and working directory is a wrapper around it, such as a venv's python.exe,
/// so the topmost one's command is taken. So is that of an ancestor that is
/// listening itself, as Tina starts Astro through cmd.exe, and the server is
/// then marked `nested`.
fn launch(pid: u32, listeners: &[(u32, u16, IpAddr)]) -> Option<Launch> {
    let read = |pid| {
        let process = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
        Some((process.image()?, process.parameters()?))
    };
    let (exe, (dir, line, mut parent)) = read(pid)?;
    let mut launch = Launch {
        dir,
        exe,
        args: arguments(&line).to_owned(),
        nested: false,
    };
    let mut unbroken = true;
    for _ in 0..MAX_ANCESTORS {
        let Some((exe, (dir, line, next))) = read(parent) else {
            break;
        };
        let same = file_name(&exe).eq_ignore_ascii_case(file_name(&launch.exe))
            && dir.eq_ignore_ascii_case(&launch.dir);
        let listening = listeners.iter().any(|l| l.0 == parent);
        unbroken &= same;
        if same && (unbroken || listening) {
            launch.exe = exe;
            launch.args = arguments(&line).to_owned();
            launch.nested |= listening;
        }
        parent = next;
    }
    Some(launch)
}

/// A command line without argv[0].
fn arguments(line: &str) -> &str {
    let rest = match line.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"').map_or("", |(_, rest)| rest),
        None => line.find([' ', '\t']).map_or("", |at| &line[at..]),
    };
    rest.trim_start_matches([' ', '\t'])
}

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

struct Process(HANDLE);

impl Process {
    fn open(pid: u32, access: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then(|| Self(handle))
    }

    fn image(&self) -> Option<String> {
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(self.0, 0, path.as_mut_ptr(), &mut length) };
        (ok != 0).then(|| String::from_utf16_lossy(&path[..length as usize]))
    }

    /// The current directory, command line and parent process ID. PEB+0x20
    /// points to the parameters, whose CurrentDirectory and CommandLine
    /// UNICODE_STRINGs sit at +0x38 and +0x70.
    fn parameters(&self) -> Option<(String, String, u32)> {
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
        Some((
            self.string(parameters + 0x38)?,
            self.string(parameters + 0x70)?,
            basic[5] as u32,
        ))
    }

    /// The UNICODE_STRING at `address`.
    fn string(&self, address: usize) -> Option<String> {
        let [lengths, buffer] = self.read::<2>(address)?;
        let mut text = vec![0u16; (lengths & 0xffff) / 2];
        let ok = unsafe {
            ReadProcessMemory(
                self.0,
                buffer as *const _,
                text.as_mut_ptr().cast(),
                text.len() * 2,
                null_mut(),
            )
        };
        (ok != 0).then(|| String::from_utf16_lossy(&text))
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
    fn opens_listeners_at_the_expected_host() {
        for (listen, single, shared) in [
            ("127.0.0.1:0", "127.0.0.1", "127.0.0.1"),
            ("127.0.0.2:0", "127.0.0.2", "127.0.0.2"),
            ("[::1]:0", "localhost", "[::1]"),
            ("0.0.0.0:0", "localhost", "127.0.0.1"),
            ("[::]:0", "localhost", "[::1]"),
        ] {
            let listener = TcpListener::bind(listen).unwrap();
            let port = listener.local_addr().unwrap().port();
            let (_, _, address) = super::listeners()
                .into_iter()
                .find(|&(pid, found, _)| pid == std::process::id() && found == port)
                .unwrap();
            let server = super::Server {
                port,
                name: "test".into(),
                address,
                launch: None,
            };
            assert_eq!(
                server.url(false),
                format!("http://{single}:{port}"),
                "{listen}"
            );
            assert_eq!(
                server.url(true),
                format!("http://{shared}:{port}"),
                "{listen}"
            );
        }
    }

    #[test]
    fn strips_the_program_from_a_command_line() {
        for (line, args) in [
            (r#""C:\node.exe"  vite.js --port 1"#, "vite.js --port 1"),
            ("node vite.js", "vite.js"),
            ("node\tvite.js", "vite.js"),
            ("node", ""),
            (r#""C:\node.exe vite.js"#, ""),
        ] {
            assert_eq!(super::arguments(line), args, "{line}");
        }
    }

    #[test]
    fn finds_loopback_and_wildcard_listeners() {
        let pid = std::process::id();
        for address in ["127.0.0.1:0", "0.0.0.0:0", "[::1]:0", "[::]:0"] {
            let listener = TcpListener::bind(address).unwrap();
            let socket = listener.local_addr().unwrap();
            assert!(
                super::listeners().contains(&(pid, socket.port(), socket.ip())),
                "{address}"
            );
        }
    }
}
