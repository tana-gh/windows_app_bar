//! A small Windows-only wrapper around the Shell AppBar API.
//!
//! The owner of the native window must forward its window messages to
//! [`AppBar::handle_window_message`]. In particular, this lets the AppBar
//! reclaim its position after another AppBar changes the available desktop
//! area.

#![cfg(windows)]

use std::{ffi::c_void, marker::PhantomData, mem::size_of, rc::Rc};

use log::debug;
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle};
use thiserror::Error;
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    UI::{
        Shell::{
            ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE,
            ABM_SETPOS, ABM_WINDOWPOSCHANGED, ABN_POSCHANGED, APPBARDATA, SHAppBarMessage,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE,
            SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos, ShowWindow, WM_APP, WM_WINDOWPOSCHANGED,
        },
    },
};

/// The callback message used by [`AppBar::register`].
///
/// Use [`AppBar::register_with_callback_message`] if this conflicts with a
/// message already used by the host application.
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
    const fn as_appbar_edge(self) -> u32 {
        match self {
            Self::Left => ABE_LEFT,
            Self::Top => ABE_TOP,
            Self::Right => ABE_RIGHT,
            Self::Bottom => ABE_BOTTOM,
        }
    }
}

/// Failures returned by [`AppBar`].
#[derive(Debug, Error)]
pub enum AppBarError {
    #[error("failed to obtain the native window handle: {0}")]
    WindowHandle(HandleError),

    #[error("the supplied window handle is not a Win32 HWND")]
    NotAWin32Window,

    #[error("the AppBar size must be greater than zero")]
    InvalidSize,

    #[error("Shell rejected AppBar {operation}")]
    ShellOperationFailed { operation: &'static str },

    #[error("Windows API error: {0}")]
    Windows(#[from] windows::core::Error),
}

/// The subset of Win32 calls used by [`AppBar`].
///
/// Keeping this boundary small lets the AppBar state machine be tested without
/// registering a real desktop toolbar.
trait AppBarApi: std::fmt::Debug {
    fn app_bar_message(&self, message: u32, data: &mut APPBARDATA) -> usize;
    fn set_window_pos(&self, hwnd: HWND, rect: RECT) -> Result<(), AppBarError>;
    fn screen_size(&self) -> (i32, i32);
}

#[derive(Debug)]
struct WindowsAppBarApi;

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

    fn screen_size(&self) -> (i32, i32) {
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }
}

/// A registered Windows Shell AppBar.
///
/// The AppBar does not own the window. The caller must ensure that its `HWND`
/// remains valid until this object is dropped or unregistered. It must also be
/// created and used on the window's owning thread.
#[derive(Debug)]
pub struct AppBar {
    hwnd: HWND,
    edge: Edge,
    size: i32,
    callback_message: u32,
    registered: bool,
    api: Box<dyn AppBarApi>,
    // Win32 window operations belong to the window's owning thread.
    _thread_affinity: PhantomData<Rc<()>>,
}

impl AppBar {
    /// Registers `window` as an AppBar using [`APP_BAR_CALLBACK_MESSAGE`] for
    /// Shell notifications.
    ///
    /// This only registers the AppBar and reserves its desktop work area; it
    /// does not make the window visible. Call [`Self::show`] when appropriate.
    pub fn register(
        window: &impl HasWindowHandle,
        edge: Edge,
        size: u32,
    ) -> Result<Self, AppBarError> {
        Self::register_with_callback_message(window, edge, size, APP_BAR_CALLBACK_MESSAGE)
    }

    /// Registers `window` as an AppBar.
    ///
    /// `callback_message` must be forwarded from the owner window's message
    /// procedure to [`Self::handle_window_message`]. Pick a value which does
    /// not conflict with the host application's private window messages.
    pub fn register_with_callback_message(
        window: &impl HasWindowHandle,
        edge: Edge,
        size: u32,
        callback_message: u32,
    ) -> Result<Self, AppBarError> {
        let size = i32::try_from(size).map_err(|_| AppBarError::InvalidSize)?;
        if size == 0 {
            return Err(AppBarError::InvalidSize);
        }

        let handle = window.window_handle().map_err(AppBarError::WindowHandle)?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err(AppBarError::NotAWin32Window);
        };

        let mut app_bar = Self {
            hwnd: HWND(handle.hwnd.get() as *mut c_void),
            edge,
            size,
            callback_message,
            registered: false,
            api: Box::new(WindowsAppBarApi),
            _thread_affinity: PhantomData,
        };
        app_bar.add()?;
        Ok(app_bar)
    }

    /// Returns the edge currently requested by this AppBar.
    pub const fn edge(&self) -> Edge {
        self.edge
    }

    /// Returns this AppBar's requested thickness in physical pixels.
    pub const fn size(&self) -> u32 {
        self.size as u32
    }

