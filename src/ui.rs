//! The strip in the taskbar and the server menu above it. Everything runs on
//! the UI thread. Window procedures reach the shared state through `with`.

use crate::{
    branches::Branches,
    editor, instance,
    menu_controls::Controls,
    projects::Projects,
    render::{Align, Face, Rect, Renderer, Style},
    servers::{self, Scanner, Server, Starting},
    theme::{Rgb, color, font, menu, strip},
    wide,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    mem::{take, zeroed},
    path::{Component, Path, PathBuf, Prefix},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use win_taskbar_host::{Content, Surface, TaskbarHost};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, EndPaint, HDC, InvalidateRect, PAINTSTRUCT, ScreenToClient, ValidateRect,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        Controls::WM_MOUSELEAVE,
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{
            GetCapture, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE,
            TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_SPACE, VK_TAB,
        },
        WindowsAndMessaging::*,
    },
};

/// The menu's class. The installer finds the running copy by it, since the
/// menu, hidden or not, also runs the timers and takes the request to exit.
pub const CLASS_NAME: &str = "Portside";
const VIEW_CLASS: &str = "PortsideStrip";
/// The class of Windows' popup menus.
const POPUP_MENU_CLASS: &str = "#32768";
/// Menu timers.
const POLL: usize = 1;
const HOVER: usize = 2;
const SPIN: usize = 3;
const POLL_MS: u32 = 1000;
/// Posted for a click outside the menu and its native popup.
const BUTTON_DOWN: u32 = WM_APP;
const FOREGROUND_CHANGED: u32 = WM_APP + 1;
const VIEW_READY: u32 = WM_APP + 3;
const EDITOR_READY: u32 = WM_APP + 4;

const STRIP_TEXT: Style = Style(font::STRIP_SIZE, Face::Regular, 0);
const MENU_TEXT: Style = Style(font::MENU_SIZE, Face::Regular, 0);
const MENU_ITALIC: Style = Style(font::MENU_SIZE, Face::Italic, 0);
const ICON: Style = Style(font::ICON_SIZE, Face::Icon, 0);
const ACTION_ICON: Style = Style(font::ACTION_SIZE, Face::Icon, 0);
const COUNT_TEXT: Style = Style(
    font::STRIP_SIZE,
    Face::Regular,
    u32::from_le_bytes(font::COUNT_FEATURE),
);

/// The globe keeps its place in every state; the count, or the name, follows it.
const GLOBE: Rect = [
    strip::LEAD + strip::LABEL_PAD_X,
    0.0,
    strip::LEAD + strip::LABEL_PAD_X + font::ICON_SIZE,
    strip::HEIGHT,
];
const TEXT_X: f32 = GLOBE[2] + strip::GLOBE_TO_TEXT;
const LABEL_TOP: f32 = (strip::HEIGHT - strip::HIT_H) / 2.0;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Toggle,
}

#[derive(Clone, Copy, PartialEq)]
enum MenuFocus {
    Hover,
    FindingFirst,
    Keyboard,
}

/// The Close row is all `Action`.
#[derive(Clone, Copy, PartialEq)]
enum Zone {
    Body,
    Editor,
    Action,
    Clear,
    Handle,
}

#[derive(Clone, Copy, PartialEq)]
enum Row {
    Line,
    Close,
    /// Its index in `servers` while running, and in `projects` when saved.
    Item {
        server: Option<usize>,
        project: Option<usize>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Label,
    Row(Row, Zone),
}

#[derive(Clone, Copy, PartialEq)]
enum Input {
    Left,
    Middle,
    Key(u16),
}

/// Control identity across scans. A saved default can start on a different port.
#[derive(Clone, PartialEq)]
enum RowKey {
    Default(String),
    Folder(String, u16),
    Process(u32, u16),
}

#[derive(Default)]
struct EditorAvailability {
    exe: Option<PathBuf>,
    folders: HashMap<String, bool>,
}

enum EditorUpdate {
    Installation(Option<PathBuf>),
    Folder(String, bool),
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum EditorSource {
    Installation,
    Drive(u8),
    Share,
}

impl EditorSource {
    fn folder(dir: &str) -> Self {
        if let Some(Component::Prefix(prefix)) = Path::new(dir).components().next()
            && let Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) = prefix.kind()
        {
            return Self::Drive(letter.to_ascii_uppercase());
        }
        // Standard and verbatim UNC paths share a worker, separate from every drive letter.
        Self::Share
    }
}

struct EditorScan {
    result: Receiver<EditorUpdate>,
    from_closed_menu: bool,
}

/// Strip geometry for the current server, kept for painting and hit tests.
#[derive(Default)]
struct Layout {
    /// The label's hover rect; empty with no servers and no defaults.
    label: Rect,
    /// Where the name starts, and the width it gets before it is cut.
    name_x: f32,
    name_max: f32,
}

struct State {
    renderer: Renderer,
    scanner: Scanner,
    branches: Branches,
    servers: Vec<Server>,
    projects: Projects,
    /// Saved commands that have not listened yet.
    starting: Vec<Starting>,
    current: usize,
    /// The server the user last moved to, kept while it restarts.
    chosen: Option<Server>,
    layout: Layout,
    view: HWND,
    menu: HWND,
    /// The visible rows, shared by painting, sizing and pointer hit tests.
    menu_rows: Vec<Row>,
    menu_w: f32,
    mode: Mode,
    /// The mouse hook, while the toggle view is open.
    hook: HHOOK,
    /// Recheck an open menu on the server poll if the foreground hook is unavailable.
    poll_menu_order: bool,
    dpi: u32,
    hover: Option<Part>,
    hover_deadline: Option<Instant>,
    pressed: Option<(Part, Input)>,
    focus: MenuFocus,
    menu_request: Option<instance::MenuRequest>,
    controls: Controls<(RowKey, Zone), Part>,
    editor: EditorAvailability,
    /// At most one worker per source across openings, even when a mapped drive or share stalls.
    editor_scans: HashMap<EditorSource, EditorScan>,
    /// The project whose handle is being dragged.
    drag: Option<usize>,
    wheel: i32,
    /// The strip's width with a server, and the width the host is built with.
    full_width: f32,
    /// Kept once created, so the strip can resize with the servers.
    host: Option<TaskbarHost>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// The menu, for hooks that can run while `STATE` is borrowed.
    static PANEL: Cell<HWND> = const { Cell::new(null_mut()) };
}

/// Runs `f` on the state, unless it is gone or already in use further up the
/// stack by a handler whose Win32 call sent this message.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn contains(rect: Rect, (x, y): (f32, f32)) -> bool {
    x >= rect[0] && x < rect[2] && y >= rect[1] && y < rect[3]
}

