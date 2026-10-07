//! The strip in the taskbar and the server menu above it. Everything runs on
//! the UI thread. Window procedures reach the shared state through `with`.

use crate::{
    render::{Align, Rect, Renderer, Style},
    servers::{Scanner, Server},
    theme::{color, font, menu, strip},
    wide,
};
use std::{
    cell::RefCell,
    mem::zeroed,
    process::{Command, Stdio},
    ptr::{null, null_mut},
};
use win_taskbar_host::{Content, Surface, TaskbarHost};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
        },
        Gdi::{BeginPaint, EndPaint, HDC, InvalidateRect, PAINTSTRUCT, ScreenToClient},
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::WM_MOUSELEAVE,
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        WindowsAndMessaging::*,
    },
};

/// The menu's class. The installer finds the running copy by it, since the
/// menu, hidden or not, also runs the timers and takes the request to exit.
pub const CLASS_NAME: &str = "Portside";
const VIEW_CLASS: &str = "PortsideStrip";
/// Menu timers.
const POLL: usize = 1;
const SHOW: usize = 2;
const POLL_MS: u32 = 1000;

const STRIP_TEXT: Style = Style(font::STRIP_SIZE, false, 0);
const MENU_TEXT: Style = Style(font::MENU_SIZE, false, 0);
const ICON: Style = Style(font::ICON_SIZE, true, 0);
const COUNT_TEXT: Style = Style(
    font::STRIP_SIZE,
    false,
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

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Label,
    Row(usize),
}

/// Strip geometry for the current server, kept for painting and hit tests.
#[derive(Default)]
struct Layout {
    /// The label's hover rect; empty when no server shows.
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
    current: usize,
    /// The server the user last moved to, kept while it restarts.
    chosen: Option<Server>,
    layout: Layout,
    view: HWND,
    menu: HWND,
    menu_w: f32,
    dpi: u32,
    hover: Option<Part>,
    pressed: Option<Part>,
    wheel: i32,
    /// The strip's width with a server, and the width the host is built with.
    full_width: f32,
    /// Kept once created, so the strip can resize with the servers.
    host: Option<TaskbarHost>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
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
    let ex_style = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
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
    STATE.set(Some(State {
        renderer,
        scanner: Scanner::default(),
        servers: Vec::new(),
        current: 0,
        chosen: None,
        layout: Layout::default(),
        view: null_mut(),
        menu,
        menu_w: 0.0,
        dpi: 96,
        hover: None,
        pressed: None,
        wheel: 0,
        full_width,
        host: None,
    }));
    with(State::refresh);
    let result = TaskbarHost::builder(full_width as f64)
        .height_dip(strip::HEIGHT as f64)
        .save_placement_as("portside")
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
            state.hide_menu();
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
            let top = (strip::HEIGHT - strip::HIT_H) / 2.0;
            layout.label = [strip::LEAD, top, right, top + strip::HIT_H];
            layout.name_x = name_x;
            layout.name_max = name_max;
            layout.label_max_w = widest - strip::LEAD;
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

    /// Polls the listener table and repaints only when the servers changed.
    fn refresh(&mut self) {
        let servers = self.scanner.scan();
        if servers == self.servers {
            return;
        }
        let index =
            |target: Option<&Server>| target.and_then(|t| servers.iter().position(|s| s == t));
        let kept = index(self.chosen.as_ref()).or_else(|| index(self.servers.get(self.current)));
        self.current = kept.unwrap_or(self.current.min(servers.len().saturating_sub(1)));
        self.servers = servers;
        self.pressed = None;
        // Rows moved; the next pointer move finds the hovered one again.
        if matches!(self.hover, Some(Part::Row(_))) {
            self.hover = None;
        }
        self.relayout();
        self.fit();
        if self.menu_open() {
            self.show_menu();
        }
        // The strip may have changed under a still pointer.
        let (under, at) = self.pointer();
        if under == self.view {
            self.strip_move(at);
        } else if self.hover == Some(Part::Label) {
            self.set_hover(None);
        }
        self.redraw();
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
        // The menu rows shifted; the next pointer move finds the hovered one again.
        if matches!(self.hover, Some(Part::Row(_))) {
            self.hover = None;
        }
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
        self.invalidate(self.view);
    }

