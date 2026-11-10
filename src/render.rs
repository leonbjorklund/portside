//! The drawing primitives in directwrite.cpp. Lengths are DIPs.
use crate::{
    theme::{Rgb, font},
    wide,
};
use std::{ffi::c_void, ptr::NonNull};
use windows_sys::Win32::Graphics::Gdi::HDC;

unsafe extern "C" {
    fn portside_renderer_create(
        regular: *const u8,
        regular_length: u32,
        italic: *const u8,
        italic_length: u32,
        family: *const u16,
        icon_family: *const u16,
        weight: u32,
        result: *mut i32,
    ) -> *mut c_void;
    fn portside_renderer_destroy(renderer: *mut c_void);
    fn portside_measure(
        renderer: *mut c_void,
        text: *const u16,
        length: u32,
        style: *const Style,
    ) -> f32;
    fn portside_begin(
        renderer: *mut c_void,
        dc: HDC,
        width: i32,
        height: i32,
        dpi: f32,
        background: Rgb,
    ) -> i32;
    fn portside_fill(renderer: *mut c_void, rect: *const Rect, radius: f32, color: Rgb, alpha: f32);
    fn portside_text(
        renderer: *mut c_void,
        text: *const u16,
        length: u32,
        style: *const Style,
        rect: *const Rect,
        align: i32,
        color: Rgb,
    );
    fn portside_arc(
        renderer: *mut c_void,
        cx: f32,
        cy: f32,
        radius: f32,
        start: f32,
        sweep: f32,
        stroke: f32,
        color: Rgb,
    );
    /// Whether the device was lost and the scene must be drawn again.
    fn portside_end(renderer: *mut c_void) -> bool;
}

/// Left, top, right, bottom, laid out as `D2D1_RECT_F`.
pub type Rect = [f32; 4];

#[repr(i32)]
#[derive(Clone, Copy)]
pub enum Face {
    Regular,
    Italic,
    Icon,
}

/// Size, face, and an OpenType feature tag or 0.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Style(pub f32, pub Face, pub u32);

/// DWRITE_TEXT_ALIGNMENT.
#[derive(Clone, Copy)]
pub enum Align {
    Leading = 0,
    Trailing = 1,
    Center = 2,
}

pub struct Renderer(NonNull<c_void>);

impl Renderer {
    pub fn new() -> Result<Self, String> {
        let mut result = 0;
        let renderer = unsafe {
            portside_renderer_create(
                font::REGULAR.as_ptr(),
                font::REGULAR.len() as u32,
                font::ITALIC.as_ptr(),
                font::ITALIC.len() as u32,
                wide(font::FAMILY).as_ptr(),
                wide(font::ICON_FAMILY).as_ptr(),
                font::WEIGHT,
                &mut result,
            )
        };
        NonNull::new(renderer)
            .map(Self)
            .ok_or_else(|| format!("Could not load the font: 0x{:08X}", result as u32))
    }

    pub fn measure(&self, text: &str, style: Style) -> f32 {
        let text: Vec<u16> = text.encode_utf16().collect();
        unsafe { portside_measure(self.0.as_ptr(), text.as_ptr(), text.len() as u32, &style) }
    }

    /// Clears to `background` and draws `scene`, once more after a lost device.
    pub fn paint(&self, dc: HDC, size: (i32, i32), dpi: u32, background: Rgb, scene: impl Fn()) {
        for _ in 0..2 {
            let begun = unsafe {
                portside_begin(self.0.as_ptr(), dc, size.0, size.1, dpi as f32, background)
            };
            if begun < 0 {
                return;
            }
            scene();
            if !unsafe { portside_end(self.0.as_ptr()) } {
                return;
            }
        }
    }

    /// Only inside `paint`.
    pub fn fill(&self, rect: Rect, radius: f32, color: Rgb, alpha: f32) {
        unsafe { portside_fill(self.0.as_ptr(), &rect, radius, color, alpha) }
    }

    /// Only inside `paint`. Centered vertically in `rect`, cut with "…" past its width.
    pub fn text(&self, text: &str, style: Style, rect: Rect, align: Align, color: Rgb) {
        let text: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            portside_text(
                self.0.as_ptr(),
                text.as_ptr(),
                text.len() as u32,
                &style,
                &rect,
                align as i32,
                color,
            )
        }
    }

    /// Only inside `paint`. An open arc with round ends. Angles are degrees
    /// clockwise from 12 o'clock.
    pub fn arc(
        &self,
        center: (f32, f32),
        radius: f32,
        start: f32,
        sweep: f32,
        stroke: f32,
        color: Rgb,
    ) {
        unsafe {
            portside_arc(
                self.0.as_ptr(),
                center.0,
                center.1,
                radius,
                start,
                sweep,
                stroke,
                color,
            )
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { portside_renderer_destroy(self.0.as_ptr()) }
    }
}
