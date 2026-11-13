//! Dev servers: TCP ports listening on localhost (or on every address) whose
//! process is a dev runtime, named by the recovered launcher's working directory.
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
        Foundation::{
            CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, NO_ERROR, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
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
                CREATE_NO_WINDOW, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
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
    /// Another listening server sits between this server and its launcher.
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
    let launch = launch(pid, listeners, Process::info);
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
/// then marked `nested`. From that fallback, accepted cmd wrappers and package
/// runners can lead to the project's original command, even in a parent folder.
fn launch(
    pid: u32,
    listeners: &[(u32, u16, IpAddr)],
    mut read: impl FnMut(u32) -> Option<ProcessInfo>,
) -> Option<Launch> {
    let mut chain = vec![(pid, read(pid)?)];
    for _ in 0..MAX_ANCESTORS {
        let (_, child) = chain.last()?;
        let parent = child.parent;
        if parent == 0 || chain.iter().any(|&(pid, _)| pid == parent) {
            break;
        }
        let Some(info) = read(parent) else {
            break;
        };
        if info.created > child.created {
            break;
        }
        chain.push((parent, info));
    }

    let original = &chain[0].1.launch;
    let mut selected = 0;
    let mut nested = false;
    let mut unbroken = true;
    for (index, (parent, info)) in chain.iter().enumerate().skip(1) {
        let same = file_name(&info.launch.exe).eq_ignore_ascii_case(file_name(&original.exe))
            && info.launch.dir.eq_ignore_ascii_case(&original.dir);
        let listening = listeners.iter().any(|l| l.0 == *parent);
        unbroken &= same;
        if same && (unbroken || listening) {
            selected = index;
            nested |= listening;
        }
    }
    let mut ancestors = chain.into_iter().skip(selected);
    let mut launch = ancestors.next()?.1.launch;
    launch.nested = nested;
    for (pid, info) in ancestors {
        nested |= listeners.iter().any(|l| l.0 == pid);
        if command_shell(&info.launch) {
            continue;
        }
        if !package_runner(&info.launch) || !contains_folder(&info.launch.dir, &launch.dir) {
            break;
        }
        launch = info.launch;
        launch.nested = nested;
    }
    Some(launch)
}

/// Only noninteractive command wrappers may connect a server to its runner.
fn command_shell(launch: &Launch) -> bool {
    if !file_name(&launch.exe).eq_ignore_ascii_case("cmd.exe") {
        return false;
    }
    let mut args = launch.args.as_str();
    let mut flags = 0;
    loop {
        let end = args.find([' ', '\t']).unwrap_or(args.len());
        let flag = &args[..end];
        args = args[end..].trim_start_matches([' ', '\t']);
        if flag.eq_ignore_ascii_case("/c") {
            return !args.is_empty();
        }
        let bit = match flag.to_ascii_lowercase().as_str() {
            "/d" => 1,
            "/s" => 2,
            _ => return false,
        };
        if flags & bit != 0 {
            return false;
        }
        flags |= bit;
    }
}

fn package_runner(launch: &Launch) -> bool {
    if !file_name(&launch.exe).eq_ignore_ascii_case("node.exe") {
        return false;
    }
    let args = launch.args.trim_start_matches([' ', '\t']);
    let script = if let Some(quoted) = args.strip_prefix('"') {
        let Some((script, rest)) = quoted.split_once('"') else {
            return false;
        };
        if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
            return false;
        }
        script
    } else {
        args.split([' ', '\t']).next().unwrap_or_default()
    };
    let script = script.replace('/', "\\").to_ascii_lowercase();
    [
        r"node_modules\npm\bin\npm-cli.js",
        r"node_modules\pnpm\bin\pnpm.cjs",
        r"node_modules\pnpm\bin\pnpm.mjs",
        r"node_modules\yarn\bin\yarn.js",
        r"node_modules\@yarnpkg\cli-dist\bin\yarn.js",
    ]
    .iter()
    .any(|suffix| {
        script
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('\\'))
    })
}

fn contains_folder(parent: &str, child: &str) -> bool {
    let mut child = Path::new(child).components();
    Path::new(parent).is_absolute()
        && Path::new(parent).components().all(|part| {
            child.next().is_some_and(|child| {
                part.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&child.as_os_str().to_string_lossy())
            })
        })
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

struct ProcessInfo {
    launch: Launch,
    parent: u32,
    created: u64,
}

impl Process {
    fn open(pid: u32, access: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then(|| Self(handle))
    }

