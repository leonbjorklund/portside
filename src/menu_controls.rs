//! Native buttons give the painted server rows accessible names, state and focus.

use crate::wide;
use std::ptr::null;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::WC_BUTTONW,
        Input::KeyboardAndMouse::{
            EnableWindow, GetFocus, IsWindowEnabled, SetFocus, VK_ESCAPE, VK_RETURN, VK_SPACE,
            VK_TAB,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

struct Button<K, A> {
    key: K,
    action: A,
    hwnd: HWND,
    name: String,
}

pub struct Controls<K, A>(Vec<Button<K, A>>);

impl<K, A> Default for Controls<K, A> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: PartialEq, A: Copy> Controls<K, A> {
    /// Keep the same native button when a refresh moves its action to another row.
    pub fn sync(&mut self, parent: HWND, actions: Vec<(K, A, RECT, String, bool)>) {
        let mut previous = std::mem::take(self);
        let mut after = HWND_TOP;
        let mut next_id = 1;
        for (key, action, rect, name, enabled) in actions {
            let existing = previous.0.iter().position(|button| button.key == key);
            let (hwnd, mut caption) = match existing {
                Some(i) => {
                    let old = previous.0.swap_remove(i);
                    (old.hwnd, old.name)
                }
                None => {
                    let Some(hwnd) = create(parent, &mut next_id) else {
                        continue;
                    };
                    (hwnd, String::new())
                }
            };
            unsafe {
                // BUTTON captions use a doubled ampersand for a literal '&'.
                if caption != name
                    && SetWindowTextW(hwnd, wide(&name.replace('&', "&&")).as_ptr()) != 0
                {
                    caption = name;
                }
                EnableWindow(hwnd, enabled.into());
                SetWindowPos(
                    hwnd,
                    after,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_NOREDRAW | SWP_SHOWWINDOW,
                );
            }
            after = hwnd;
            self.0.push(Button {
                key,
                action,
                hwnd,
                name: caption,
            });
        }
        previous.clear();
    }

    pub fn clear(&mut self) {
        for button in self.0.drain(..) {
            unsafe { DestroyWindow(button.hwnd) };
        }
    }

    pub fn action(&self, hwnd: HWND) -> Option<A> {
        self.0.iter().find(|b| b.hwnd == hwnd).map(|b| b.action)
    }

    pub fn focused(&self) -> Option<A> {
        let hwnd = unsafe { GetFocus() };
        self.action(hwnd)
            .filter(|_| unsafe { IsWindowEnabled(hwnd) != 0 })
    }

    pub fn step(&self, backwards: bool) {
        let count = self.0.len() as isize;
        let (step, start) = if backwards { (-1, 0) } else { (1, -1) };
        let focus = unsafe { GetFocus() };
        let mut i = self
            .0
            .iter()
            .position(|button| button.hwnd == focus)
            .map_or(start, |i| i as isize);
        for _ in 0..count {
            i = (i + step).rem_euclid(count);
            let hwnd = self.0[i as usize].hwnd;
            if unsafe { IsWindowEnabled(hwnd) != 0 } {
                unsafe { SetFocus(hwnd) };
                break;
            }
        }
    }
}

fn create(parent: HWND, next_id: &mut i32) -> Option<HWND> {
    unsafe {
        while !GetDlgItem(parent, *next_id).is_null() {
            *next_id += 1;
        }
        let hwnd = CreateWindowExW(
            WS_EX_TRANSPARENT,
            WC_BUTTONW,
            null(),
            WS_CHILD | WS_TABSTOP | (BS_OWNERDRAW | BS_NOTIFY) as u32,
            0,
            0,
            0,
            0,
            parent,
            *next_id as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return None;
        }
        if SetWindowSubclass(hwnd, Some(button_proc), 0, 0) == 0 {
            DestroyWindow(hwnd);
            return None;
        }
        Some(hwnd)
    }
}

unsafe extern "system" fn button_proc(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        let menu_key = matches!(w as u16, VK_TAB | VK_RETURN | VK_SPACE | VK_ESCAPE);
        match message {
            // The parent paints the whole layered surface and owns each mouse gesture.
            WM_NCHITTEST => HTTRANSPARENT as LRESULT,
            WM_ERASEBKGND => 1,
            WM_KEYDOWN | WM_KEYUP if menu_key => SendMessageW(GetParent(hwnd), message, w, l),
            WM_CHAR if menu_key => 0,
            // UI Automation can press the button with BM_SETSTATE before invoking it.
            BM_CLICK => {
                SendMessageW(hwnd, BM_SETSTATE, 0, 0);
                if IsWindowVisible(hwnd) != 0 && IsWindowEnabled(hwnd) != 0 {
                    let id = GetDlgCtrlID(hwnd) as WPARAM;
                    SendMessageW(GetParent(hwnd), WM_COMMAND, id, hwnd as LPARAM);
                }
                0
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(button_proc), 0);
                DefSubclassProc(hwnd, message, w, l)
            }
            _ => DefSubclassProc(hwnd, message, w, l),
        }
    }
}
