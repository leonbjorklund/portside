//! The strip in the taskbar and the server menu above it. Everything runs on
//! the UI thread. Window procedures reach the shared state through `with`.

use crate::{
    projects::Projects,
    render::{Align, Face, Rect, Renderer, Style},
    servers::{self, Scanner, Server, Starting},
    theme::{Rgb, color, font, menu, strip},
    wide,
};
use std::{
    cell::{Cell, RefCell},
    mem::{take, zeroed},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    time::{SystemTime, UNIX_EPOCH},
};
use win_taskbar_host::{Content, Surface, TaskbarHost};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
        },
        Gdi::{
            BeginPaint, EndPaint, HDC, InvalidateRect, PAINTSTRUCT, ScreenToClient, ValidateRect,
        },
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::WM_MOUSELEAVE,
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{
            GetCapture, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
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
const SHOW: usize = 2;
const SPIN: usize = 3;
const POLL_MS: u32 = 1000;
/// Posted to the menu when a mouse button goes down anywhere.
const BUTTON_DOWN: u32 = WM_APP;

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

/// The part of a toggle view row under the pointer. The Close row is all `Action`.
#[derive(Clone, Copy, PartialEq)]
enum Zone {
    Body,
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

/// Strip geometry for the current server, kept for painting and hit tests.
#[derive(Default)]
struct Layout {
    /// The label's hover rect; empty with no servers and no defaults.
    label: Rect,
    /// Where the name starts, and the width it gets before it is cut.
    name_x: f32,
    name_max: f32,
    /// The label's width with the widest server, so the menu keeps one width while cycling.
    label_max_w: f32,
}

struct State {
    renderer: Renderer,
    scanner: Scanner,
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
    menu_w: f32,
    mode: Mode,
    /// The mouse hook, while the toggle view is open.
    hook: HHOOK,
    dpi: u32,
    hover: Option<Part>,
    pressed: Option<Part>,
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
    /// The menu, for the mouse hook, which can run while `STATE` is borrowed.
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

pub fn run() -> Result<(), String> {
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
        // DWM rounds the corners and adds the shadow. Its border matches the
        // one paint_menu draws, for systems where DWM keeps corners square.
        let border = color::MENU_BORDER.swap_bytes() >> 8; // COLORREF is 0x00BBGGRR
        for (attribute, value) in [
            (DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND as u32),
            (DWMWA_BORDER_COLOR, border),
        ] {
            DwmSetWindowAttribute(menu, attribute as u32, (&raw const value).cast(), 4);
        }
        SetTimer(menu, POLL, POLL_MS, None);
    }
    PANEL.set(menu);
    STATE.set(Some(State {
        renderer,
        scanner: Scanner::default(),
        servers: Vec::new(),
        projects: Projects::load(),
        starting: Vec::new(),
        current: 0,
        chosen: None,
        layout: Layout::default(),
        view: null_mut(),
        menu,
        menu_w: 0.0,
        mode: Mode::Normal,
        hook: null_mut(),
        dpi: 96,
        hover: None,
        pressed: None,
        drag: None,
        wheel: 0,
        full_width,
        host: None,
    }));
    with(|state| {
        let servers = state.scanner.scan();
        state.apply(servers);
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
    unsafe { DestroyWindow(menu) };
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
            // A server's name width limit and the label's right edge when it shows.
            let fit = |server: &Server| {
                let port_w = self
                    .renderer
                    .measure(&format!(":{}", server.port), STRIP_TEXT);
                let name_max = strip::NAME_SLOT_W - strip::NAME_TO_PORT - port_w;
                let name_w = self
                    .renderer
                    .measure(&server.name, STRIP_TEXT)
                    .min(name_max);
                let right = name_x + name_w + strip::NAME_TO_PORT + port_w + strip::LABEL_PAD_X;
                (name_max, right)
            };
            let (name_max, right) = fit(server);
            let widest = self.servers.iter().map(|s| fit(s).1).fold(0.0, f32::max);
            layout.label = [strip::LEAD, LABEL_TOP, right, LABEL_TOP + strip::HIT_H];
            layout.name_x = name_x;
            layout.name_max = name_max;
            layout.label_max_w = widest - strip::LEAD;
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
        let servers = self.scanner.scan();
        let starting = self.starting.len();
        let mut started = Vec::new();
        for start in take(&mut self.starting) {
            match servers.iter().find(|s| s.runs(&start.launch, start.port)) {
                Some(server) => started.push(server.clone()), // the handle closes, the server runs on
                None if start.expired() => start.end(),
                None => self.starting.push(start),
            }
        }
        if let Some(last) = started.last() {
            self.chosen = Some(last.clone());
        }
        if servers != self.servers {
            self.apply(servers);
        } else if self.starting.len() != starting {
            self.rehover();
        }
        for server in &started {
            self.open(server);
        }
    }

    /// Takes in a new scan and repaints.
    fn apply(&mut self, servers: Vec<Server>) {
        // Keep the selection if a server changes its address within the same family.
        let index = |target: Option<&Server>| {
            target.and_then(|t| {
                servers.iter().position(|s| {
                    (s.port, &s.name, s.address.is_ipv6()) == (t.port, &t.name, t.address.is_ipv6())
                })
            })
        };
        let kept = index(self.chosen.as_ref()).or_else(|| index(self.servers.get(self.current)));
        self.current = kept.unwrap_or(self.current.min(servers.len().saturating_sub(1)));
        self.servers = servers;
        self.projects.observe(&self.servers);
        self.pressed = None;
        self.relayout();
        self.fit();
        if self.menu_open() {
            self.show_menu();
        }
        self.rehover();
    }

    fn step(&mut self, by: isize) {
        let n = self.servers.len() as isize;
        if n > 1 {
            self.choose((self.current as isize + by).rem_euclid(n) as usize);
        }
    }

    fn choose(&mut self, i: usize) {
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
            self.pressed = None;
        }
        changed
    }

    fn strip_move(&mut self, at: (f32, f32)) {
        let part = contains(self.layout.label, at).then_some(Part::Label);
        if !self.set_hover(part) {
            return;
        }
        unsafe {
            if part.is_none() {
                KillTimer(self.menu, SHOW);
                self.hide_menu();
            } else if !self.menu_open() {
                SetTimer(self.menu, SHOW, menu::SHOW_DELAY_MS, None);
            }
        }
        self.redraw();
    }

    fn menu_move(&mut self, at: (f32, f32)) {
        if self.drag.is_some() {
            return self.drag_to(at.1);
        }
        let toggle = self.mode == Mode::Toggle;
        let part = self
            .slots()
            .into_iter()
            .find(|&(_, rect)| contains(rect, at))
            .and_then(|(row, rect)| {
                let zone = match row {
                    Row::Close => Zone::Action,
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

    /// The window under the pointer, and the pointer in `client`'s DIPs.
    fn pointer(&self, client: HWND) -> (HWND, (f32, f32)) {
        unsafe {
            let mut cursor = zeroed();
            GetCursorPos(&mut cursor);
            let under = WindowFromPoint(cursor);
            ScreenToClient(client, &mut cursor);
            let scale = self.scale();
            (under, (cursor.x as f32 / scale, cursor.y as f32 / scale))
        }
    }

    /// The rows or the strip may have changed under a still pointer.
    fn rehover(&mut self) {
        let (under, at) = self.pointer(self.view);
        if under == self.menu {
            self.menu_move(self.pointer(self.menu).1);
        } else if under == self.view {
            self.strip_move(at);
        } else {
            self.set_hover(None);
        }
        self.redraw();
    }

    /// The pointer left the strip or the menu. The menu stays open only while
    /// the pointer is on it or on the label.
    fn leave(&mut self, from_menu: bool) {
        unsafe { KillTimer(self.menu, SHOW) };
        self.wheel = 0;
        if self
            .hover
            .is_some_and(|part| matches!(part, Part::Row(..)) == from_menu)
        {
            self.set_hover(None);
            self.redraw();
        }
        let (under, at) = self.pointer(self.view);
        if under != self.menu && !contains(self.layout.label, at) {
            self.hide_menu();
        }
    }

    /// `left` is the left button, otherwise the middle one. Only what reacts
    /// to that button is pressed, so `stop` can't act on a stale target.
    fn press(&mut self, left: bool) {
        self.pressed = match self.hover {
            Some(Part::Row(_, Zone::Body)) if self.mode == Mode::Toggle => None,
            Some(Part::Row(_, Zone::Action | Zone::Clear)) if !left => None,
            Some(Part::Row(Row::Item { project, .. }, Zone::Handle)) => {
                if left {
                    self.drag = project;
                    self.hover = None;
                    unsafe { SetCapture(self.menu) };
                }
                None
            }
            hover => hover,
        };
        self.redraw();
    }

    fn release(&mut self) {
        if self.drag.is_some() {
            return self.end_drag();
        }
        let Some(part) = self.pressed.take() else {
            return;
        };
        match part {
            Part::Label => {
                if let Some(server) = self.servers.get(self.current) {
                    self.open(server);
                }
            }
            Part::Row(Row::Close, _) => self.close_toggle(),
            Part::Row(
                Row::Item {
                    server,
                    project: Some(p),
                },
                zone @ (Zone::Action | Zone::Clear),
            ) => {
                if zone == Zone::Action {
                    let default = !self.projects.list()[p].default;
                    self.projects.set_default(p, default, server, &self.servers);
                } else if self.can_clear(Some(p)) {
                    self.projects.remove_past(p);
                }
                self.relayout();
                self.show_menu();
                self.rehover();
            }
            Part::Row(
                Row::Item {
                    server: Some(i), ..
                },
                _,
            ) => {
                self.choose(i);
                self.open(&self.servers[i]);
            }
            Part::Row(
                Row::Item {
                    project: Some(p), ..
                },
                _,
            ) if !self.is_starting(p) => {
                let saved = &self.projects.list()[p];
                if let Some(start) = servers::start(&saved.launch, saved.port) {
                    self.starting.push(start);
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

    /// Middle-click: stops the label's or the row's server. It leaves the
    /// strip on the first poll after it exits.
    fn stop(&mut self) {
        let Some(part) = self.pressed.take() else {
            return;
        };
        let i = match part {
            Part::Label => Some(self.current),
            Part::Row(Row::Item { server, .. }, _) => server,
            Part::Row(..) => None,
        };
        if let Some(server) = i.and_then(|i| self.servers.get(i)) {
            self.scanner.stop(server);
        }
        self.redraw();
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
        let items = self
            .rows(Mode::Normal)
            .iter()
            .filter(|row| matches!(row, Row::Item { .. }))
            .count();
        if (self.mode == Mode::Normal && items <= usize::from(!self.servers.is_empty()))
            || self.view.is_null()
        {
            return self.hide_menu();
        }
        let widest = |mode| {
            self.rows(mode)
                .into_iter()
                .map(|row| self.row_width(mode, row))
                .fold(0.0, f32::max)
        };
        let inset = menu::BORDER + menu::PAD;
        // Both views' rows, so switching between them keeps one width. At least
        // as wide as the label with any server, so the pointer can move
        // straight up into it and cycling never resizes it.
        let width = (widest(Mode::Normal).max(widest(Mode::Toggle))
            + 2.0 * (menu::ROW_PAD_X + inset))
            .max(menu::MIN_W)
            .max(self.layout.label_max_w);
        let scale = self.scale();
        let w = (width * scale).ceil() as i32;
        self.menu_w = w as f32 / scale;
        let height = self.slots().last().map_or(0.0, |&(_, rect)| rect[3]) + inset;
        let h = (height * scale).ceil() as i32;
        unsafe {
            let mut view: RECT = zeroed();
            GetWindowRect(self.view, &mut view);
            // Bottom edge on the top of the label's hover rect, left edges aligned.
            let left = view.left + (strip::LEAD * scale).round() as i32;
            let bottom = view.top + (LABEL_TOP * scale).ceil() as i32;
            let rect = RECT {
                left,
                top: bottom - h,
                right: left + w,
                bottom,
            };
            if self.present_menu(rect) {
                let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
                SetWindowPos(self.menu, HWND_TOPMOST, 0, 0, 0, 0, flags);
            }
        }
        // The label keeps its highlight while the menu is open.
        self.redraw();
    }

    /// Closes the normal menu. The toggle view closes through `close_toggle`.
    fn hide_menu(&mut self) {
        if self.mode == Mode::Normal && self.menu_open() {
            unsafe { ShowWindow(self.menu, SW_HIDE) };
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
        self.mode = Mode::Toggle;
        self.hook = unsafe {
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), GetModuleHandleW(null()), 0)
        };
        self.show_menu();
        self.rehover();
    }

    fn close_toggle(&mut self) {
        self.end_drag();
        unsafe { UnhookWindowsHookEx(self.hook) };
        self.hook = null_mut();
        self.mode = Mode::Normal;
        self.hide_menu();
    }

    /// A mouse button went down somewhere. Clicking outside closes the toggle
    /// view, but its right-click menu and the right-click that opens it don't.
    fn button_down(&mut self, button: u32) {
        let (under, _) = self.pointer(self.view);
        let mut class = [0u16; 8];
        let length = unsafe { GetClassNameW(under, class.as_mut_ptr(), class.len() as i32) };
        let popup = String::from_utf16_lossy(&class[..length as usize]) == POPUP_MENU_CLASS;
        let right_on_strip = button == WM_RBUTTONDOWN && under == self.view;
        if self.mode == Mode::Toggle && under != self.menu && !popup && !right_on_strip {
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
            .iter()
            .rev()
            .find_map(|&(row, rect)| match row {
                Row::Item {
                    project: Some(p), ..
                } if p < split && rect[1] <= y => Some(p),
                _ => None,
            })
            .unwrap_or(0);
        if to != from {
            self.projects.reorder(from, to);
            self.drag = Some(to);
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
        self.starting.iter().any(|start| start.launch.dir == *dir)
    }

    fn can_clear(&self, project: Option<usize>) -> bool {
        project.is_some_and(|p| {
            let project = &self.projects.list()[p];
            !project.default && project.server.is_none() && !self.is_starting(p)
        })
    }

    /// What a view lists, top to bottom, with a line between groups that have rows.
    fn rows(&self, mode: Mode) -> Vec<Row> {
        let list = self.projects.list();
        let split = self.projects.split();
        let defaults = (0..split).map(|p| Row::Item {
            server: list[p].server,
            project: Some(p),
        });
        // The servers no default owns, each with its folder's saved record.
        let running = (0..self.servers.len())
            .filter(|&i| !list[..split].iter().any(|p| p.server == Some(i)))
            .map(|i| Row::Item {
                server: Some(i),
                project: (split..list.len())
                    .find(|&p| self.servers[i].in_folder(&list[p].launch.dir)),
            });
        let groups: [Vec<Row>; 3] = match mode {
            Mode::Normal => [running.collect(), defaults.collect(), Vec::new()],
            Mode::Toggle => {
                let past = (split..list.len())
                    .filter(|&p| list[p].server.is_none())
                    .map(|p| Row::Item {
                        server: None,
                        project: Some(p),
                    });
                let running = running.filter(|row| {
                    matches!(
                        row,
                        Row::Item {
                            project: Some(_),
                            ..
                        }
                    )
                });
                [
                    past.chain(running).collect(),
                    defaults.collect(),
                    vec![Row::Close],
                ]
            }
        };
        groups
            .into_iter()
            .filter(|group| !group.is_empty())
            .collect::<Vec<_>>()
            .join(&Row::Line)
    }

    /// The open view's rows and where each one is.
    fn slots(&self) -> Vec<(Row, Rect)> {
        let inset = menu::BORDER + menu::PAD;
        let mut top = inset;
        self.rows(self.mode)
            .into_iter()
            .map(|row| {
                let height = match row {
                    Row::Line => 2.0 * menu::LINE_MARGIN + menu::LINE_H,
                    Row::Close | Row::Item { .. } => menu::ROW_H,
                };
                let rect = [inset, top, self.menu_w - inset, top + height];
                top += height + menu::ROW_GAP;
                (row, rect)
            })
            .collect()
    }

    /// An item's name, port and text style.
    fn item(&self, server: Option<usize>, project: Option<usize>) -> (&str, String, Style) {
        let saved = project.map(|p| &self.projects.list()[p]);
        let (name, port) = match (server.map(|i| &self.servers[i]), saved) {
            (Some(running), _) => (running.name.as_str(), running.port),
            (None, Some(saved)) => (saved.launch.name(), saved.port),
            (None, None) => ("", 0),
        };
        let style = if self.is_default(project) {
            MENU_TEXT
        } else {
            MENU_ITALIC
        };
        (name, format!(":{port}"), style)
    }

    /// An item's content width in `mode`. Other rows fit in any panel.
    fn row_width(&self, mode: Mode, row: Row) -> f32 {
        let Row::Item { server, project } = row else {
            return 0.0;
        };
        let (name, port, style) = self.item(server, project);
        let mut width = self.renderer.measure(name, style)
            + menu::NAME_TO_PORT_MIN
            + self.renderer.measure(&port, style);
        if mode == Mode::Toggle {
            width += 2.0 * (font::ACTION_SIZE + menu::ICON_TO_TEXT);
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
        r.paint(dc, size, self.dpi, color::TASKBAR, || {
            if self.hover == Some(Part::Label) || self.menu_open() {
                let pressed = self.pressed == Some(Part::Label);
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
        r.present(self.menu, rect, self.dpi, color::MENU_BORDER, || {
            let inside = [
                menu::BORDER,
                menu::BORDER,
                self.menu_w - menu::BORDER,
                height - menu::BORDER,
            ];
            r.fill(inside, 0.0, color::MENU, 1.0);
            for (row, rect) in self.slots() {
                let (server, project) = match row {
                    Row::Item { server, project } => (server, project),
                    _ => (None, None),
                };
                let hovered = matches!(self.hover, Some(Part::Row(hovered, _)) if hovered == row);
                let lit = hovered
                    || (self.mode == Mode::Normal
                        && self.hover == Some(Part::Label)
                        && server == Some(self.current))
                    || (project.is_some() && project == self.drag);
                if lit {
                    let pressed = hovered && self.pressed == self.hover;
                    self.highlight(pressed, rect, menu::ROW_RADIUS);
                }
                let content = [
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
                    Row::Item { .. } => self.paint_item(server, project, content, accent),
                }
            }
        })
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
        let (name, port, style) = self.item(server, project);
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

/// Installed only while the toggle view is open: the panel never takes focus,
/// so nothing else tells it about a click outside.
unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        let down = matches!(
            w as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        );
        if code >= 0 && down {
            PostMessageW(PANEL.get(), BUTTON_DOWN, w, 0);
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
            WM_MOUSELEAVE => _ = with(|state| state.leave(false)),
            WM_LBUTTONDOWN => _ = with(|state| state.press(true)),
            WM_MBUTTONDOWN => _ = with(|state| state.press(false)),
            WM_LBUTTONUP => _ = with(State::release),
            WM_MBUTTONUP => _ = with(State::stop),
            WM_MOUSEWHEEL => _ = with(|state| state.scroll((w >> 16) as i16 as i32)),
            // The host's menu runs inside DefWindowProcW, and the pointer may
            // be elsewhere by the time it closes.
            WM_CONTEXTMENU => {
                with(|state| {
                    KillTimer(state.menu, SHOW);
                    state.hide_menu();
                });
                let result = DefWindowProcW(hwnd, message, w, l);
                with(State::rehover);
                return result;
            }
            WM_NCDESTROY => {
                with(|state| {
                    if state.view == hwnd {
                        state.close_toggle();
                        state.view = null_mut();
                        state.hover = None;
                        state.pressed = None;
                        KillTimer(state.menu, SHOW);
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
            WM_MOUSELEAVE => _ = with(|state| state.leave(true)),
            WM_LBUTTONDOWN => _ = with(|state| state.press(true)),
            WM_MBUTTONDOWN => _ = with(|state| state.press(false)),
            WM_LBUTTONUP => _ = with(State::release),
            WM_MBUTTONUP => _ = with(State::stop),
            WM_CAPTURECHANGED => _ = with(State::end_drag),
            BUTTON_DOWN => _ = with(|state| state.button_down(w as u32)),
            // Right-click anywhere opens the host's menu, which the strip forwards.
            WM_CONTEXTMENU => {
                if let Some(view) = with(|state| state.view).filter(|view| !view.is_null()) {
                    return SendMessageW(view, message, view as WPARAM, l);
                }
            }
            WM_TIMER if w == POLL => _ = with(State::refresh),
            WM_TIMER if w == SHOW => {
                KillTimer(hwnd, SHOW);
                with(|state| {
                    if state.hover == Some(Part::Label) {
                        state.show_menu();
                    }
                });
            }
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