    fn info(pid: u32) -> Option<ProcessInfo> {
        let process = Self::open(
            pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ | PROCESS_SYNCHRONIZE,
        )?;
        let exe = process.image()?;
        let (dir, line, parent) = process.parameters()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let timed = unsafe {
            GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user)
        };
        if timed == 0 || unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT {
            return None;
        }
        Some(ProcessInfo {
            launch: Launch {
                dir,
                exe,
                args: arguments(&line).to_owned(),
                nested: false,
            },
            parent,
            created: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
        })
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
    use super::*;
    use std::net::TcpListener;

    fn process(pid: u32, parent: u32, dir: &str, exe: &str, args: &str) -> (u32, ProcessInfo) {
        (
            pid,
            ProcessInfo {
                launch: Launch {
                    dir: dir.into(),
                    exe: exe.into(),
                    args: args.into(),
                    nested: false,
                },
                parent,
                created: 100 - u64::from(pid),
            },
        )
    }

    fn recover(chain: Vec<(u32, ProcessInfo)>, listening: &[u32]) -> Launch {
        let chain: HashMap<_, _> = chain.into_iter().collect();
        let listeners: Vec<_> = listening
            .iter()
            .map(|&pid| (pid, 3000, Ipv4Addr::LOCALHOST.into()))
            .collect();
        launch(1, &listeners, |pid| {
            let info = chain.get(&pid)?;
            Some(ProcessInfo {
                launch: info.launch.clone(),
                parent: info.parent,
                created: info.created,
            })
        })
        .unwrap()
    }

    #[test]
    fn recovers_the_root_launcher_across_nested_scripts_and_a_port_change() {
        let root = r"C:\design-kit\";
        let preview = r"C:\design-kit\preview\";
        let node = r"C:\Program Files\nodejs\node.exe";
        let runner = r#""C:\User Files\npm\\node_modules\pnpm\bin\pnpm.mjs" "dev""#;
        let saved = recover(
            vec![
                process(1, 2, preview, node, "vite.js dev --port 4176"),
                process(2, 3, preview, "cmd.exe", "/d /s /c vite dev --port 4176"),
                process(
                    3,
                    4,
                    root,
                    node,
                    &runner.replace("\"dev\"", "-C preview dev"),
                ),
                process(4, 5, root, "cmd.exe", "/d /s /c pnpm -C preview dev"),
                process(5, 6, root, node, runner),
                process(6, 7, root, "cmd.exe", "/d /s /c pnpm.cmd dev"),
                process(7, 0, root, "Claude.exe", ""),
            ],
            &[1],
        );
        assert_eq!(saved.dir, root);
        assert_eq!(saved.exe, node);
        assert_eq!(saved.args, runner);
        assert!(!saved.nested);
        let running = Server {
            port: 4177,
            name: saved.name().into(),
            address: Ipv4Addr::LOCALHOST.into(),
            launch: Some(saved.clone()),
        };
        assert!(running.runs(&saved, 4176));
    }