pub fn run(request: Option<instance::MenuRequest>) -> Result<(), String> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let renderer = Renderer::new()?;
    // Wide enough for every state with a server, so the strip never shifts
    // along the taskbar. The host measures its position against this width.
    let full_width = TEXT_X
        + renderer.measure("99/99", COUNT_TEXT)
        + strip::COUNT_TO_NAME
        + strip::NAME_SLOT_W
        + strip::LABEL_PAD_X;
    register(CLASS_NAME, Some(menu_proc));
    register(VIEW_CLASS, Some(view_proc));
    let ex_style = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED;
    let menu = window(CLASS_NAME, ex_style, WS_POPUP, null_mut(), (0, 0))?;
    unsafe {
        SetTimer(menu, POLL, POLL_MS, None);
    }
    PANEL.set(menu);
    let foreground_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            null_mut(),
            Some(foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    STATE.set(Some(State {
        renderer,
        scanner: Scanner::default(),
        branches: Branches::new(),
        servers: Vec::new(),
        projects: Projects::load(),
        starting: Vec::new(),
        current: 0,
        chosen: None,
        layout: Layout::default(),
        view: null_mut(),
        menu,
        menu_rows: Vec::new(),
        menu_w: 0.0,
        mode: Mode::Normal,
        hook: null_mut(),
        poll_menu_order: foreground_hook.is_null(),
        dpi: 96,
        hover: None,
        hover_deadline: None,
        pressed: None,
        focus: MenuFocus::Hover,
        menu_request: request,
        controls: Controls::default(),
        editor: EditorAvailability::default(),
        editor_scans: HashMap::new(),
        drag: None,
        wheel: 0,
        full_width,
        host: None,
    }));
    with(|state| {
        let servers = state.scanner.scan();
        state.apply(servers);
        state.try_menu_request();
    });
    let result = TaskbarHost::builder(full_width as f64)
        .height_dip(strip::HEIGHT as f64)
        .save_placement_as("portside")
        .menu_item_checked(
            "Defaults",
            || with(|state| state.mode == Mode::Toggle).unwrap_or(false),
            || _ = with(State::toggle_defaults),
        )
        .menu_item("Exit", move || unsafe {
            PostMessageW(menu, WM_CLOSE, 0, 0);
        })
        .create(|surface: &Surface| {
            let size = (surface.width, surface.height);
            let view = window(VIEW_CLASS, 0, WS_CHILD | WS_VISIBLE, surface.parent, size)?;
            with(|state| {
                state.view = view;
                state.dpi = surface.dpi;
                // The host positions and shows this child after the factory returns.
                unsafe { PostMessageW(state.menu, VIEW_READY, 0, 0) };
            });
            Ok(View(view))
        })
        .and_then(|host| {
            with(|state| {
                state.host = Some(host);
                state.fit();
            });
            let result = message_loop();
            // The view's procedure still uses the state while the host destroys it.
            drop(with(|state| state.host.take()));
            result
        });
    unsafe {
        if !foreground_hook.is_null() {
            UnhookWinEvent(foreground_hook);
        }
        DestroyWindow(menu);
    }
    STATE.take();
    result
}