    fn menu_move(&mut self, at: (f32, f32)) {
        let row = self
            .others()
            .enumerate()
            .find(|&(slot, _)| contains(self.row(slot), at));
        if self.set_hover(row.map(|(_, i)| Part::Row(i))) {
            self.invalidate(self.menu);
        }
    }

    /// The window under the pointer, and the pointer in the strip's DIPs.
    fn pointer(&self) -> (HWND, (f32, f32)) {
        unsafe {
            let mut cursor = zeroed();
            GetCursorPos(&mut cursor);
            let under = WindowFromPoint(cursor);
            ScreenToClient(self.view, &mut cursor);
            let scale = self.scale();
            (under, (cursor.x as f32 / scale, cursor.y as f32 / scale))
        }
    }

    /// The pointer left the strip or the menu. The menu stays open only while
    /// the pointer is on it or on the label.
    fn leave(&mut self, from_menu: bool) {
        unsafe { KillTimer(self.menu, SHOW) };
        self.wheel = 0;
        if self
            .hover
            .is_some_and(|part| matches!(part, Part::Row(_)) == from_menu)
        {
            self.set_hover(None);
            self.invalidate(if from_menu { self.menu } else { self.view });
        }
        let (under, at) = self.pointer();
        if under != self.menu && !contains(self.layout.label, at) {
            self.hide_menu();
        }
    }

    fn press(&mut self) {
        self.pressed = self.hover;
        self.redraw();
    }