    /// Returns the Shell callback message registered for this AppBar.
    pub const fn callback_message(&self) -> u32 {
        self.callback_message
    }

    /// Returns whether this AppBar currently reserves desktop work area.
    pub const fn is_visible(&self) -> bool {
        self.registered
    }

    /// Changes the requested thickness and immediately repositions a visible AppBar.
    ///
    /// If repositioning fails, the previous thickness is retained.
    pub fn set_size(&mut self, size: u32) -> Result<(), AppBarError> {
        let size = i32::try_from(size).map_err(|_| AppBarError::InvalidSize)?;
        if size == 0 {
            return Err(AppBarError::InvalidSize);
        }

        if self.registered {
            self.apply_position(self.edge, size)?;
        }
        self.size = size;
        Ok(())
    }

    /// Changes the desktop edge and immediately repositions a visible AppBar.
    ///
    /// If repositioning fails, the previous edge is retained.
    pub fn set_edge(&mut self, edge: Edge) -> Result<(), AppBarError> {
        if self.registered {
            self.apply_position(edge, self.size)?;
        }
        self.edge = edge;
        Ok(())
    }

    /// Shows the AppBar and reserves its desktop work area.
    pub fn show(&mut self) -> Result<(), AppBarError> {
        if !self.registered {
            self.add()?;
        }
        // `ShowWindow` returns whether the window was previously visible, not
        // whether the operation succeeded.
        let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNA) };
        Ok(())
    }

    /// Hides the AppBar and releases its desktop work area.
    pub fn hide(&mut self) -> Result<(), AppBarError> {
        if self.registered {
            self.remove()?;
        }
        // `ShowWindow` returns whether the window was previously visible, not
        // whether the operation succeeded.
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        Ok(())
    }

    /// Explicitly removes the AppBar from the Shell.
    ///
    /// Dropping an `AppBar` also attempts this cleanup, but errors during
    /// `Drop` cannot be reported.
    pub fn unregister(mut self) -> Result<(), AppBarError> {
        if self.registered {
            self.remove()?;
        }
        Ok(())
    }

    /// Handles a native window message for the AppBar's owner window.
    ///
    /// Returns `true` when the message was consumed. Forward the exact values
    /// received by the WndProc. `WM_WINDOWPOSCHANGED` is also observed so the
    /// Shell can update its work-area bookkeeping.
    pub fn handle_window_message(
        &mut self,
        message: u32,
        wparam: usize,
        _lparam: isize,
    ) -> Result<bool, AppBarError> {
        if message == self.callback_message {
            if wparam as u32 == ABN_POSCHANGED && self.registered {
                self.apply_position(self.edge, self.size)?;
            }
            return Ok(true);
        }

        if message == WM_WINDOWPOSCHANGED && self.registered {
            self.notify_window_position_changed();
        }

        Ok(false)
    }

    fn add(&mut self) -> Result<(), AppBarError> {
        let mut data = self.data();
        let result = self.api.app_bar_message(ABM_NEW, &mut data);
        if result == 0 {
            return Err(AppBarError::ShellOperationFailed {
                operation: "registration",
            });
        }

        self.registered = true;
        if let Err(error) = self.apply_position(self.edge, self.size) {
            let _ = self.remove();
            return Err(error);
        }
        debug!("registered AppBar for {:?}", self.edge);
        Ok(())
    }

    fn remove(&mut self) -> Result<(), AppBarError> {
        let mut data = self.data();
        let result = self.api.app_bar_message(ABM_REMOVE, &mut data);
        if result == 0 {
            return Err(AppBarError::ShellOperationFailed {
                operation: "removal",
            });
        }
        self.registered = false;
        debug!("removed AppBar");
        Ok(())
    }

    fn apply_position(&self, edge: Edge, size: i32) -> Result<(), AppBarError> {
        let mut data = self.data_for(edge);
        data.rc = self.proposed_rect(edge, size);
        self.api.app_bar_message(ABM_QUERYPOS, &mut data);
        Self::apply_thickness(&mut data.rc, edge, size);
        self.api.app_bar_message(ABM_SETPOS, &mut data);
        self.api.set_window_pos(self.hwnd, data.rc)
    }

    fn notify_window_position_changed(&self) {
        let mut data = self.data();
        self.api.app_bar_message(ABM_WINDOWPOSCHANGED, &mut data);
    }

    fn data(&self) -> APPBARDATA {
        self.data_for(self.edge)
    }

    fn data_for(&self, edge: Edge) -> APPBARDATA {
        APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uCallbackMessage: self.callback_message,
            uEdge: edge.as_appbar_edge(),
            rc: RECT::default(),
            lParam: LPARAM(0),
        }
    }

    fn proposed_rect(&self, edge: Edge, size: i32) -> RECT {
        let (width, height) = self.api.screen_size();
        match edge {
            Edge::Left => RECT {
                left: 0,
                top: 0,
                right: size,
                bottom: height,
            },
            Edge::Top => RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: size,
            },
            Edge::Right => RECT {
                left: width - size,
                top: 0,
                right: width,
                bottom: height,
            },
            Edge::Bottom => RECT {
                left: 0,
                top: height - size,
                right: width,
                bottom: height,
            },
        }
    }

    fn apply_thickness(rect: &mut RECT, edge: Edge, size: i32) {
        match edge {
            Edge::Left => rect.right = rect.left + size,
            Edge::Top => rect.bottom = rect.top + size,
            Edge::Right => rect.left = rect.right - size,
            Edge::Bottom => rect.top = rect.bottom - size,
        }
    }
}