    #[test]
    fn recovers_supported_runners_without_changing_their_arguments() {
        for (script, shell) in [
            (r"C:\Program Files\node_modules\npm\bin\npm-cli.js", "/c"),
            (r"C:\tools\node_modules\pnpm\bin\pnpm.cjs", "/d /c"),
            (r"C:/tools/NODE_MODULES/PNPM/bin/pnpm.mjs", "/s /c"),
            (r"C:\tools\node_modules\yarn\bin\yarn.js", "/d /s /c"),
            (
                r"C:\tools\node_modules\@yarnpkg\cli-dist\bin\yarn.js",
                "/S /D /C",
            ),
        ] {
            let args = format!(r#""{script}" run storybook -- --title "two words""#);
            let found = recover(
                vec![
                    process(1, 2, r"C:\app\child\", "node.exe", "serve.js"),
                    process(2, 3, r"C:\app\child\", "cmd.exe", &format!("{shell} serve")),
                    process(3, 0, r"c:/APP/", "NODE.EXE", &args),
                ],
                &[1],
            );
            assert_eq!(found.args, args, "{script}");
            assert_eq!(found.exe, "NODE.EXE");
            assert_eq!(found.dir, r"c:/APP/");
        }
    }

    #[test]
    fn unknown_commands_and_unrelated_folders_stop_runner_recovery() {
        let runner = r"C:\tools\node_modules\npm\bin\npm-cli.js run site";
        for (exe, args, dir) in [
            ("cmd.exe", "/k serve", r"C:\app\"),
            ("cmd.exe", "/c   ", r"C:\app\"),
            ("cmd.exe", "/q /c serve", r"C:\app\"),
            ("cmd.exe", "/d /d /c serve", r"C:\app\"),
            ("powershell.exe", "-Command serve", r"C:\app\"),
            ("Claude.exe", "", r"C:\app\"),
            ("node.exe", "agent.js", r"C:\app\"),
            (
                "node.exe",
                "unknown.js --config node_modules\\npm\\bin\\npm-cli.js",
                r"C:\app\",
            ),
            (
                "node.exe",
                r"C:\notnode_modules\npm\bin\npm-cli.js run site",
                r"C:\app\",
            ),
            (
                "node.exe",
                r#""C:\node_modules\npm\bin\npm-cli.js"suffix run site"#,
                r"C:\app\",
            ),
            ("bun.exe", runner, r"C:\app\"),
            ("node.exe", runner, r"C:\other\"),
            ("node.exe", runner, r"C:\ap\"),
        ] {
            let found = recover(
                vec![
                    process(1, 2, r"C:\app\", "node.exe", "serve.js"),
                    process(2, 3, r"C:\app\", "cmd.exe", "/c serve"),
                    process(3, 4, dir, exe, args),
                    process(4, 5, r"C:\app\", "cmd.exe", "/c runner"),
                    process(5, 0, r"C:\app\", "node.exe", runner),
                ],
                &[1],
            );
            assert_eq!(found.args, "serve.js", "{exe} {args} in {dir}");
        }
    }

    #[test]
    fn unverifiable_ancestry_retains_the_last_valid_launcher() {
        let runner = r"C:\tools\node_modules\pnpm\bin\pnpm.cjs run site";
        for boundary in ["missing", "reused", "cycle", "depth"] {
            let mut chain = vec![
                process(1, 2, r"C:\app\", "node.exe", "serve.js"),
                process(2, 3, r"C:\app\", "cmd.exe", "/c serve"),
                process(3, 4, r"C:\app\", "node.exe", runner),
            ];
            match boundary {
                "reused" => {
                    let mut reused =
                        process(4, 0, r"C:\app\", "node.exe", &format!("{runner} outer"));
                    reused.1.created = 1000;
                    chain.push(reused);
                }
                "cycle" => {
                    chain.push(process(4, 1, r"C:\app\", "cmd.exe", "/c runner"));
                    for (_, info) in &mut chain {
                        info.created = 1;
                    }
                }
                "depth" => {
                    for pid in 4..=9 {
                        chain.push(process(pid, pid + 1, r"C:\app\", "cmd.exe", "/c runner"));
                    }
                    chain.push(process(
                        10,
                        0,
                        r"C:\app\",
                        "node.exe",
                        &format!("{runner} outer"),
                    ));
                }
                _ => {}
            }
            assert_eq!(recover(chain, &[1]).args, runner, "{boundary}");
        }
    }

    #[test]
    fn preserves_direct_commands_python_wrappers_and_nested_endpoints() {
        for (exe, args) in [
            (
                "node.exe",
                "./node_modules/@angular/cli/bin/ng serve -c development",
            ),
            ("python.exe", "-m http.server 8000"),
        ] {
            let found = recover(vec![process(1, 0, r"C:\site\", exe, args)], &[1]);
            assert_eq!((found.exe.as_str(), found.args.as_str()), (exe, args));
            assert!(!found.nested);
        }
        let python = recover(
            vec![
                process(
                    1,
                    2,
                    r"C:\site\",
                    r"C:\Python\python.exe",
                    "-m http.server 8000",
                ),
                process(
                    2,
                    0,
                    r"C:\site\",
                    r"C:\site\.venv\Scripts\python.exe",
                    "-m http.server 8000",
                ),
            ],
            &[1],
        );
        assert_eq!(python.exe, r"C:\site\.venv\Scripts\python.exe");

        let runner = r"C:\tools\node_modules\pnpm\bin\pnpm.cjs dev";
        let tina_launch = recover(
            vec![
                process(1, 2, r"C:\site\", "node.exe", "tina.js"),
                process(2, 3, r"C:\site\", "cmd.exe", "/c tina"),
                process(3, 0, r"C:\site\", "node.exe", runner),
            ],
            &[1],
        );
        let astro_launch = recover(
            vec![
                process(1, 2, r"C:\site\", "node.exe", "astro.js"),
                process(2, 3, r"C:\site\", "cmd.exe", "/c astro"),
                process(3, 4, r"C:\site\", "node.exe", "tina.js"),
                process(4, 5, r"C:\site\", "cmd.exe", "/c tina"),
                process(5, 0, r"C:\site\", "node.exe", runner),
            ],
            &[1, 3],
        );
        assert_eq!(tina_launch.args, runner);
        assert!(!tina_launch.nested);
        assert_eq!(astro_launch.args, runner);
        assert!(astro_launch.nested);
        let tina = Server {
            port: 4001,
            name: "site".into(),
            address: Ipv4Addr::LOCALHOST.into(),
            launch: Some(tina_launch),
        };
        assert!(!tina.runs(&astro_launch, 4321));
    }

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