fn message_loop() -> Result<(), String> {
    unsafe {
        let mut msg: MSG = zeroed();
        loop {
            match GetMessageW(&mut msg, null_mut(), 0, 0) {
                -1 => return Err(format!("Window message loop failed: {}", GetLastError())),
                0 => return Ok(()),
                _ => {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }
}

fn register(class: &str, procedure: WNDPROC) {
    let name = wide(class);
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: procedure,
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: name.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&class);
    }
}

fn window(
    class: &str,
    ex_style: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
    parent: HWND,
    (width, height): (i32, i32),
) -> Result<HWND, String> {
    unsafe {
        let hwnd = CreateWindowExW(
            ex_style,
            wide(class).as_ptr(),
            null(),
            style,
            0,
            0,
            width,
            height,
            parent,
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return Err(format!("Could not create a window: {}", GetLastError()));
        }
        Ok(hwnd)
    }
}

struct View(HWND);

impl Content for View {
    fn hwnd(&self) -> HWND {
        self.0
    }
    fn resized(&mut self, surface: &Surface) {
        with(|state| {
            state.dpi = surface.dpi;
            if state.menu_open() {
                state.show_menu();
            }
        });
        unsafe { InvalidateRect(self.0, null(), 0) };
    }
}

impl State {
    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// A mouse message's client position in DIPs.
    fn point(&self, l: LPARAM) -> (f32, f32) {
        let scale = self.scale();
        ((l as i16) as f32 / scale, ((l >> 16) as i16) as f32 / scale)
    }

    fn menu_open(&self) -> bool {
        unsafe { IsWindowVisible(self.menu) != 0 }
    }

    /// Sizes the strip in the taskbar: just the globe with no servers.
    fn fit(&self) {
        let Some(host) = &self.host else {
            return;
        };
        let width = if self.servers.is_empty() {
            GLOBE[2] + strip::LABEL_PAD_X
        } else {
            self.full_width
        };
        let _ = host.set_width_dip(width.into());
    }

    fn relayout(&mut self) {
        let mut layout = Layout::default();
        let n = self.servers.len();
        if let Some(server) = self.servers.get(self.current) {
            let mut name_x = TEXT_X;
            if n > 1 {
                // Sized for the widest position, so cycling never moves the name.
                name_x +=
                    self.renderer.measure(&format!("{n}/{n}"), COUNT_TEXT) + strip::COUNT_TO_NAME;
            }
            let port_w = self
                .renderer
                .measure(&format!(":{}", server.port), STRIP_TEXT);
            let name_max = strip::NAME_SLOT_W - strip::NAME_TO_PORT - port_w;
            let name_w = self
                .renderer
                .measure(&server.name, STRIP_TEXT)
                .min(name_max);
            let right = name_x + name_w + strip::NAME_TO_PORT + port_w + strip::LABEL_PAD_X;
            layout.label = [strip::LEAD, LABEL_TOP, right, LABEL_TOP + strip::HIT_H];
            layout.name_x = name_x;
            layout.name_max = name_max;
        } else if self.projects.split() > 0 {
            let right = GLOBE[2] + strip::LABEL_PAD_X;
            layout.label = [strip::LEAD, LABEL_TOP, right, LABEL_TOP + strip::HIT_H];
        }
        self.layout = layout;
    }

    fn invalidate(&self, hwnd: HWND) {
        if !hwnd.is_null() {
            unsafe { InvalidateRect(hwnd, null(), 0) };
        }
    }

    /// Repaints the strip, and the menu while it is open.
    fn redraw(&self) {
        self.invalidate(self.view);
        if self.menu_open() {
            self.invalidate(self.menu);
        }
    }

    /// Polls the listener table and finishes the starts that now listen.
    fn refresh(&mut self) {
        if self.poll_menu_order && self.menu_open() {
            unsafe {
                let mut rect = RECT::default();
                if GetWindowRect(self.menu, &mut rect) != 0 {
                    let edge = POINT {
                        x: rect.left + (rect.right - rect.left) / 2,
                        y: rect.bottom - 1,
                    };
                    let foreground = GetForegroundWindow();
                    if GetAncestor(WindowFromPoint(edge), GA_ROOT) == foreground {
                        raise_for_taskbar(self.menu, self.view, foreground);
                    }
                }
            }
        }
        let servers = self.scanner.scan();
        let starting = self.starting.len();
        let mut started = Vec::new();
        for start in take(&mut self.starting) {
            match start.launch.find_server(&servers, start.port) {
                Some(index) => started.push(index), // the handle closes, the server runs on
                None if start.expired() => start.end(),
                None => self.starting.push(start),
            }
        }
        if let Some(&last) = started.last() {
            self.chosen = Some(servers[last].clone());
        }
        if servers != self.servers {
            self.apply(servers);
        } else if self.starting.len() != starting {
            self.sync_controls();
            self.rehover();
        }
        for index in started {
            self.open(&self.servers[index]);
        }
        // Keep menu geometry still while a click or drag is in progress.
        if self.pressed.is_none() && self.drag.is_none() {
            // observe keeps a project for every readable running folder, too.
            let dirs = self
                .projects
                .list()
                .iter()
                .map(|p| p.launch.dir.clone())
                .collect();
            if self.branches.refresh(dirs) {
                if self.menu_open() {
                    self.show_menu();
                } else if self.hover == Some(Part::Label) {
                    self.arm_hover(menu::SHOW_DELAY_MS);
                }
            }
        }
        if self.menu_open() && self.mode == Mode::Normal {
            self.editor_ready();
            if self.editor.exe.is_some() {
                self.check_editor(true);
            }
        }
        self.try_menu_request();
    }

    /// Takes in a new scan and repaints.
    fn apply(&mut self, servers: Vec<Server>) {
        let held = self
            .pressed
            .take()
            .and_then(|(part, input)| self.action_id(part).map(|id| (id, input)));
        let index = |target: Option<&Server>| {
            target.and_then(|target| servers.iter().position(|s| s.same_endpoint(target)))
        };
        let kept = index(self.chosen.as_ref()).or_else(|| index(self.servers.get(self.current)));
        self.current = kept.unwrap_or(self.current.min(servers.len().saturating_sub(1)));
        self.servers = servers;
        self.projects.observe(&self.servers);
        self.relayout();
        self.fit();
        if self.menu_open() {
            self.show_menu();
        } else {
            self.rehover();
        }
        // A scan can change row indices. Continue only if the same action is still targeted.
        if let Some((id, input)) = held
            && let Some(part) = self.target(input)
            && self.enabled(part)
            && self.action_id(part) == Some(id)
        {
            self.pressed = Some((part, input));
        }
    }

    fn step(&mut self, by: isize) {
        let n = self.servers.len() as isize;
        if n > 1 {
            self.choose((self.current as isize + by).rem_euclid(n) as usize);
        }
    }

    fn choose(&mut self, i: usize) {
        if self.current != i && self.pressed.is_some_and(|(part, _)| part == Part::Label) {
            self.pressed = None;
        }
        self.current = i;
        self.chosen = self.servers.get(i).cloned();
        self.relayout();
        self.redraw();
    }

    fn scroll(&mut self, delta: i32) {
        if (self.wheel > 0) != (delta > 0) {
            self.wheel = 0;
        }
        self.wheel += delta;
        let notches = self.wheel / WHEEL_DELTA as i32;
        self.wheel %= WHEEL_DELTA as i32;
        // Wheel up is the previous server.
        self.step(-notches as isize);
    }

    fn set_hover(&mut self, part: Option<Part>) -> bool {
        let changed = self.hover != part;
        if changed {
            self.hover = part;
            if !matches!(self.pressed, Some((_, Input::Key(_)))) {
                self.pressed = None;
            }
        }
        changed
    }

    fn strip_move(&mut self, at: (f32, f32)) {
        let part = contains(self.layout.label, at).then_some(Part::Label);
        let changed = self.set_hover(part);
        if part.is_none() {
            self.outside_menu_and_label();
        } else if self.menu_open() {
            self.cancel_hover_timer();
        } else if changed {
            self.arm_hover(menu::SHOW_DELAY_MS);
        }
        if changed {
            self.redraw();
        }
    }

    fn menu_move(&mut self, at: (f32, f32)) {
        self.cancel_hover_timer();
        if self.drag.is_some() {
            return self.drag_to(at.1);
        }
        let toggle = self.mode == Mode::Toggle;
        let part = self
            .slots()
            .find(|&(_, rect)| contains(rect, at))
            .and_then(|(row, rect)| {
                let zone = match row {
                    Row::Close => Zone::Action,
                    Row::Item { .. } if !toggle && contains(row_buttons(rect).1, at) => {
                        Zone::Editor
                    }
                    Row::Item { .. } if !toggle => Zone::Body,
                    Row::Item { .. } if at.0 >= rect[2] - menu::ROW_PAD_X - font::ACTION_SIZE => {
                        Zone::Action
                    }
                    Row::Item { project, .. }
                        if at.0 < rect[0] + menu::ROW_PAD_X + font::ACTION_SIZE =>
                    {
                        if self.is_default(project) {
                            Zone::Handle
                        } else if self.can_clear(project) {
                            Zone::Clear
                        } else {
                            Zone::Body
                        }
                    }
                    Row::Item { .. } => Zone::Body,
                    _ => return None,
                };
                Some(Part::Row(row, zone))
            });
        if self.set_hover(part) {
            self.invalidate(self.menu);
        }
    }

    /// Read the cursor once. Native buttons share the painted menu's coordinates.
    fn pointer(&self) -> (HWND, (f32, f32)) {
        unsafe {
            let mut cursor = zeroed();
            if GetCursorPos(&mut cursor) == 0 {
                return (null_mut(), (0.0, 0.0));
            }
            let under = WindowFromPoint(cursor);
            let under = if IsChild(self.menu, under) != 0 {
                self.menu
            } else {
                under
            };
            if under != self.view && under != self.menu {
                return (under, (0.0, 0.0));
            }
            ScreenToClient(under, &mut cursor);
            let scale = self.scale();
            (under, (cursor.x as f32 / scale, cursor.y as f32 / scale))
        }
    }

    /// The rows or the strip may have changed under a still pointer.
    fn rehover(&mut self) {
        let (under, at) = self.pointer();
        if under == self.menu {
            self.menu_move(at);
        } else if under == self.view {
            self.strip_move(at);
        } else {
            self.outside_menu_and_label();
            self.set_hover(None);
        }
        self.redraw();
    }

    fn outside_menu_and_label(&mut self) {
        if self.menu_open() && self.mode == Mode::Normal && self.focus == MenuFocus::Hover {
            self.arm_hover(menu::HIDE_DELAY_MS);
        } else {
            self.cancel_hover_timer();
        }
    }

    fn arm_hover(&mut self, delay: u32) {
        if self.hover_deadline.is_none() && unsafe { SetTimer(self.menu, HOVER, delay, None) } != 0
        {
            self.hover_deadline = Some(Instant::now() + Duration::from_millis(delay.into()));
        }
    }

    fn cancel_hover_timer(&mut self) {
        if self.hover_deadline.take().is_some() {
            unsafe { KillTimer(self.menu, HOVER) };
        }
    }

    fn finish_hover(&mut self) {
        let Some(deadline) = self.hover_deadline else {
            return;
        };
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            // KillTimer can leave a message from the previous entry or exit in the queue.
            unsafe { SetTimer(self.menu, HOVER, remaining.as_millis().max(1) as u32, None) };
            return;
        }
        self.cancel_hover_timer();
        let (under, at) = self.pointer();
        let label = under == self.view && contains(self.layout.label, at);
        if self.menu_open() {
            if !label
                && under != self.menu
                && self.focus == MenuFocus::Hover
                && self.mode == Mode::Normal
            {
                self.hide_menu();
            }
        } else if label {
            self.show_menu();
        }
    }

    fn leave(&mut self) {
        self.wheel = 0;
        self.rehover();
    }

    fn press(&mut self, button: Input) {
        if self.focus == MenuFocus::FindingFirst {
            self.focus = MenuFocus::Keyboard;
        }
        let target = self.hover.filter(|&part| {
            self.enabled(part) && (button != Input::Middle || self.stop_target(part).is_some())
        });
        self.pressed = match target {
            Some(Part::Row(Row::Item { project, .. }, Zone::Handle)) => {
                self.drag = project;
                self.hover = None;
                unsafe { SetCapture(self.menu) };
                None
            }
            hover => hover.map(|part| (part, button)),
        };
        self.redraw();
    }

    fn mouse_button(&mut self, window: HWND, message: u32, l: LPARAM) {
        let at = self.point(l);
        if window == self.menu {
            self.menu_move(at);
        } else {
            self.strip_move(at);
        }
        match message {
            WM_LBUTTONDOWN => self.press(Input::Left),
            WM_MBUTTONDOWN => self.press(Input::Middle),
            WM_LBUTTONUP => self.release(Input::Left),
            WM_MBUTTONUP => self.release(Input::Middle),
            _ => {}
        }
    }

    fn target(&self, input: Input) -> Option<Part> {
        match input {
            Input::Key(_) => self.controls.focused(),
            _ => self.hover,
        }
    }

    fn release(&mut self, input: Input) {
        if self.drag.is_some() && input == Input::Left {
            return self.end_drag();
        }
        let Some((part, down)) = self.pressed else {
            return;
        };
        if down != input {
            return;
        }
        self.pressed = None;
        if self.target(input) == Some(part) {
            if input != Input::Middle {
                return self.activate(part);
            }
            if let Some(i) = self.stop_target(part) {
                self.scanner.stop(&self.servers[i]);
            }
        }
        self.redraw();
    }

    fn activate(&mut self, part: Part) {
        if self.focus == MenuFocus::FindingFirst {
            self.focus = MenuFocus::Keyboard;
        }
        if !self.enabled(part) {
            return;
        }
        match part {
            Part::Label => {
                if let Some(server) = self.servers.get(self.current) {
                    self.open(server);
                }
            }
            Part::Row(Row::Close, _) => self.close_toggle(),
            Part::Row(row, Zone::Editor) => {
                if let Some(exe) = &self.editor.exe
                    && let Some(dir) = self.row_dir(row)
                {
                    let exe = exe.clone();
                    let dir = PathBuf::from(dir);
                    thread::spawn(move || editor::open(&exe, &dir));
                }
            }
            Part::Row(
                Row::Item {
                    server,
                    project: Some(p),
                },
                zone @ (Zone::Action | Zone::Clear),
            ) => {
                if zone == Zone::Action {
                    self.projects.toggle_default(p, server, &self.servers);
                } else if self.can_clear(Some(p)) {
                    self.projects.remove_past(p);
                }
                self.relayout();
                self.show_menu();
            }
            Part::Row(
                Row::Item {
                    server: Some(i), ..
                },
                Zone::Body,
            ) => {
                self.choose(i);
                self.open(&self.servers[i]);
            }
            Part::Row(
                Row::Item {
                    project: Some(p), ..
                },
                Zone::Body,
            ) => {
                let saved = &self.projects.list()[p];
                if let Some(start) = servers::start(&saved.launch, saved.port) {
                    self.starting.push(start);
                    self.sync_controls();
                    unsafe { SetTimer(self.menu, SPIN, menu::SPINNER_FRAME_MS, None) };
                }
            }
            Part::Row(..) => {}
        }
        self.redraw();
    }

    /// Opens a server in the default browser.
    fn open(&self, server: &Server) {
        // localhost may reach another server when they share a port.
        let shared = self
            .servers
            .iter()
            .filter(|s| s.port == server.port)
            .count()
            > 1;
        // A short-lived Explorer opens the default browser, so the shell's
        // URL handlers never load into this long-running process. It gets
        // no standard handles, which a stop briefly swaps for a console's.
        let _ = Command::new("explorer")
            .arg(server.url(shared))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    /// A middle press belongs only to a running strip label or row body.
    fn stop_target(&self, part: Part) -> Option<usize> {
        match part {
            Part::Label => self.servers.get(self.current).map(|_| self.current),
            Part::Row(Row::Item { server, .. }, Zone::Body) if self.mode == Mode::Normal => server,
            Part::Row(..) => None,
        }
    }

    /// One spinner frame. The timer outlasts the last start by one tick, which
    /// clears a spinner that timed out.
    fn spin(&self) {
        if self.menu_open() {
            self.invalidate(self.menu);
        }
        if self.starting.is_empty() {
            unsafe { KillTimer(self.menu, SPIN) };
        }
    }

    fn show_menu(&mut self) {
        let opening = !self.menu_open();
        let rows = self.rows();
        let can_open = match rows.as_slice() {
            [] => false,
            [
                Row::Item {
                    server: Some(i), ..
                },
            ] => self.servers[*i]
                .launch
                .as_ref()
                .is_some_and(|launch| self.branches.get(&launch.dir).is_some()),
            _ => true,
        };
        if self.view.is_null() || !can_open {
            self.hide_menu();
            self.rehover();
            return;
        }
        let widest = rows
            .iter()
            .map(|&row| self.row_width(row))
            .fold(0.0, f32::max);
        self.menu_rows = rows;
        let inset = menu::BORDER + menu::PAD;
        let width = widest + 2.0 * (menu::ROW_PAD_X + inset);
        let scale = self.scale();
        let w = (width * scale).ceil() as i32;
        self.menu_w = w as f32 / scale;
        let height = self.slots().last().map_or(0.0, |(_, rect)| rect[3]) + inset;
        let h = (height * scale).ceil() as i32;
        if opening {
            self.cancel_hover_timer();
        }
        self.sync_controls();
        let presented = unsafe {
            let mut view: RECT = zeroed();
            let located = GetWindowRect(self.view, &mut view) != 0;
            // Bottom edge on the top of the label's hover rect, left edges aligned.
            let left = view.left + (strip::LEAD * scale).round() as i32;
            let bottom = view.top + (LABEL_TOP * scale).ceil() as i32;
            let rect = RECT {
                left,
                top: bottom - h,
                right: left + w,
                bottom,
            };
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
            located
                && self.present_menu(rect)
                && SetWindowPos(self.menu, HWND_TOPMOST, 0, 0, 0, 0, flags) != 0
        };
        if !presented {
            if self.mode == Mode::Toggle {
                self.close_toggle();
            } else {
                self.hide_menu();
            }
        } else if opening && self.mode == Mode::Normal {
            self.check_editor(true);
        }
        self.rehover();
    }

    fn try_menu_request(&mut self) {
        let Some(request) = self.menu_request else {
            return;
        };
        if !request.is_fresh() {
            self.menu_request = None;
            return;
        }
        if self.mode == Mode::Toggle {
            self.close_toggle();
        }
        if let Some(dir) = self.single_server_dir()
            && !self.branches.checked(dir)
        {
            let dir = dir.to_owned();
            self.branches.prioritize(&dir);
            return;
        }
        if self.view.is_null() || unsafe { IsWindowVisible(self.view) } == 0 {
            return;
        }
        self.menu_request = None;
        let was_open = self.menu_open();
        self.show_menu();
        if self.menu_open() {
            if request.is_fresh() && unsafe { SetForegroundWindow(self.menu) } != 0 {
                self.focus = MenuFocus::FindingFirst;
                self.cancel_hover_timer();
                self.pressed = None;
                unsafe { SetFocus(self.menu) };
                self.focus_first_available();
            } else if !was_open {
                self.hide_menu();
            }
        }
    }

    fn single_server_dir(&self) -> Option<&str> {
        match self.rows().as_slice() {
            [
                row @ Row::Item {
                    server: Some(_), ..
                },
            ] => self.row_dir(*row),
            _ => None,
        }
    }

    fn check_editor(&mut self, installation: bool) {
        let mut groups = HashMap::<_, Vec<_>>::new();
        if installation && !self.editor_scans.contains_key(&EditorSource::Installation) {
            groups.insert(EditorSource::Installation, Vec::new());
        }
        if self.editor.exe.is_some() {
            let mut seen = HashSet::new();
            for dir in self.menu_rows.iter().filter_map(|&row| self.row_dir(row)) {
                let source = EditorSource::folder(dir);
                if !self.editor_scans.contains_key(&source) && seen.insert(dir) {
                    groups
                        .entry(source)
                        .or_default()
                        .push((dir.to_owned(), self.editor.folders.get(dir).copied()));
                }
            }
        }
        for (source, folders) in groups {
            self.start_editor_scan(source, folders);
        }
    }

    fn start_editor_scan(&mut self, source: EditorSource, folders: Vec<(String, Option<bool>)>) {
        let (send, result) = mpsc::channel();
        self.editor_scans.insert(
            source,
            EditorScan {
                result,
                from_closed_menu: false,
            },
        );
        let menu = self.menu as usize;
        let cached = self.editor.exe.clone();
        thread::spawn(move || {
            let publish = |update| {
                if send.send(update).is_ok() {
                    unsafe { PostMessageW(menu as HWND, EDITOR_READY, 0, 0) };
                }
            };
            if source == EditorSource::Installation {
                let exe = match cached {
                    Some(path) => path.is_file().then_some(path),
                    None => editor::discover(),
                };
                publish(EditorUpdate::Installation(exe));
            } else {
                for (dir, previous) in folders {
                    let readable = editor::available(Path::new(&dir));
                    if previous != Some(readable) {
                        publish(EditorUpdate::Folder(dir, readable));
                    }
                }
            }
            publish(EditorUpdate::Done);
        });
    }

    fn editor_ready(&mut self) {
        let open = self.mode == Mode::Normal && self.menu_open();
        let installed = self.editor.exe.is_some();
        let mut changed = false;
        let mut retry_installation = false;
        let mut retry_folders = false;
        self.editor_scans.retain(|&source, scan| {
            loop {
                match scan.result.try_recv() {
                    Ok(EditorUpdate::Done) if !scan.from_closed_menu => return false,
                    Ok(EditorUpdate::Done) | Err(mpsc::TryRecvError::Disconnected) => {
                        if source == EditorSource::Installation {
                            retry_installation = true;
                        } else {
                            retry_folders = true;
                        }
                        return false;
                    }
                    Err(mpsc::TryRecvError::Empty) => return true,
                    Ok(_) if !open || scan.from_closed_menu => {}
                    Ok(EditorUpdate::Installation(exe)) => {
                        changed |= self.editor.exe != exe;
                        self.editor.exe = exe;
                    }
                    Ok(EditorUpdate::Folder(dir, readable)) => {
                        changed |= self.editor.folders.insert(dir, readable) != Some(readable);
                    }
                }
            }
        });
        if !open {
            return;
        }
        if retry_installation || retry_folders || (!installed && self.editor.exe.is_some()) {
            self.check_editor(retry_installation);
        }
        if changed {
            self.sync_controls();
            self.invalidate(self.menu);
        } else {
            self.focus_first_available();
        }
    }

    fn enabled(&self, part: Part) -> bool {
        match part {
            Part::Row(Row::Item { server, project }, Zone::Body) => {
                self.mode == Mode::Normal
                    && (server.is_some() || project.is_some_and(|p| !self.is_starting(p)))
            }
            Part::Row(row, Zone::Editor) => {
                self.editor.exe.is_some()
                    && self
                        .row_dir(row)
                        .is_some_and(|dir| self.editor.folders.get(dir) == Some(&true))
            }
            _ => true,
        }
    }

    fn row_dir(&self, row: Row) -> Option<&str> {
        let Row::Item { server, project } = row else {
            return None;
        };
        self.row_source(server, project)
            .2
            .map(|launch| launch.dir.as_str())
    }

    fn row_key(&self, server: Option<usize>, project: Option<usize>) -> RowKey {
        if let Some(i) = server
            && !self.is_default(project)
        {
            let running = &self.servers[i];
            return match &running.launch {
                Some(launch) => RowKey::Folder(launch.dir.to_ascii_lowercase(), running.port),
                None => RowKey::Process(running.pid, running.port),
            };
        }
        let saved = &self.projects.list()[project.expect("a default has a saved project")];
        RowKey::Default(saved.launch.dir.to_ascii_lowercase())
    }

    /// Opening or stopping a replacement listener is a different action, even on the same port.
    fn action_id(&self, part: Part) -> Option<(RowKey, Zone, Option<Server>)> {
        let Part::Row(Row::Item { server, project }, zone) = part else {
            return None;
        };
        (self.mode == Mode::Normal).then(|| {
            let endpoint = server
                .filter(|_| zone == Zone::Body)
                .map(|i| self.servers[i].clone());
            (self.row_key(server, project), zone, endpoint)
        })
    }

    /// The native controls follow the painted actions in visual order.
    fn sync_controls(&mut self) {
        if self.mode != Mode::Normal || self.menu_rows.is_empty() {
            self.controls.clear();
            return;
        }
        if self.pressed.is_some_and(|(part, _)| !self.enabled(part)) {
            self.pressed = None;
        }
        let scale = self.scale();
        let pixels = |dip: Rect| {
            let [left, top, right, bottom] = dip.map(|v| (v * scale).ceil() as i32);
            RECT {
                left,
                top,
                right,
                bottom,
            }
        };
        let mut controls = Vec::new();
        for (row, rect) in self.slots() {
            let Row::Item { server, project } = row else {
                continue;
            };
            let (name, port, _) = self.row_source(server, project);
            let id = self.row_key(server, project);
            let (body, editor) = row_buttons(rect);
            let body_part = Part::Row(row, Zone::Body);
            let editor_part = Part::Row(row, Zone::Editor);
            let verb = if server.is_some() { "Open" } else { "Start" };
            controls.push((
                (id.clone(), Zone::Body),
                body_part,
                pixels(body),
                format!("{verb} {name} on port {port}"),
                self.enabled(body_part),
            ));
            controls.push((
                (id, Zone::Editor),
                editor_part,
                pixels(editor),
                format!("Open {name} in VS Code"),
                self.enabled(editor_part),
            ));
        }
        self.controls.sync(self.menu, controls);
        self.focus_first_available();
    }

    fn focus_first_available(&mut self) {
        if self.focus == MenuFocus::Hover || unsafe { GetForegroundWindow() } != self.menu {
            return;
        }
        let focused = self.controls.focused();
        if self.focus == MenuFocus::FindingFirst {
            // Give Start an immediately usable action. An earlier starting row's ribbon may
            // become the first action as its folder is checked, until the user navigates.
            let mut pending = false;
            let first = self.menu_rows.iter().find_map(|&row| {
                if !matches!(row, Row::Item { .. }) {
                    return None;
                }
                let action = [Zone::Body, Zone::Editor]
                    .map(|zone| Part::Row(row, zone))
                    .into_iter()
                    .find(|&part| self.enabled(part));
                if action.is_none()
                    && (self.editor.exe.is_some()
                        || self.editor_scans.contains_key(&EditorSource::Installation))
                {
                    pending |= self
                        .row_dir(row)
                        .is_some_and(|dir| !self.editor.folders.contains_key(dir));
                }
                action
            });
            if !pending {
                self.focus = MenuFocus::Keyboard;
            }
            if focused.is_some() && focused == first {
                return;
            }
        } else if focused.is_some() {
            return;
        }
        if matches!(self.pressed, Some((_, Input::Key(_)))) {
            self.pressed = None;
        }
        unsafe { SetFocus(self.menu) };
        self.controls.step(false);
        self.invalidate(self.menu);
    }

    fn key(&mut self, key: u16, down: bool, repeat: bool) {
        if self.focus == MenuFocus::Hover || unsafe { GetForegroundWindow() } != self.menu {
            return;
        }
        if down {
            self.focus = MenuFocus::Keyboard;
        }
        match key {
            VK_ESCAPE if down => self.hide_menu(),
            VK_TAB if down => {
                self.pressed = None;
                self.controls
                    .step(unsafe { GetKeyState(i32::from(VK_SHIFT)) } < 0);
            }
            VK_RETURN | VK_SPACE if down && !repeat => {
                self.pressed = self.controls.focused().map(|part| (part, Input::Key(key)));
            }
            VK_RETURN | VK_SPACE if !down => return self.release(Input::Key(key)),
            _ => {}
        }
        self.invalidate(self.menu);
    }

    /// Closes the normal menu. The toggle view closes through `close_toggle`.
    fn hide_menu(&mut self) {
        self.cancel_hover_timer();
        if self.mode == Mode::Normal && !self.menu_rows.is_empty() {
            self.focus = MenuFocus::Hover;
            self.pressed = None;
            unsafe { ShowWindow(self.menu, SW_HIDE) };
            self.controls.clear();
            self.editor = EditorAvailability::default();
            for scan in self.editor_scans.values_mut() {
                scan.from_closed_menu = true;
            }
            self.menu_rows.clear();
            if matches!(self.hover, Some(Part::Row(..))) {
                self.set_hover(None);
            }
            self.invalidate(self.view);
        }
    }

    /// Right-click > Defaults.
    fn toggle_defaults(&mut self) {
        if self.mode == Mode::Toggle {
            return self.close_toggle();
        }
        self.hide_menu();
        self.mode = Mode::Toggle;
        self.hook = unsafe {
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), GetModuleHandleW(null()), 0)
        };
        self.show_menu();
    }

    fn close_toggle(&mut self) {
        self.end_drag();
        unsafe { UnhookWindowsHookEx(self.hook) };
        self.hook = null_mut();
        self.mode = Mode::Normal;
        self.hide_menu();
    }

    /// The hook already excluded the panel and its popup menu.
    fn button_down(&mut self, button: u32, target: HWND) {
        let right_on_strip = button == WM_RBUTTONDOWN && target == self.view;
        if self.mode == Mode::Toggle && !right_on_strip {
            self.close_toggle();
        }
    }

    /// Moves the dragged default to the last default that starts above `y`.
    fn drag_to(&mut self, y: f32) {
        let Some(from) = self.drag else {
            return;
        };
        let split = self.projects.split();
        let to = self
            .slots()
            .filter_map(|(row, rect)| match row {
                Row::Item {
                    project: Some(p), ..
                } if p < split && rect[1] <= y => Some(p),
                _ => None,
            })
            .last()
            .unwrap_or(0);
        if to != from {
            self.projects.reorder(from, to);
            self.drag = Some(to);
            self.menu_rows = self.rows();
            self.invalidate(self.menu);
        }
    }

    fn end_drag(&mut self) {
        if self.drag.take().is_none() {
            return;
        }
        self.projects.save();
        self.rehover();
        // Last, since this sends WM_CAPTURECHANGED.
        unsafe {
            if GetCapture() == self.menu {
                ReleaseCapture();
            }
        }
    }

    fn is_default(&self, project: Option<usize>) -> bool {
        project.is_some_and(|p| self.projects.list()[p].default)
    }

    fn is_starting(&self, project: usize) -> bool {
        let dir = &self.projects.list()[project].launch.dir;
        self.starting
            .iter()
            .any(|start| start.launch.dir.eq_ignore_ascii_case(dir))
    }

    fn can_clear(&self, project: Option<usize>) -> bool {
        project.is_some_and(|p| {
            let project = &self.projects.list()[p];
            !project.default && project.server.is_none() && !self.is_starting(p)
        })
    }

    /// What a view lists, top to bottom, with a line between groups that have rows.
    fn rows(&self) -> Vec<Row> {
        let list = self.projects.list();
        let split = self.projects.split();
        let toggle = self.mode == Mode::Toggle;
        let mut rows = Vec::new();
        if toggle {
            for (p, project) in list.iter().enumerate().skip(split) {
                if project.server.is_none() {
                    rows.push(Row::Item {
                        server: None,
                        project: Some(p),
                    });
                }
            }
        }
        // The servers no default owns, each with its folder's saved record.
        for (i, server) in self.servers.iter().enumerate() {
            if list[..split].iter().any(|p| p.server == Some(i)) {
                continue;
            }
            let project = (split..list.len()).find(|&p| server.in_folder(&list[p].launch.dir));
            if !toggle || project.is_some() {
                rows.push(Row::Item {
                    server: Some(i),
                    project,
                });
            }
        }
        if split > 0 && !rows.is_empty() {
            rows.push(Row::Line);
        }
        rows.extend(
            list[..split]
                .iter()
                .enumerate()
                .map(|(p, project)| Row::Item {
                    server: project.server,
                    project: Some(p),
                }),
        );
        if toggle {
            if !rows.is_empty() {
                rows.push(Row::Line);
            }
            rows.push(Row::Close);
        }
        rows
    }

    /// The open view's rows and where each one is.
    fn slots(&self) -> impl Iterator<Item = (Row, Rect)> + '_ {
        let inset = menu::BORDER + menu::PAD;
        let mut top = inset;
        self.menu_rows.iter().map(move |&row| {
            let height = match row {
                Row::Line => 2.0 * menu::LINE_MARGIN + menu::LINE_H,
                Row::Close | Row::Item { .. } => menu::ROW_H,
            };
            let rect = [inset, top, self.menu_w - inset, top + height];
            top += height + menu::ROW_GAP;
            (row, rect)
        })
    }

    fn row_source(
        &self,
        server: Option<usize>,
        project: Option<usize>,
    ) -> (&str, u16, Option<&servers::Launch>) {
        if let Some(i) = server {
            let running = &self.servers[i];
            (running.name.as_str(), running.port, running.launch.as_ref())
        } else if let Some(p) = project {
            let saved = &self.projects.list()[p];
            (saved.launch.name(), saved.port, Some(&saved.launch))
        } else {
            ("", 0, None)
        }
    }

    /// An item's name, branch, port and text style.
    fn item(&self, server: Option<usize>, project: Option<usize>) -> (&str, String, String, Style) {
        let (name, port, launch) = self.row_source(server, project);
        let style = if self.is_default(project) {
            MENU_TEXT
        } else {
            MENU_ITALIC
        };
        let branch = launch
            .and_then(|launch| self.branches.get(&launch.dir))
            .map_or_else(String::new, |branch| format!(" · {branch}"));
        (name, branch, format!(":{port}"), style)
    }

    /// A row's full content width in the current view, before side padding.
    fn row_width(&self, row: Row) -> f32 {
        let (server, project) = match row {
            Row::Item { server, project } => (server, project),
            Row::Close => return self.renderer.measure("Close defaults", MENU_TEXT),
            Row::Line => return 0.0,
        };
        let (name, branch, port, style) = self.item(server, project);
        let mut width = self.renderer.measure(name, style)
            + self.renderer.measure(&branch, MENU_ITALIC)
            + menu::NAME_TO_PORT_MIN
            + self.renderer.measure(&port, style);
        if self.mode == Mode::Toggle {
            width += 2.0 * (font::ACTION_SIZE + menu::ICON_TO_TEXT);
        } else {
            // The button supplies the right padding already counted by show_menu.
            width += menu::EDITOR_W - menu::ROW_PAD_X;
        }
        width
    }

    fn highlight(&self, pressed: bool, rect: Rect, radius: f32) {
        let alpha = if pressed {
            color::PRESSED_ALPHA
        } else {
            color::HOVER_ALPHA
        };
        self.renderer.fill(rect, radius, color::HOVER, alpha);
    }

    fn paint_strip(&self, dc: HDC, size: (i32, i32)) {
        let (r, l) = (&self.renderer, &self.layout);
        r.paint(dc, size, self.dpi, Some(color::TASKBAR), || {
            if self.hover == Some(Part::Label) || self.menu_open() {
                let pressed = self.pressed.is_some_and(|(part, _)| part == Part::Label);
                self.highlight(pressed, l.label, strip::HIT_RADIUS);
            }
            let server = self.servers.get(self.current);
            let ink = if server.is_some() {
                color::TEXT
            } else {
                color::GLOBE_EMPTY
            };
            r.text(font::GLOBE, ICON, GLOBE, Align::Leading, ink);
            let Some(server) = server else {
                return;
            };
            if self.servers.len() > 1 {
                // Right-aligned in the widest count's box, so the total stays put.
                let count = format!("{}/{}", self.current + 1, self.servers.len());
                let rect = [TEXT_X, 0.0, l.name_x - strip::COUNT_TO_NAME, strip::HEIGHT];
                r.text(&count, COUNT_TEXT, rect, Align::Trailing, color::TEXT_DIM);
            }
            let rect = [l.name_x, 0.0, l.name_x + l.name_max, strip::HEIGHT];
            r.text(&server.name, STRIP_TEXT, rect, Align::Leading, color::TEXT);
            let rect = [
                l.label[0],
                0.0,
                l.label[2] - strip::LABEL_PAD_X,
                strip::HEIGHT,
            ];
            let port = format!(":{}", server.port);
            r.text(&port, STRIP_TEXT, rect, Align::Trailing, color::TEXT_DIM);
        });
    }

    fn paint_menu(&self) {
        unsafe {
            ValidateRect(self.menu, null());
            let mut rect = zeroed();
            GetWindowRect(self.menu, &mut rect);
            self.present_menu(rect);
        }
    }

    fn present_menu(&self, rect: RECT) -> bool {
        let r = &self.renderer;
        let height = (rect.bottom - rect.top) as f32 / self.scale();
        r.present(self.menu, rect, self.dpi, || {
            let outside = [0.0, 0.0, self.menu_w, height];
            r.fill(outside, strip::HIT_RADIUS, color::MENU_BORDER, 1.0);
            let inside = [
                menu::BORDER,
                menu::BORDER,
                self.menu_w - menu::BORDER,
                height - menu::BORDER,
            ];
            r.fill(inside, strip::HIT_RADIUS - menu::BORDER, color::MENU, 1.0);
            let focus = self.controls.focused();
            for (row, rect) in self.slots() {
                let (server, project) = match row {
                    Row::Item { server, project } => (server, project),
                    _ => (None, None),
                };
                let hovered = matches!(self.hover, Some(Part::Row(hovered, _)) if hovered == row);
                let focused_zone = match focus {
                    Some(Part::Row(focused_row, zone)) if focused_row == row => Some(zone),
                    _ => None,
                };
                let lit = hovered
                    || focused_zone.is_some()
                    || (project.is_some() && project == self.drag);
                if lit {
                    let pressed = matches!(self.pressed, Some((Part::Row(r, zone), _))
                        if r == row && zone != Zone::Editor);
                    self.highlight(pressed, rect, menu::ROW_RADIUS);
                }
                let mut content = [
                    rect[0] + menu::ROW_PAD_X,
                    rect[1],
                    rect[2] - menu::ROW_PAD_X,
                    rect[3],
                ];
                let accent = if lit { color::TEXT } else { color::TEXT_DIM };
                match row {
                    Row::Line => {
                        let top = rect[1] + menu::LINE_MARGIN;
                        let line = [content[0], top, content[2], top + menu::LINE_H];
                        r.fill(line, 0.0, color::MENU_BORDER, 1.0);
                    }
                    Row::Close => {
                        r.text("Close defaults", MENU_TEXT, content, Align::Center, accent);
                    }
                    Row::Item { .. } => {
                        if self.mode == Mode::Normal {
                            let (body, editor) = row_buttons(rect);
                            content[2] = editor[0];
                            self.paint_editor(row, editor, focused_zone == Some(Zone::Editor));
                            if let Some(zone) = focused_zone {
                                let button = if zone == Zone::Editor { editor } else { body };
                                r.outline(
                                    button,
                                    menu::ROW_RADIUS,
                                    menu::FOCUS_STROKE,
                                    color::TEXT_DIM,
                                );
                            }
                        }
                        self.paint_item(server, project, content, accent);
                    }
                }
            }
        })
    }

    fn paint_editor(&self, row: Row, rect: Rect, focused: bool) {
        let part = Part::Row(row, Zone::Editor);
        let lit = (self.hover == Some(part) || focused) && self.enabled(part);
        if lit {
            let plate = [
                rect[0] + menu::EDITOR_INSET_X,
                rect[1] + menu::EDITOR_INSET_Y,
                rect[2] - menu::EDITOR_INSET_X,
                rect[3] - menu::EDITOR_INSET_Y,
            ];
            let pressed = self.pressed.is_some_and(|(down, _)| down == part);
            let (color, alpha) = if pressed {
                (color::EDITOR_PRESSED, color::EDITOR_PRESSED_ALPHA)
            } else {
                (color::HOVER, color::EDITOR_HOVER_ALPHA)
            };
            self.renderer.fill(plate, menu::EDITOR_RADIUS, color, alpha);
        }
        let ink = if lit { color::TEXT } else { color::TEXT_DIM };
        self.renderer.vscode_ribbon(
            rect[0] + menu::ROW_PAD_X,
            (rect[1] + rect[3] - menu::EDITOR_ICON) / 2.0,
            menu::EDITOR_ICON,
            ink,
        );
    }

    /// `accent` is the color of the toggle view's icons.
    fn paint_item(
        &self,
        server: Option<usize>,
        project: Option<usize>,
        content: Rect,
        accent: Rgb,
    ) {
        let r = &self.renderer;
        let (name, branch, port, style) = self.item(server, project);
        let ink = if server.is_some() {
            color::TEXT
        } else {
            color::STOPPED
        };
        let mut text = content;
        if self.mode == Mode::Toggle {
            let default = self.is_default(project);
            let action = if default { font::REMOVE } else { font::ADD };
            r.text(action, ACTION_ICON, content, Align::Trailing, accent);
            text[0] += font::ACTION_SIZE + menu::ICON_TO_TEXT;
            text[2] -= font::ACTION_SIZE + menu::ICON_TO_TEXT;
            if default {
                let left = content[0] + (font::ACTION_SIZE - menu::HANDLE_W) / 2.0;
                let top = (content[1] + content[3] - menu::HANDLE_H) / 2.0;
                for bar in menu::HANDLE_BARS.map(|center| top + center - menu::LINE_H / 2.0) {
                    let rect = [left, bar, left + menu::HANDLE_W, bar + menu::LINE_H];
                    r.fill(rect, menu::LINE_H / 2.0, accent, 1.0);
                }
            } else if self.can_clear(project) {
                let icon = [
                    content[0],
                    content[1],
                    content[0] + font::ACTION_SIZE,
                    content[3],
                ];
                r.text(font::CLEAR, ACTION_ICON, icon, Align::Center, accent);
            }
        }
        r.text(name, style, text, Align::Leading, ink);
        if !branch.is_empty() {
            let mut branch_rect = text;
            branch_rect[0] += r.measure(name, style);
            r.text(&branch, MENU_ITALIC, branch_rect, Align::Leading, ink);
        }
        r.text(&port, style, text, Align::Trailing, ink);
        if server.is_none() && project.is_some_and(|p| self.is_starting(p)) {
            let right = text[2] - r.measure(&port, style) - menu::ICON_TO_TEXT;
            let center = (right - menu::SPINNER / 2.0, (text[1] + text[3]) / 2.0);
            // From the clock, so the spinner holds no state.
            let now = SystemTime::now().duration_since(UNIX_EPOCH);
            let turn = now.unwrap_or_default().as_millis() % u128::from(menu::SPINNER_TURN_MS);
            let angle = 360.0 * turn as f32 / menu::SPINNER_TURN_MS as f32;
            r.arc(
                center,
                menu::SPINNER_RADIUS,
                angle,
                menu::SPINNER_SWEEP,
                menu::SPINNER_STROKE,
                color::TEXT_DIM,
            );
        }
    }
}