impl Drop for AppBar {
    fn drop(&mut self) {
        if self.registered
            && let Err(error) = self.remove()
        {
            debug!("failed to remove AppBar during drop: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    #[derive(Debug)]
    struct MockAppBarApi {
        messages: RefCell<Vec<(u32, RECT)>>,
        query_response: RECT,
        fail_set_window_pos: bool,
    }

    impl MockAppBarApi {
        fn new(query_response: RECT, fail_set_window_pos: bool) -> Self {
            Self {
                messages: RefCell::new(Vec::new()),
                query_response,
                fail_set_window_pos,
            }
        }
    }

    impl AppBarApi for Rc<MockAppBarApi> {
        fn app_bar_message(&self, message: u32, data: &mut APPBARDATA) -> usize {
            self.messages.borrow_mut().push((message, data.rc));
            if message == ABM_QUERYPOS {
                data.rc = self.query_response;
            }
            1
        }

        fn set_window_pos(&self, _hwnd: HWND, _rect: RECT) -> Result<(), AppBarError> {
            if self.fail_set_window_pos {
                Err(AppBarError::ShellOperationFailed {
                    operation: "positioning",
                })
            } else {
                Ok(())
            }
        }

        fn screen_size(&self) -> (i32, i32) {
            (1920, 1080)
        }
    }

    fn app_bar_for_test(api: Rc<MockAppBarApi>) -> AppBar {
        AppBar {
            hwnd: HWND::default(),
            edge: Edge::Bottom,
            size: 30,
            callback_message: APP_BAR_CALLBACK_MESSAGE,
            registered: true,
            api: Box::new(api),
            _thread_affinity: PhantomData,
        }
    }

    #[test]
    fn thickness_is_applied_from_each_requested_edge() {
        let original = RECT {
            left: 10,
            top: 20,
            right: 110,
            bottom: 220,
        };

        let mut left = original;
        AppBar::apply_thickness(&mut left, Edge::Left, 30);
        assert_eq!(left.right, 40);

        let mut top = original;
        AppBar::apply_thickness(&mut top, Edge::Top, 30);
        assert_eq!(top.bottom, 50);

        let mut right = original;
        AppBar::apply_thickness(&mut right, Edge::Right, 30);
        assert_eq!(right.left, 80);

        let mut bottom = original;
        AppBar::apply_thickness(&mut bottom, Edge::Bottom, 30);
        assert_eq!(bottom.top, 190);
    }

    #[test]
    fn position_changed_notification_uses_wparam_and_proposes_a_screen_edge_rect() {
        let api = Rc::new(MockAppBarApi::new(
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            false,
        ));
        let mut app_bar = app_bar_for_test(api.clone());

        assert!(
            app_bar
                .handle_window_message(APP_BAR_CALLBACK_MESSAGE, ABN_POSCHANGED as usize, 0)
                .unwrap()
        );

        let messages = api.messages.borrow();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, ABM_QUERYPOS);
        assert_eq!(
            messages[0].1,
            RECT {
                left: 0,
                top: 1050,
                right: 1920,
                bottom: 1080,
            }
        );
        assert_eq!(messages[1].0, ABM_SETPOS);

        drop(messages);
        app_bar
            .handle_window_message(APP_BAR_CALLBACK_MESSAGE, 0, ABN_POSCHANGED as isize)
            .unwrap();
        assert_eq!(api.messages.borrow().len(), 2);
    }

    #[test]
    fn failed_reposition_keeps_the_previous_size_and_edge() {
        let api = Rc::new(MockAppBarApi::new(RECT::default(), true));
        let mut app_bar = app_bar_for_test(api);

        assert!(app_bar.set_size(40).is_err());
        assert_eq!(app_bar.size(), 30);

        assert!(app_bar.set_edge(Edge::Left).is_err());
        assert_eq!(app_bar.edge(), Edge::Bottom);
    }
}
