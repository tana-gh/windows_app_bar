use std::{ffi::c_void, marker::PhantomData, mem::size_of, rc::Rc};

use log::debug;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    UI::{
        Shell::{
            ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABM_WINDOWPOSCHANGED, ABN_POSCHANGED,
            APPBARDATA,
        },
        WindowsAndMessaging::{SW_HIDE, SW_SHOWNA, ShowWindow, WM_WINDOWPOSCHANGED},
    },
};

use crate::{
    APP_BAR_CALLBACK_MESSAGE, AppBarError, Edge,
    platform::{AppBarApi, WindowsAppBarApi},
};

/// A registered Windows Shell AppBar.
///
/// The AppBar does not own the window. The caller must ensure that its `HWND`
/// remains valid until this object is dropped or unregistered. It must also be
/// created and used on the window's owning thread.
#[derive(Debug)]
pub struct AppBar {
    pub(crate) hwnd: HWND,
    pub(crate) monitor_index: usize,
    pub(crate) edge: Edge,
    pub(crate) size: i32,
    pub(crate) callback_message: u32,
    pub(crate) registered: bool,
    pub(crate) api: Box<dyn AppBarApi>,
    // Win32 window operations belong to the window's owning thread.
    pub(crate) _thread_affinity: PhantomData<Rc<()>>,
}

impl AppBar {
    /// Registers `window` as an AppBar on `monitor_index` using
    /// [`APP_BAR_CALLBACK_MESSAGE`] for Shell notifications.
    ///
    /// `monitor_index` is zero-based in [`crate::enumerate_monitors`] order. This
    /// only registers the AppBar and reserves its desktop work area; it does
    /// not make the window visible. Call [`Self::show`] when appropriate.
    pub fn register(
        window: &impl HasWindowHandle,
        monitor_index: usize,
        edge: Edge,
        size: u32,
    ) -> Result<Self, AppBarError> {
        Self::register_with_callback_message(
            window,
            monitor_index,
            edge,
            size,
            APP_BAR_CALLBACK_MESSAGE,
        )
    }

    /// Registers `window` as an AppBar on `monitor_index`.
    ///
    /// `callback_message` must be forwarded from the owner window's message
    /// procedure to [`Self::handle_window_message`]. Pick a value which does
    /// not conflict with the host application's private window messages.
    pub fn register_with_callback_message(
        window: &impl HasWindowHandle,
        monitor_index: usize,
        edge: Edge,
        size: u32,
        callback_message: u32,
    ) -> Result<Self, AppBarError> {
        let size = i32::try_from(size).map_err(|_| AppBarError::InvalidSize)?;
        if size == 0 {
            return Err(AppBarError::InvalidSize);
        }

        let mut app_bar = Self {
            hwnd: hwnd_from_window(window)?,
            monitor_index,
            edge,
            size,
            callback_message,
            registered: false,
            api: Box::new(WindowsAppBarApi),
            _thread_affinity: PhantomData,
        };
        // Reject an invalid index before registering with the Shell, so no
        // rollback is needed for this caller error.
        app_bar.api.monitor_rect(monitor_index)?;
        app_bar.add()?;
        Ok(app_bar)
    }

    /// Returns the edge currently requested by this AppBar.
    pub const fn edge(&self) -> Edge {
        self.edge
    }
    /// Returns the zero-based monitor index selected for this AppBar.
    pub const fn monitor_index(&self) -> usize {
        self.monitor_index
    }
    /// Returns this AppBar's requested thickness in physical pixels.
    pub const fn size(&self) -> u32 {
        self.size as u32
    }
    /// Returns the Shell callback message registered for this AppBar.
    pub const fn callback_message(&self) -> u32 {
        self.callback_message
    }
    /// Returns whether this AppBar is currently registered with the Shell.
    ///
    /// This does not report the native window's visibility.
    pub const fn is_registered(&self) -> bool {
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

    /// Moves the AppBar to another monitor in [`crate::enumerate_monitors`] order.
    ///
    /// If repositioning fails, the previous monitor index is retained.
    pub fn set_monitor_index(&mut self, monitor_index: usize) -> Result<(), AppBarError> {
        // Validate even while hidden, so a later `show` cannot fail merely
        // because of a stale index supplied here.
        self.api.monitor_rect(monitor_index)?;
        if self.registered {
            self.apply_position_for(monitor_index, self.edge, self.size)?;
        }
        self.monitor_index = monitor_index;
        Ok(())
    }

    /// Registers this AppBar with the Shell if needed, then shows its native
    /// window without activating it.
    ///
    /// [`Self::is_registered`] reports the Shell registration state; it does
    /// not report whether the native window is visible.
    pub fn show(&mut self) -> Result<(), AppBarError> {
        if !self.registered {
            self.add()?;
        }
        // `ShowWindow` returns whether the window was previously visible, not
        // whether the operation succeeded.
        let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNA) };
        Ok(())
    }

    /// Hides its native window and releases its Shell AppBar registration.
    ///
    /// [`Self::is_registered`] reports the Shell registration state; it does
    /// not report whether the native window is visible.
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
        if self.api.app_bar_message(ABM_NEW, &mut data) == 0 {
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

    pub(crate) fn remove(&mut self) -> Result<(), AppBarError> {
        let mut data = self.data();
        if self.api.app_bar_message(ABM_REMOVE, &mut data) == 0 {
            return Err(AppBarError::ShellOperationFailed {
                operation: "removal",
            });
        }
        self.registered = false;
        debug!("removed AppBar");
        Ok(())
    }

    fn apply_position(&self, edge: Edge, size: i32) -> Result<(), AppBarError> {
        self.apply_position_for(self.monitor_index, edge, size)
    }

    fn apply_position_for(
        &self,
        monitor_index: usize,
        edge: Edge,
        size: i32,
    ) -> Result<(), AppBarError> {
        let mut data = self.data_for(edge);
        data.rc = Self::proposed_rect(self.api.monitor_rect(monitor_index)?, edge, size);
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

    pub(crate) fn proposed_rect(monitor: RECT, edge: Edge, size: i32) -> RECT {
        match edge {
            Edge::Left => RECT {
                left: monitor.left,
                top: monitor.top,
                right: monitor.left + size,
                bottom: monitor.bottom,
            },
            Edge::Top => RECT {
                left: monitor.left,
                top: monitor.top,
                right: monitor.right,
                bottom: monitor.top + size,
            },
            Edge::Right => RECT {
                left: monitor.right - size,
                top: monitor.top,
                right: monitor.right,
                bottom: monitor.bottom,
            },
            Edge::Bottom => RECT {
                left: monitor.left,
                top: monitor.bottom - size,
                right: monitor.right,
                bottom: monitor.bottom,
            },
        }
    }

    pub(crate) fn apply_thickness(rect: &mut RECT, edge: Edge, size: i32) {
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

pub(crate) fn hwnd_from_window(window: &impl HasWindowHandle) -> Result<HWND, AppBarError> {
    let handle = window.window_handle().map_err(AppBarError::WindowHandle)?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err(AppBarError::NotAWin32Window);
    };
    Ok(HWND(handle.hwnd.get() as *mut c_void))
}
