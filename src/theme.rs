//! Portside's look. Lengths are DIPs (1/96 inch), colors are 0xRRGGBB as
//! `D2D1::ColorF` takes them. Values match agent-usage-overlay so the two
//! strips read as one.

pub type Rgb = u32;

pub mod font {
    pub const FAMILY: &str = "Atkinson Hyperlegible";
    pub const REGULAR: &[u8] =
        include_bytes!("../assets/fonts/atkinsonhyperlegible/AtkinsonHyperlegible-Regular.ttf");
    /// The only weight used anywhere (DWRITE_FONT_WEIGHT_NORMAL).
    pub const WEIGHT: u32 = 400;

    /// The count, name and port, at agent-usage-overlay's text size.
    pub const STRIP_SIZE: f32 = 13.0;
    pub const MENU_SIZE: f32 = 12.0;
    /// The count uses the `tnum` feature: Atkinson's default digits are
    /// proportional, so "1/3" and "2/3" would otherwise differ in width.
    pub const COUNT_FEATURE: [u8; 4] = *b"tnum";

    /// Ships with Windows 11; never embedded.
    pub const ICON_FAMILY: &str = "Segoe Fluent Icons";
    /// The globe fills its one-em box, which is centered in the strip.
    pub const ICON_SIZE: f32 = 12.0;
    pub const GLOBE: &str = "\u{E774}"; // Globe
}

pub mod color {
    use super::Rgb;

    pub const TASKBAR: Rgb = 0x1c1c1c;
    pub const TEXT: Rgb = 0xf1f3f4;
    /// Count and port.
    pub const TEXT_DIM: Rgb = 0x9c9c9c;
    /// Hover and pressed backgrounds are this white drawn over the background.
    pub const HOVER: Rgb = 0xffffff;
    /// White drawn over the background at this opacity.
    pub const HOVER_ALPHA: f32 = 0.08;
    pub const PRESSED_ALPHA: f32 = 0.05;

    pub const MENU: Rgb = 0x2b2b2b;
    pub const MENU_BORDER: Rgb = 0x3a3a3a;
    pub const MENU_ROW: Rgb = 0xb4b4b4;

    /// The globe while no servers are running.
    pub const GLOBE_EMPTY: Rgb = 0x6b6b6b;
}

/// The strip in the taskbar: the label, one hover rect holding the globe,
/// count, name and port, left to right.
pub mod strip {
    pub const HEIGHT: f32 = 26.0;
    /// Before the label's hover rect.
    pub const LEAD: f32 = 6.0;

    /// The label's hover rect, vertically centered in the strip.
    pub const HIT_H: f32 = 22.0;
    pub const HIT_RADIUS: f32 = 4.0;
    pub const LABEL_PAD_X: f32 = 6.0;

    pub const GLOBE_TO_TEXT: f32 = 7.0;
    pub const COUNT_TO_NAME: f32 = 10.0;
    /// Reserved for the name and port, so cycling never moves anything. The
    /// hover rect hugs the text, and the name is cut with "…" where it would
    /// pass this width. The port is never cut.
    pub const NAME_SLOT_W: f32 = 158.0;
    pub const NAME_TO_PORT: f32 = 5.0;
}

/// Server list shown while the label is hovered. Its bottom edge touches the
/// top of the label's hover rect, so the pointer can move into it directly.
pub mod menu {
    pub const SHOW_DELAY_MS: u32 = 350;
    pub const MIN_W: f32 = 170.0;
    pub const PAD: f32 = 3.0;
    pub const BORDER: f32 = 1.0;
    /// DWMWCP_ROUND, which also gives the system shadow.
    #[expect(dead_code, reason = "DWM rounds the corners; this records its radius")]
    pub const RADIUS: f32 = 8.0;
    pub const ROW_H: f32 = 22.0;
    pub const ROW_PAD_X: f32 = 6.0;
    pub const ROW_GAP: f32 = 1.0;
    pub const ROW_RADIUS: f32 = 4.0;
    pub const NAME_TO_PORT_MIN: f32 = 16.0;
}
