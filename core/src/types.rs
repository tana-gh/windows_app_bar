use windows::Win32::{
    Foundation::RECT,
    UI::{
        Shell::{ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP},
        WindowsAndMessaging::WM_APP,
    },
};

/// The callback message used by [`crate::AppBar::register`].
///
/// Use [`crate::AppBar::register_with_callback_message`] if this conflicts
/// with a message already used by the host application.
pub const APP_BAR_CALLBACK_MESSAGE: u32 = WM_APP + 0x3a0;

/// The desktop edge occupied by an AppBar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

impl Edge {
    pub(crate) const fn as_appbar_edge(self) -> u32 {
        match self {
            Self::Left => ABE_LEFT,
            Self::Top => ABE_TOP,
            Self::Right => ABE_RIGHT,
            Self::Bottom => ABE_BOTTOM,
        }
    }
}

/// The bounds of a display monitor in virtual-screen physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl From<RECT> for MonitorRect {
    fn from(rect: RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

/// A monitor in the order used by the AppBar registration APIs.
///
/// `index` is zero-based and is the order returned by `EnumDisplayMonitors`.
/// It is not the number shown by Windows Display Settings, and may change when
/// the display configuration changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorInfo {
    pub index: usize,
    pub bounds: MonitorRect,
}
