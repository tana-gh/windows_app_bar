use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::{
        Shell::{APPBARDATA, SHAppBarMessage},
        WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos},
    },
};

use crate::{AppBarError, enumerate_monitors};

/// The subset of Win32 calls used by [`crate::AppBar`].
///
/// Keeping this boundary small lets the AppBar state machine be tested without
/// registering a real desktop toolbar.
pub(crate) trait AppBarApi: std::fmt::Debug {
    fn app_bar_message(&self, message: u32, data: &mut APPBARDATA) -> usize;
    fn set_window_pos(&self, hwnd: HWND, rect: RECT) -> Result<(), AppBarError>;
    fn monitor_rect(&self, index: usize) -> Result<RECT, AppBarError>;
}

#[derive(Debug)]
pub(crate) struct WindowsAppBarApi;

impl AppBarApi for WindowsAppBarApi {
    fn app_bar_message(&self, message: u32, data: &mut APPBARDATA) -> usize {
        unsafe { SHAppBarMessage(message, data) }
    }

    fn set_window_pos(&self, hwnd: HWND, rect: RECT) -> Result<(), AppBarError> {
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
            )?;
        }
        Ok(())
    }

    fn monitor_rect(&self, index: usize) -> Result<RECT, AppBarError> {
        enumerate_monitors().and_then(|monitors| {
            monitors
                .get(index)
                .map(|monitor| RECT {
                    left: monitor.bounds.left,
                    top: monitor.bounds.top,
                    right: monitor.bounds.right,
                    bottom: monitor.bounds.bottom,
                })
                .ok_or(AppBarError::MonitorNotFound { index })
        })
    }
}