/// One split for painting, pointer hits and native button bounds.
fn row_buttons([left, top, right, bottom]: Rect) -> (Rect, Rect) {
    let split = right - menu::EDITOR_W;
    ([left, top, split, bottom], [split, top, right, bottom])
}

unsafe fn paint(hwnd: HWND, draw: fn(&State, HDC, (i32, i32))) {
    unsafe {
        let mut ps: PAINTSTRUCT = zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut rect: RECT = zeroed();
        GetClientRect(hwnd, &mut rect);
        with(|state| draw(state, dc, (rect.right, rect.bottom)));
        EndPaint(hwnd, &ps);
    }
}

fn track_leave(hwnd: HWND) {
    let mut track = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    unsafe { TrackMouseEvent(&mut track) };
}

unsafe extern "system" fn foreground_changed(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    let menu = PANEL.get();
    unsafe {
        if IsWindowVisible(menu) != 0 {
            // Explorer finishes activating the taskbar before we restore the menu's order.
            PostMessageW(menu, FOREGROUND_CHANGED, 0, 0);
        }
    }
}

fn raise_for_taskbar(menu: HWND, view: HWND, foreground: HWND) {
    unsafe {
        if !foreground.is_null()
            && foreground == GetAncestor(view, GA_ROOT)
            && IsWindowVisible(menu) != 0
        {
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            SetWindowPos(menu, HWND_TOPMOST, 0, 0, 0, 0, flags);
        }
    }
}