    fn release(&mut self) {
        let Some(part) = self.pressed.take() else {
            return;
        };
        match part {
            Part::Label => self.invalidate(self.view),
            Part::Row(row) => self.choose(row),
        }
        if let Some(server) = self.servers.get(self.current) {
            // localhost reaches only one of two servers sharing a port, so
            // those open at the loopback address of their own family.
            let shared = self
                .servers
                .iter()
                .filter(|s| s.port == server.port)
                .count()
                > 1;
            let host = match (shared, server.ipv6) {
                (false, _) => "localhost",
                (true, false) => "127.0.0.1",
                (true, true) => "[::1]",
            };
            // A short-lived Explorer opens the default browser, so the shell's
            // URL handlers never load into this long-running process. It gets
            // no standard handles, which a stop briefly swaps for a console's.
            let url = format!("http://{host}:{}", server.port);
            let _ = Command::new("explorer")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }

    /// Middle-click: stops the label's or the row's server. It leaves the
    /// strip on the first poll after it exits.
    fn stop(&mut self) {
        let Some(part) = self.pressed.take() else {
            return;
        };
        let i = match part {
            Part::Label => self.current,
            Part::Row(row) => row,
        };
        if let Some(server) = self.servers.get(i) {
            self.scanner.stop(server);
        }
        self.redraw();
    }

    fn show_menu(&mut self) {
        if self.servers.len() < 2 || self.view.is_null() {
            return self.hide_menu();
        }
        let widest = self.servers.iter().map(|server| {
            let port = format!(":{}", server.port);
            self.renderer.measure(&server.name, MENU_TEXT)
                + menu::NAME_TO_PORT_MIN
                + self.renderer.measure(&port, MENU_TEXT)
        });
        let inset = menu::BORDER + menu::PAD;
        // At least as wide as the label with any server, so the pointer can
        // move straight up into it and cycling never resizes it.
        let width = (widest.fold(0.0, f32::max) + 2.0 * (menu::ROW_PAD_X + inset))
            .max(menu::MIN_W)
            .max(self.layout.label_max_w);
        let rows = self.others().count() as f32;
        let height = 2.0 * inset + rows * menu::ROW_H + (rows - 1.0) * menu::ROW_GAP;
        let scale = self.scale();
        let w = (width * scale).ceil() as i32;
        let h = (height * scale).ceil() as i32;
        self.menu_w = w as f32 / scale;
        unsafe {
            let mut view: RECT = zeroed();
            GetWindowRect(self.view, &mut view);
            // Bottom edge on the top of the label's hover rect, left edges aligned.
            let left = view.left + (self.layout.label[0] * scale).round() as i32;
            let bottom = view.top + (self.layout.label[1] * scale).ceil() as i32;
            let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
            SetWindowPos(self.menu, HWND_TOPMOST, left, bottom - h, w, h, flags);
        }
        // The label keeps its highlight while the menu is open.
        self.redraw();
    }

    fn hide_menu(&mut self) {
        if self.menu_open() {
            unsafe { ShowWindow(self.menu, SW_HIDE) };
            if matches!(self.hover, Some(Part::Row(_))) {
                self.set_hover(None);
            }
            self.invalidate(self.view);
        }
    }

    /// The indexes of the servers the menu lists: all but the one in the strip.
    fn others(&self) -> impl Iterator<Item = usize> {
        (0..self.servers.len()).filter(|&i| i != self.current)
    }

    /// The rect of the menu's `slot`th row.
    fn row(&self, slot: usize) -> Rect {
        let inset = menu::BORDER + menu::PAD;
        let top = inset + slot as f32 * (menu::ROW_H + menu::ROW_GAP);
        [inset, top, self.menu_w - inset, top + menu::ROW_H]
    }

    fn highlight(&self, part: Part, rect: Rect, radius: f32) {
        let alpha = if self.pressed == Some(part) {
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
                self.highlight(Part::Label, l.label, strip::HIT_RADIUS);
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

    fn paint_menu(&self, dc: HDC, size: (i32, i32)) {
        let r = &self.renderer;
        let height = size.1 as f32 / self.scale();
        r.paint(dc, size, self.dpi, color::MENU_BORDER, || {
            let inside = [
                menu::BORDER,
                menu::BORDER,
                self.menu_w - menu::BORDER,
                height - menu::BORDER,
            ];
            r.fill(inside, 0.0, color::MENU, 1.0);
            for (slot, i) in self.others().enumerate() {
                let server = &self.servers[i];
                let row = self.row(slot);
                if self.hover == Some(Part::Row(i)) {
                    self.highlight(Part::Row(i), row, menu::ROW_RADIUS);
                }
                let text = [
                    row[0] + menu::ROW_PAD_X,
                    row[1],
                    row[2] - menu::ROW_PAD_X,
                    row[3],
                ];
                r.text(
                    &server.name,
                    MENU_TEXT,
                    text,
                    Align::Leading,
                    color::MENU_ROW,
                );
                let port = format!(":{}", server.port);
                r.text(&port, MENU_TEXT, text, Align::Trailing, color::TEXT_DIM);
            }
        });
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
            WM_LBUTTONDOWN | WM_MBUTTONDOWN => _ = with(State::press),
            WM_LBUTTONUP => _ = with(State::release),
            WM_MBUTTONUP => _ = with(State::stop),
            WM_MOUSEWHEEL => _ = with(|state| state.scroll((w >> 16) as i16 as i32)),
            WM_NCDESTROY => {
                with(|state| {
                    if state.view == hwnd {
                        state.hide_menu();
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
            WM_PAINT => paint(hwnd, State::paint_menu),
            WM_ERASEBKGND => return 1,
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
            WM_MOUSEMOVE => {
                track_leave(hwnd);
                with(|state| state.menu_move(state.point(l)));
            }
            WM_MOUSELEAVE => _ = with(|state| state.leave(true)),
            WM_LBUTTONDOWN | WM_MBUTTONDOWN => _ = with(State::press),
            WM_LBUTTONUP => _ = with(State::release),
            WM_MBUTTONUP => _ = with(State::stop),
            // Right-click anywhere opens the host's menu, which the strip forwards.
            WM_CONTEXTMENU => {
                let view = with(|state| {
                    state.hide_menu();
                    state.view
                });
                if let Some(view) = view.filter(|view| !view.is_null()) {
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
            WM_CLOSE => {
                crate::update::closing();
                EndMenu();
                PostQuitMessage(0);
            }
            _ => return DefWindowProcW(hwnd, message, w, l),
        }
        0
    }
}