/// Installed only while the toggle view is open: the panel never takes focus,
/// so nothing else tells it about a click outside.
unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        let down = matches!(
            w as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        );
        if code >= 0 && down {
            // Capture the actual click before the pointer moves or the host's
            // popup menu closes. The posted message can be handled later.
            let target = WindowFromPoint((*(l as *const MSLLHOOKSTRUCT)).pt);
            let panel = PANEL.get();
            if target != panel {
                let mut class = [0u16; 8];
                let length = GetClassNameW(target, class.as_mut_ptr(), class.len() as i32);
                let popup = class[..length as usize]
                    .iter()
                    .copied()
                    .eq(POPUP_MENU_CLASS.encode_utf16())
                    && GetWindowThreadProcessId(target, null_mut())
                        == GetWindowThreadProcessId(panel, null_mut());
                if !popup {
                    PostMessageW(panel, BUTTON_DOWN, w, target as LPARAM);
                }
            }
        }
        CallNextHookEx(null_mut(), code, w, l)
    }
}

unsafe extern "system" fn view_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_PAINT => paint(hwnd, State::paint_strip),
            WM_ERASEBKGND => return 1,
            WM_MOUSEMOVE => {
                track_leave(hwnd);
                with(|state| state.strip_move(state.point(l)));
            }
            WM_MOUSELEAVE => _ = with(State::leave),
            WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONUP => {
                with(|state| state.mouse_button(hwnd, message, l));
            }
            WM_MOUSEWHEEL => _ = with(|state| state.scroll((w >> 16) as i16 as i32)),
            // The host's menu runs inside DefWindowProcW, and the pointer may
            // be elsewhere by the time it closes.
            WM_CONTEXTMENU => {
                with(State::hide_menu);
                let result = DefWindowProcW(hwnd, message, w, l);
                with(State::rehover);
                return result;
            }
            WM_NCDESTROY => {
                with(|state| {
                    if state.view == hwnd {
                        state.menu_request = None;
                        state.close_toggle();
                        state.view = null_mut();
                        state.hover = None;
                        state.pressed = None;
                        state.cancel_hover_timer();
                    }
                });
                return DefWindowProcW(hwnd, message, w, l);
            }
            // DefWindowProcW passes mouse activation and right-clicks up to the
            // host's container, which activates the taskbar and shows the menu.
            _ => return DefWindowProcW(hwnd, message, w, l),
        }
        0
    }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_PAINT => _ = with(|state| state.paint_menu()),
            WM_ERASEBKGND => return 1,
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
            WM_MOUSEMOVE => {
                track_leave(hwnd);
                with(|state| state.menu_move(state.point(l)));
            }
            WM_MOUSELEAVE => _ = with(State::leave),
            WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONUP => {
                with(|state| state.mouse_button(hwnd, message, l));
            }
            WM_KEYDOWN | WM_KEYUP => {
                with(|state| state.key(w as u16, message == WM_KEYDOWN, l & (1 << 30) != 0));
            }
            WM_DRAWITEM => return 1, // Buttons are painted with the rest of the layered menu.
            WM_COMMAND => {
                with(|state| {
                    let Some(part) = state.controls.action(l as HWND) else {
                        return;
                    };
                    match (w >> 16) as u32 {
                        BN_CLICKED => state.activate(part),
                        BN_SETFOCUS => {
                            if GetForegroundWindow() == hwnd {
                                state.focus = MenuFocus::Keyboard;
                                state.cancel_hover_timer();
                            }
                            state.invalidate(hwnd);
                        }
                        BN_KILLFOCUS => {
                            if matches!(state.pressed, Some((held, Input::Key(_))) if held == part)
                            {
                                state.pressed = None;
                            }
                            state.invalidate(hwnd);
                        }
                        _ => {}
                    }
                });
            }
            WM_ACTIVATE if (w as u16) == WA_INACTIVE as u16 => {
                with(|state| {
                    if state.focus != MenuFocus::Hover {
                        state.hide_menu();
                    }
                });
                return DefWindowProcW(hwnd, message, w, l);
            }
            instance::SHOW_MENU => {
                with(|state| {
                    let request = instance::MenuRequest::from_message(w, l);
                    if request.is_fresh() {
                        state.menu_request = Some(request);
                        state.try_menu_request();
                    }
                });
            }
            VIEW_READY => _ = with(State::try_menu_request),
            EDITOR_READY => _ = with(State::editor_ready),
            WM_CAPTURECHANGED => _ = with(State::end_drag),
            BUTTON_DOWN => _ = with(|state| state.button_down(w as u32, l as HWND)),
            FOREGROUND_CHANGED => {
                let foreground = GetForegroundWindow();
                with(|state| {
                    if state.focus != MenuFocus::Hover
                        && !foreground.is_null()
                        && foreground != hwnd
                    {
                        state.hide_menu();
                    } else {
                        raise_for_taskbar(hwnd, state.view, foreground);
                    }
                });
            }
            // Right-click anywhere opens the host's menu, which the strip forwards.
            WM_CONTEXTMENU => {
                if let Some(view) = with(|state| state.view).filter(|view| !view.is_null()) {
                    return SendMessageW(view, message, view as WPARAM, l);
                }
            }
            WM_TIMER if w == POLL => _ = with(State::refresh),
            WM_TIMER if w == HOVER => _ = with(State::finish_hover),
            WM_TIMER if w == SPIN => _ = with(|state| state.spin()),
            WM_CLOSE => {
                EndMenu();
                PostQuitMessage(0);
            }
            _ => return DefWindowProcW(hwnd, message, w, l),
        }
        0
    }
}
