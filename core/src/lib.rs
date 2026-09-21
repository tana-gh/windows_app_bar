//! A small Windows-only wrapper around the Shell AppBar API.
//!
//! # Choosing an API
//!
//! [`AppBar`] is the manual integration API. Use it when the application owns
//! its WndProc: forward every received message to
//! [`AppBar::handle_window_message`]. This lets the AppBar reclaim its
//! position after another AppBar changes the available desktop area.
//!
//! [`SubclassedAppBar`] is the automatic integration API. It installs a
//! Common Controls window subclass with `SetWindowSubclass`, forwards AppBar
//! messages itself, and passes unrelated messages on through the existing
//! subclass chain. It is suited to frameworks such as Bevy and winit, where
//! an existing HWND is available but the application does not own its WndProc.
//! Keep the returned value alive for as long as that window is an AppBar.
//!
//! # Selecting a monitor
//!
//! Registration APIs take a zero-based `monitor_index`. Obtain valid indices
//! with [`enumerate_monitors`]. The index is the current
//! `EnumDisplayMonitors` enumeration order; it is not the number displayed in
//! Windows Display Settings and can change when the display configuration
//! changes.
//!
//! # Thread affinity
//!
//! An AppBar must be created, used, and dropped on its HWND's owning thread.
//!
//! # `SubclassedAppBar` lifecycle
//!
//! `SubclassedAppBar` does not own its HWND. Call
//! [`SubclassedAppBar::unregister`] on the owning thread before destroying the
//! native window; this is the only cleanup path that reports errors. If it is
//! missed, both [`Drop`] and `WM_DESTROY` attempt best-effort cleanup, but
//! cannot report failure. `WM_CLOSE` is never consumed by this crate and is
//! forwarded to the next window procedure, so the owner retains its normal
//! close and cancellation policy.

#![cfg(windows)]

use std::{
    cell::RefCell, collections::HashMap, ffi::c_void, marker::PhantomData, mem::size_of, rc::Rc,
};

use log::debug;
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle};
use thiserror::Error;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO},
    UI::{
        Shell::{
            ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE,
            ABM_SETPOS, ABM_WINDOWPOSCHANGED, ABN_POSCHANGED, APPBARDATA, DefSubclassProc,
            RemoveWindowSubclass, SHAppBarMessage, SetWindowSubclass,
        },
        WindowsAndMessaging::{
            SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos,
            ShowWindow, WM_APP, WM_DESTROY, WM_WINDOWPOSCHANGED,
        },
    },
};

/// The callback message used by [`AppBar::register`].
///
/// Use [`AppBar::register_with_callback_message`] if this conflicts with a
/// message already used by the host application.
pub const APP_BAR_CALLBACK_MESSAGE: u32 = WM_APP + 0x3a0;

const APP_BAR_SUBCLASS_ID: usize = 0x77_41_42_00;

/// Win32 operations performed by the window-subclass lifecycle.
///
/// Keeping these calls behind a narrow boundary lets the state transitions be
/// tested without a real HWND. The callback itself always uses
/// `WindowsWindowSubclassApi`.
trait WindowSubclassApi: std::fmt::Debug {
    fn install(&self, hwnd: HWND) -> windows::core::Result<()>;
    fn remove(&self, hwnd: HWND) -> windows::core::Result<()>;
    fn call_next(&self, hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
}

#[derive(Debug)]
struct WindowsWindowSubclassApi;

impl WindowSubclassApi for WindowsWindowSubclassApi {
    fn install(&self, hwnd: HWND) -> windows::core::Result<()> {
        unsafe {
            SetWindowSubclass(
                hwnd,
                Some(subclassed_app_bar_window_proc),
                APP_BAR_SUBCLASS_ID,
                0,
            )
            .ok()
        }
    }

    fn remove(&self, hwnd: HWND) -> windows::core::Result<()> {
        unsafe {
            RemoveWindowSubclass(
                hwnd,
                Some(subclassed_app_bar_window_proc),
                APP_BAR_SUBCLASS_ID,
            )
            .ok()
        }
    }

    fn call_next(&self, hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
}

/// The desktop edge occupied by an AppBar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
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

/// Enumerates monitors in the order accepted by [`AppBar::register`] and
/// [`SubclassedAppBar::register`].
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, AppBarError> {
    WindowsAppBarApi::enumerate_monitors()
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

    #[error("the supplied window already has a SubclassedAppBar registered")]
    SubclassAlreadyRegistered,

    #[error("display monitor at index {index} was not found")]
    MonitorNotFound { index: usize },

    #[error("Shell rejected AppBar {operation}")]
    ShellOperationFailed { operation: &'static str },

    #[error("Windows API error: {0}")]
    Windows(#[from] windows::core::Error),

    #[error(
        "AppBar registration failed: {registration_error}; additionally failed to remove the AppBar window subclass: {subclass_error}"
    )]
    SubclassRegistrationCleanup {
        registration_error: Box<AppBarError>,
        subclass_error: windows::core::Error,
    },
}

/// The subset of Win32 calls used by [`AppBar`].
///
/// Keeping this boundary small lets the AppBar state machine be tested without
/// registering a real desktop toolbar.
trait AppBarApi: std::fmt::Debug {
    fn app_bar_message(&self, message: u32, data: &mut APPBARDATA) -> usize;
    fn set_window_pos(&self, hwnd: HWND, rect: RECT) -> Result<(), AppBarError>;
    fn monitor_rect(&self, index: usize) -> Result<RECT, AppBarError>;
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

    fn monitor_rect(&self, index: usize) -> Result<RECT, AppBarError> {
        Self::enumerate_monitors().and_then(|monitors| {
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

impl WindowsAppBarApi {
    fn enumerate_monitors() -> Result<Vec<MonitorInfo>, AppBarError> {
        let mut handles = Vec::<HMONITOR>::new();
        unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor_handle),
                LPARAM((&mut handles as *mut Vec<HMONITOR>) as isize),
            )
            .ok()?;
        }

        handles
            .into_iter()
            .enumerate()
            .map(|(index, handle)| {
                let mut info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                unsafe { GetMonitorInfoW(handle, &mut info).ok()? };
                Ok(MonitorInfo {
                    index,
                    bounds: info.rcMonitor.into(),
                })
            })
            .collect()
    }
}

unsafe extern "system" fn collect_monitor_handle(
    monitor: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    // `data` points to `handles`, which remains alive for the synchronous
    // EnumDisplayMonitors call above.
    unsafe { (&mut *(data.0 as *mut Vec<HMONITOR>)).push(monitor) };
    true.into()
}

/// A registered Windows Shell AppBar.
///
/// The AppBar does not own the window. The caller must ensure that its `HWND`
/// remains valid until this object is dropped or unregistered. It must also be
/// created and used on the window's owning thread.
#[derive(Debug)]
pub struct AppBar {
    hwnd: HWND,
    monitor_index: usize,
    edge: Edge,
    size: i32,
    callback_message: u32,
    registered: bool,
    api: Box<dyn AppBarApi>,
    // Win32 window operations belong to the window's owning thread.
    _thread_affinity: PhantomData<Rc<()>>,
}

thread_local! {
    static SUBCLASSED_APP_BARS: RefCell<SubclassRegistry> = RefCell::default();
}

/// Owns the state slots for the AppBar subclasses installed on this thread.
///
/// A slot is reserved before installing the native subclass so that a second
/// Rust value cannot update the same `SetWindowSubclass` procedure/ID pair.
#[derive(Debug, Default)]
struct SubclassRegistry {
    states: HashMap<isize, Rc<RefCell<SubclassState>>>,
}

impl SubclassRegistry {
    fn reserve(
        &mut self,
        hwnd: HWND,
        state: Rc<RefCell<SubclassState>>,
    ) -> Result<(), AppBarError> {
        if self
            .states
            .get(&(hwnd.0 as isize))
            .is_some_and(|existing| existing.borrow().is_attached())
        {
            return Err(AppBarError::SubclassAlreadyRegistered);
        }

        self.states.insert(hwnd.0 as isize, state);
        Ok(())
    }

    fn state(&self, hwnd: HWND) -> Option<Rc<RefCell<SubclassState>>> {
        self.states.get(&(hwnd.0 as isize)).cloned()
    }

    fn release_if_owned(&mut self, hwnd: HWND, state: &Rc<RefCell<SubclassState>>) {
        if self
            .states
            .get(&(hwnd.0 as isize))
            .is_some_and(|existing| Rc::ptr_eq(existing, state))
        {
            self.states.remove(&(hwnd.0 as isize));
        }
    }
}

#[derive(Debug)]
struct SubclassState {
    app_bar: Option<AppBar>,
    callback_message: u32,
    attached: bool,
    operation_in_progress: bool,
    pending_reposition: bool,
    pending_window_position_changed: bool,
}

impl SubclassState {
    fn registering(callback_message: u32) -> Self {
        Self {
            app_bar: None,
            callback_message,
            attached: true,
            operation_in_progress: true,
            pending_reposition: false,
            pending_window_position_changed: false,
        }
    }

    fn is_attached(&self) -> bool {
        self.attached
    }

    fn dispatch_message(&mut self, message: u32, wparam: usize) -> SubclassMessageDispatch {
        if let Some(consumed) = self.defer_reentrant_message(message, wparam) {
            return if consumed {
                SubclassMessageDispatch::Consume
            } else {
                SubclassMessageDispatch::Forward
            };
        }

        let Some(app_bar) = self.app_bar.take() else {
            return SubclassMessageDispatch::Forward;
        };
        self.operation_in_progress = true;
        SubclassMessageDispatch::HandleWithAppBar(app_bar)
    }

    /// Records a message received while an AppBar operation is in progress.
    fn defer_reentrant_message(&mut self, message: u32, wparam: usize) -> Option<bool> {
        if !self.operation_in_progress {
            return None;
        }

        if message == self.callback_message {
            if wparam as u32 == ABN_POSCHANGED {
                self.pending_reposition = true;
            }
            Some(true)
        } else {
            if message == WM_WINDOWPOSCHANGED {
                self.pending_window_position_changed = true;
            }
            Some(false)
        }
    }

    fn begin_operation(&mut self) -> Option<AppBar> {
        if !self.attached || self.operation_in_progress {
            return None;
        }
        let app_bar = self.app_bar.take()?;
        self.operation_in_progress = true;
        Some(app_bar)
    }

    fn take_pending_notifications(&mut self) -> (bool, bool, u32) {
        (
            std::mem::take(&mut self.pending_reposition),
            std::mem::take(&mut self.pending_window_position_changed),
            self.callback_message,
        )
    }

    fn finish_operation(&mut self, app_bar: AppBar) -> Option<AppBar> {
        self.operation_in_progress = false;
        if self.attached {
            self.app_bar = Some(app_bar);
            None
        } else {
            Some(app_bar)
        }
    }

    fn begin_detach(&mut self) -> Option<AppBar> {
        self.attached = false;
        self.operation_in_progress = true;
        self.app_bar.take()
    }

    fn abort_registration(&mut self) {
        self.operation_in_progress = false;
        self.pending_reposition = false;
        self.pending_window_position_changed = false;
    }
}

/// The action the WndProc must take after inspecting one message.
#[derive(Debug)]
enum SubclassMessageDispatch {
    Consume,
    Forward,
    HandleWithAppBar(AppBar),
}

/// An [`AppBar`] which automatically forwards its owner window's messages.
///
/// This type subclasses the native window procedure, forwarding AppBar Shell
/// notifications to its contained [`AppBar`] and all other messages to the
/// original procedure.
///
/// Call [`Self::unregister`] before destroying the owner `HWND`. If that is
/// missed, `Drop` and a `WM_DESTROY` notification perform best-effort cleanup
/// without being able to report errors. `WM_CLOSE` is forwarded to the next
/// window procedure unchanged.
///
/// This type does not own the window. Create, use, unregister, and drop it on
/// the window's owning thread.
#[derive(Debug)]
pub struct SubclassedAppBar {
    hwnd: HWND,
    state: Rc<RefCell<SubclassState>>,
    _thread_affinity: PhantomData<Rc<()>>,
}

impl AppBar {
    /// Registers `window` as an AppBar on `monitor_index` using
    /// [`APP_BAR_CALLBACK_MESSAGE`] for Shell notifications.
    ///
    /// `monitor_index` is zero-based in [`enumerate_monitors`] order. This
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

    /// Moves the AppBar to another monitor in [`enumerate_monitors`] order.
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

    fn proposed_rect(monitor: RECT, edge: Edge, size: i32) -> RECT {
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

    fn apply_thickness(rect: &mut RECT, edge: Edge, size: i32) {
        match edge {
            Edge::Left => rect.right = rect.left + size,
            Edge::Top => rect.bottom = rect.top + size,
            Edge::Right => rect.left = rect.right - size,
            Edge::Bottom => rect.top = rect.bottom - size,
        }
    }
}

impl SubclassedAppBar {
    /// Registers `window` as an AppBar on `monitor_index` and subclasses its
    /// window procedure so AppBar messages are forwarded automatically.
    ///
    /// The returned value must be kept alive until before the native window is
    /// destroyed. It is bound to the window's owning thread.
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

    /// Like [`Self::register`], but uses `callback_message` for Shell AppBar
    /// notifications.
    pub fn register_with_callback_message(
        window: &impl HasWindowHandle,
        monitor_index: usize,
        edge: Edge,
        size: u32,
        callback_message: u32,
    ) -> Result<Self, AppBarError> {
        let hwnd = hwnd_from_window(window)?;
        // Shell calls made during registration may synchronously enter the
        // WndProc. Queue their relevant notifications until `app_bar` is
        // available below.
        let state = Rc::new(RefCell::new(SubclassState::registering(callback_message)));
        reserve_subclass_state(hwnd, state.clone())?;
        if let Err(error) = install_window_subclass_with_api(hwnd, &WindowsWindowSubclassApi) {
            release_subclass_state(hwnd, &state);
            return Err(error);
        }

        let app_bar = complete_subclass_registration(
            hwnd,
            &state,
            AppBar::register_with_callback_message(
                window,
                monitor_index,
                edge,
                size,
                callback_message,
            ),
            &WindowsWindowSubclassApi,
        )?;
        finish_app_bar_operation(&state, app_bar);

        Ok(Self {
            hwnd,
            state,
            _thread_affinity: PhantomData,
        })
    }

    /// Returns the edge currently requested by this AppBar.
    pub fn edge(&self) -> Edge {
        self.with_registered_app_bar(AppBar::edge)
    }

    /// Returns the zero-based monitor index selected for this AppBar.
    pub fn monitor_index(&self) -> usize {
        self.with_registered_app_bar(AppBar::monitor_index)
    }

    /// Returns this AppBar's requested thickness in physical pixels.
    pub fn size(&self) -> u32 {
        self.with_registered_app_bar(AppBar::size)
    }

    /// Returns the Shell callback message registered for this AppBar.
    pub fn callback_message(&self) -> u32 {
        self.state.borrow().callback_message
    }

    /// Returns whether this AppBar is currently registered with the Shell.
    ///
    /// This does not report the native window's visibility.
    pub fn is_registered(&self) -> bool {
        self.with_registered_app_bar(AppBar::is_registered)
    }

    /// Changes the requested thickness and immediately repositions the AppBar.
    pub fn set_size(&mut self, size: u32) -> Result<(), AppBarError> {
        self.with_app_bar(|app_bar| app_bar.set_size(size))
    }

    /// Changes the desktop edge and immediately repositions the AppBar.
    pub fn set_edge(&mut self, edge: Edge) -> Result<(), AppBarError> {
        self.with_app_bar(|app_bar| app_bar.set_edge(edge))
    }

    /// Moves the AppBar to another monitor in [`enumerate_monitors`] order.
    pub fn set_monitor_index(&mut self, monitor_index: usize) -> Result<(), AppBarError> {
        self.with_app_bar(|app_bar| app_bar.set_monitor_index(monitor_index))
    }

    /// Registers this AppBar with the Shell if needed, then shows its native
    /// window without activating it.
    pub fn show(&mut self) -> Result<(), AppBarError> {
        self.with_app_bar(AppBar::show)
    }

    /// Hides its native window and releases its Shell AppBar registration.
    pub fn hide(&mut self) -> Result<(), AppBarError> {
        self.with_app_bar(AppBar::hide)
    }

    /// Removes the AppBar and removes this library's window subclass.
    ///
    /// Call this on the owner thread before destroying the native window.
    ///
    /// This is the only cleanup path that reports errors. If it is skipped,
    /// `Drop` and `WM_DESTROY` perform best-effort cleanup instead.
    pub fn unregister(self) -> Result<(), AppBarError> {
        detach_subclass_state_with_api(self.hwnd, &WindowsWindowSubclassApi)
    }

    /// Runs `operation` with the registered AppBar.
    ///
    /// Any AppBar notifications synchronously re-entered while `operation`
    /// calls Win32 are deferred and processed before this method returns.
    pub fn with_app_bar<R>(&mut self, operation: impl FnOnce(&mut AppBar) -> R) -> R {
        let app_bar = begin_app_bar_operation(&self.state)
            .expect("an AppBar operation cannot be nested on the same instance");
        let mut app_bar = app_bar;
        let result = operation(&mut app_bar);
        finish_app_bar_operation(&self.state, app_bar);
        result
    }

    /// Runs a read-only operation with the AppBar stored in this subclass state.
    ///
    /// The AppBar is temporarily removed only while an internal mutable
    /// operation is running. Public read accessors cannot overlap that
    /// operation, so its presence is an invariant here.
    fn with_registered_app_bar<R>(&self, operation: impl FnOnce(&AppBar) -> R) -> R {
        let state = self.state.borrow();
        let app_bar = state
            .app_bar
            .as_ref()
            .expect("AppBar is available outside an operation");
        operation(app_bar)
    }
}

impl Drop for SubclassedAppBar {
    fn drop(&mut self) {
        if let Err(error) = detach_subclass_state(self.hwnd) {
            debug!("failed to detach AppBar during drop: {error}");
        }
    }
}

fn hwnd_from_window(window: &impl HasWindowHandle) -> Result<HWND, AppBarError> {
    let handle = window.window_handle().map_err(AppBarError::WindowHandle)?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err(AppBarError::NotAWin32Window);
    };
    Ok(HWND(handle.hwnd.get() as *mut c_void))
}

fn install_window_subclass_with_api(
    hwnd: HWND,
    api: &impl WindowSubclassApi,
) -> Result<(), AppBarError> {
    api.install(hwnd)?;
    Ok(())
}

/// Reserves the per-HWND state slot before installing the native subclass.
///
/// `SetWindowSubclass` treats a repeated procedure/ID pair as an update, so
/// allowing a second owner here would make the two Rust values interfere with
/// each other's cleanup.
fn reserve_subclass_state(
    hwnd: HWND,
    state: Rc<RefCell<SubclassState>>,
) -> Result<(), AppBarError> {
    SUBCLASSED_APP_BARS.with(|app_bars| app_bars.borrow_mut().reserve(hwnd, state))
}

/// Removes a reservation only when it still belongs to `state`.
fn release_subclass_state(hwnd: HWND, state: &Rc<RefCell<SubclassState>>) {
    SUBCLASSED_APP_BARS.with(|app_bars| {
        app_bars.borrow_mut().release_if_owned(hwnd, state);
    });
}

/// Completes AppBar registration after the window subclass has been installed.
///
/// Keeping this rollback path independent from Win32 lets failure handling be
/// tested with a mock subclass API.
fn complete_subclass_registration(
    hwnd: HWND,
    state: &Rc<RefCell<SubclassState>>,
    app_bar: Result<AppBar, AppBarError>,
    api: &impl WindowSubclassApi,
) -> Result<AppBar, AppBarError> {
    match app_bar {
        Ok(app_bar) => Ok(app_bar),
        Err(error) => match detach_subclass_state_with_api(hwnd, api) {
            Ok(()) => Err(error),
            Err(AppBarError::Windows(subclass_error)) => {
                let mut state = state.borrow_mut();
                state.abort_registration();
                Err(AppBarError::SubclassRegistrationCleanup {
                    registration_error: Box::new(error),
                    subclass_error,
                })
            }
            Err(rollback_error) => Err(rollback_error),
        },
    }
}

unsafe extern "system" fn subclassed_app_bar_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _reference_data: usize,
) -> LRESULT {
    let api = WindowsWindowSubclassApi;
    if message == WM_DESTROY {
        if let Err(error) = detach_subclass_state_with_api(hwnd, &api) {
            debug!("failed to detach AppBar during window destruction: {error}");
        }
        return api.call_next(hwnd, message, wparam, lparam);
    }

    let Some(state) = registered_subclass_state(hwnd) else {
        return LRESULT(0);
    };
    let mut app_bar = match dispatch_subclass_message(&state, message, wparam.0) {
        SubclassMessageDispatch::Consume => return LRESULT(0),
        SubclassMessageDispatch::Forward => return api.call_next(hwnd, message, wparam, lparam),
        SubclassMessageDispatch::HandleWithAppBar(app_bar) => app_bar,
    };
    let consumed = match app_bar.handle_window_message(message, wparam.0, lparam.0) {
        Ok(consumed) => consumed,
        Err(error) => {
            debug!("AppBar message handling failed: {error}");
            false
        }
    };

    finish_app_bar_operation(&state, app_bar);

    if consumed {
        LRESULT(0)
    } else {
        api.call_next(hwnd, message, wparam, lparam)
    }
}

/// Chooses the WndProc action and performs only in-memory state transitions.
fn dispatch_subclass_message(
    state: &Rc<RefCell<SubclassState>>,
    message: u32,
    wparam: usize,
) -> SubclassMessageDispatch {
    state.borrow_mut().dispatch_message(message, wparam)
}

fn detach_subclass_state(hwnd: HWND) -> Result<(), AppBarError> {
    detach_subclass_state_with_api(hwnd, &WindowsWindowSubclassApi)
}

fn detach_subclass_state_with_api(
    hwnd: HWND,
    api: &impl WindowSubclassApi,
) -> Result<(), AppBarError> {
    let state = registered_subclass_state(hwnd);
    let Some(state) = state else {
        return Ok(());
    };
    {
        if !state.borrow().is_attached() {
            return Ok(());
        }
    }

    // This removes only the callback identified by our procedure and ID,
    // leaving any earlier or later subclasses in the chain untouched.
    api.remove(hwnd)?;

    release_subclass_state(hwnd, &state);

    let app_bar = state.borrow_mut().begin_detach();

    if let Some(mut app_bar) = app_bar {
        // The owner may outlive this state (for example after WM_CLOSE), so
        // mark it unregistered before its HWND can be destroyed.
        let result = app_bar.remove();
        return result;
    }

    Ok(())
}

fn registered_subclass_state(hwnd: HWND) -> Option<Rc<RefCell<SubclassState>>> {
    SUBCLASSED_APP_BARS.with(|app_bars| app_bars.borrow().state(hwnd))
}

fn begin_app_bar_operation(state: &Rc<RefCell<SubclassState>>) -> Option<AppBar> {
    state.borrow_mut().begin_operation()
}

fn finish_app_bar_operation(state: &Rc<RefCell<SubclassState>>, mut app_bar: AppBar) {
    loop {
        let (reposition, position_changed, callback_message) =
            state.borrow_mut().take_pending_notifications();

        if !reposition && !position_changed {
            break;
        }

        if reposition
            && let Err(error) =
                app_bar.handle_window_message(callback_message, ABN_POSCHANGED as usize, 0)
        {
            debug!("deferred AppBar reposition failed: {error}");
        }
        if position_changed
            && let Err(error) = app_bar.handle_window_message(WM_WINDOWPOSCHANGED, 0, 0)
        {
            debug!("deferred AppBar position notification failed: {error}");
        }
    }

    let app_bar_to_drop = state.borrow_mut().finish_operation(app_bar);
    drop(app_bar_to_drop);
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
    use std::{
        cell::RefCell,
        num::NonZeroIsize,
        rc::Rc,
        sync::{Mutex, Once},
    };

    use raw_window_handle::{WebWindowHandle, Win32WindowHandle, WindowHandle};
    use windows::{
        Win32::{
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
                HMENU, RegisterClassW, SendMessageW, SetWindowLongPtrW, WINDOW_EX_STYLE, WM_CLOSE,
                WNDCLASSW, WS_POPUP,
            },
        },
        core::w,
    };

    const INTEGRATION_TEST_CLASS: windows::core::PCWSTR = w!("windows_app_bar_integration_test");
    static INTERACTIVE_DESKTOP: Mutex<()> = Mutex::new(());

    struct TestWindow(HWND);

    impl HasWindowHandle for TestWindow {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let hwnd = NonZeroIsize::new(self.0.0 as isize).ok_or(HandleError::Unavailable)?;
            let handle = Win32WindowHandle::new(hwnd);
            // The test keeps the native window alive for the lifetime of this
            // borrowing wrapper.
            unsafe { Ok(WindowHandle::borrow_raw(RawWindowHandle::Win32(handle))) }
        }
    }

    struct NonWin32TestWindow;

    impl HasWindowHandle for NonWin32TestWindow {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            // This synthetic handle is used only to exercise the platform
            // validation path; it is never passed to a native API.
            unsafe {
                Ok(WindowHandle::borrow_raw(RawWindowHandle::Web(
                    WebWindowHandle::new(0),
                )))
            }
        }
    }

    unsafe extern "system" fn integration_test_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_CLOSE {
            let close_count = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut usize;
            if !close_count.is_null() {
                unsafe { *close_count += 1 };
            }
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn create_integration_test_window() -> TestWindow {
        static CLASS: Once = Once::new();
        CLASS.call_once(|| unsafe {
            let instance = GetModuleHandleW(None).expect("get test module handle");
            let class = WNDCLASSW {
                hInstance: instance.into(),
                lpszClassName: INTEGRATION_TEST_CLASS,
                lpfnWndProc: Some(integration_test_window_proc),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0, "register test window class");
        });

        unsafe {
            let instance = GetModuleHandleW(None).expect("get test module handle");
            TestWindow(
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    INTEGRATION_TEST_CLASS,
                    w!("windows_app_bar integration test"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    Some(HMENU::default()),
                    Some(instance.into()),
                    None,
                )
                .expect("create test window"),
            )
        }
    }

    fn try_register_integration_app_bar(window: &TestWindow) -> Option<SubclassedAppBar> {
        match SubclassedAppBar::register(window, 0, Edge::Bottom, 1) {
            Ok(app_bar) => Some(app_bar),
            Err(AppBarError::ShellOperationFailed {
                operation: "registration",
            }) => {
                eprintln!("skipping: the current desktop does not provide a Shell AppBar host");
                None
            }
            Err(error) => panic!("register AppBar on the primary enumerated monitor: {error}"),
        }
    }

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

        fn monitor_rect(&self, index: usize) -> Result<RECT, AppBarError> {
            if index == 0 {
                Ok(RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                })
            } else {
                Err(AppBarError::MonitorNotFound { index })
            }
        }
    }

    #[derive(Debug)]
    struct MockWindowSubclassApi {
        calls: RefCell<Vec<&'static str>>,
        fail_install: bool,
        fail_remove: bool,
    }

    impl MockWindowSubclassApi {
        fn new(fail_install: bool, fail_remove: bool) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                fail_install,
                fail_remove,
            }
        }

        fn failure() -> windows::core::Error {
            windows::core::Error::new(
                windows::core::HRESULT(0x8000_4005u32 as i32),
                "mock window subclass failure",
            )
        }
    }

    impl WindowSubclassApi for MockWindowSubclassApi {
        fn install(&self, _hwnd: HWND) -> windows::core::Result<()> {
            self.calls.borrow_mut().push("install");
            if self.fail_install {
                Err(Self::failure())
            } else {
                Ok(())
            }
        }

        fn remove(&self, _hwnd: HWND) -> windows::core::Result<()> {
            self.calls.borrow_mut().push("remove");
            if self.fail_remove {
                Err(Self::failure())
            } else {
                Ok(())
            }
        }

        fn call_next(
            &self,
            _hwnd: HWND,
            _message: u32,
            _wparam: WPARAM,
            _lparam: LPARAM,
        ) -> LRESULT {
            self.calls.borrow_mut().push("call_next");
            LRESULT(123)
        }
    }

    fn app_bar_for_test(api: Rc<MockAppBarApi>) -> AppBar {
        AppBar {
            hwnd: HWND::default(),
            monitor_index: 0,
            edge: Edge::Bottom,
            size: 30,
            callback_message: APP_BAR_CALLBACK_MESSAGE,
            registered: true,
            api: Box::new(api),
            _thread_affinity: PhantomData,
        }
    }

    fn subclass_state_for_test(app_bar: Option<AppBar>) -> Rc<RefCell<SubclassState>> {
        Rc::new(RefCell::new(SubclassState {
            app_bar,
            callback_message: APP_BAR_CALLBACK_MESSAGE,
            attached: true,
            operation_in_progress: false,
            pending_reposition: false,
            pending_window_position_changed: false,
        }))
    }

    fn subclassed_app_bar_for_test(app_bar: AppBar) -> SubclassedAppBar {
        SubclassedAppBar {
            hwnd: HWND::default(),
            state: subclass_state_for_test(Some(app_bar)),
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
    fn position_changed_notification_uses_wparam_and_proposes_a_monitor_edge_rect() {
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
    fn proposed_rect_uses_the_selected_monitor_bounds() {
        let monitor = RECT {
            left: -2560,
            top: 100,
            right: 0,
            bottom: 1540,
        };

        assert_eq!(
            AppBar::proposed_rect(monitor, Edge::Bottom, 48),
            RECT {
                left: -2560,
                top: 1492,
                right: 0,
                bottom: 1540,
            }
        );
        assert_eq!(
            AppBar::proposed_rect(monitor, Edge::Left, 48),
            RECT {
                left: -2560,
                top: 100,
                right: -2512,
                bottom: 1540,
            }
        );
    }

    #[test]
    fn failed_reposition_keeps_the_previous_size_and_edge() {
        let api = Rc::new(MockAppBarApi::new(RECT::default(), true));
        let mut app_bar = app_bar_for_test(api);

        assert!(app_bar.set_size(40).is_err());
        assert_eq!(app_bar.size(), 30);

        assert!(app_bar.set_edge(Edge::Left).is_err());
        assert_eq!(app_bar.edge(), Edge::Bottom);

        assert!(matches!(
            app_bar.set_monitor_index(1),
            Err(AppBarError::MonitorNotFound { index: 1 })
        ));
        assert_eq!(app_bar.monitor_index(), 0);
    }

    #[test]
    fn hwnd_from_window_accepts_win32_handles_and_rejects_other_platforms() {
        let window = TestWindow(HWND(std::ptr::dangling_mut::<c_void>()));
        assert_eq!(hwnd_from_window(&window).unwrap(), window.0);

        assert!(matches!(
            hwnd_from_window(&NonWin32TestWindow),
            Err(AppBarError::NotAWin32Window)
        ));
    }

    #[test]
    fn subclassed_app_bar_read_accessors_use_the_registered_app_bar() {
        let api = Rc::new(MockAppBarApi::new(RECT::default(), false));
        let app_bar = subclassed_app_bar_for_test(app_bar_for_test(api));

        assert_eq!(app_bar.edge(), Edge::Bottom);
        assert_eq!(app_bar.monitor_index(), 0);
        assert_eq!(app_bar.size(), 30);
        assert_eq!(app_bar.callback_message(), APP_BAR_CALLBACK_MESSAGE);
        assert!(app_bar.is_registered());
    }

    #[test]
    fn explicit_unregister_removes_a_registered_app_bar_once() {
        let api = Rc::new(MockAppBarApi::new(RECT::default(), false));
        app_bar_for_test(api.clone()).unregister().unwrap();

        let messages = api.messages.borrow();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, ABM_REMOVE);
    }

    #[test]
    fn subclass_installation_is_testable_without_a_real_window() {
        let hwnd = HWND::default();
        let success = MockWindowSubclassApi::new(false, false);
        install_window_subclass_with_api(hwnd, &success).unwrap();
        assert_eq!(*success.calls.borrow(), ["install"]);

        let failure = MockWindowSubclassApi::new(true, false);
        assert!(matches!(
            install_window_subclass_with_api(hwnd, &failure),
            Err(AppBarError::Windows(_))
        ));
        assert_eq!(*failure.calls.borrow(), ["install"]);
    }

    #[test]
    fn duplicate_subclass_registration_keeps_the_original_state() {
        let hwnd = HWND::default();
        let original = subclass_state_for_test(None);
        reserve_subclass_state(hwnd, original.clone()).unwrap();

        let duplicate = subclass_state_for_test(None);
        assert!(matches!(
            reserve_subclass_state(hwnd, duplicate.clone()),
            Err(AppBarError::SubclassAlreadyRegistered)
        ));

        let registered =
            registered_subclass_state(hwnd).expect("original state remains registered");
        assert!(Rc::ptr_eq(&registered, &original));
        assert!(!Rc::ptr_eq(&registered, &duplicate));
        release_subclass_state(hwnd, &original);
    }

    #[test]
    fn failed_app_bar_registration_removes_the_installed_subclass() {
        let hwnd = HWND::default();
        let state = subclass_state_for_test(None);
        state.borrow_mut().operation_in_progress = true;
        reserve_subclass_state(hwnd, state.clone()).unwrap();

        let api = MockWindowSubclassApi::new(false, false);
        let error = complete_subclass_registration(
            hwnd,
            &state,
            Err(AppBarError::ShellOperationFailed {
                operation: "registration",
            }),
            &api,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            AppBarError::ShellOperationFailed {
                operation: "registration"
            }
        ));
        assert_eq!(*api.calls.borrow(), ["remove"]);
        assert!(registered_subclass_state(hwnd).is_none());
        assert!(!state.borrow().attached);
    }

    #[test]
    fn failed_registration_reports_both_errors_when_subclass_rollback_fails() {
        let hwnd = HWND::default();
        let state = subclass_state_for_test(None);
        state.borrow_mut().operation_in_progress = true;
        reserve_subclass_state(hwnd, state.clone()).unwrap();

        let api = MockWindowSubclassApi::new(false, true);
        let error = complete_subclass_registration(
            hwnd,
            &state,
            Err(AppBarError::ShellOperationFailed {
                operation: "registration",
            }),
            &api,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            AppBarError::SubclassRegistrationCleanup { .. }
        ));
        assert_eq!(*api.calls.borrow(), ["remove"]);
        assert!(registered_subclass_state(hwnd).is_some());
        assert!(state.borrow().attached);
        assert!(!state.borrow().operation_in_progress);

        release_subclass_state(hwnd, &state);
    }

    #[test]
    fn dispatch_defers_reentrant_messages_and_forwards_other_messages() {
        let api = Rc::new(MockAppBarApi::new(RECT::default(), false));
        let state = subclass_state_for_test(Some(app_bar_for_test(api)));
        state.borrow_mut().operation_in_progress = true;

        assert!(matches!(
            dispatch_subclass_message(&state, APP_BAR_CALLBACK_MESSAGE, ABN_POSCHANGED as usize),
            SubclassMessageDispatch::Consume
        ));
        assert!(matches!(
            dispatch_subclass_message(&state, WM_WINDOWPOSCHANGED, 0),
            SubclassMessageDispatch::Forward
        ));

        let state = state.borrow();
        assert!(state.pending_reposition);
        assert!(state.pending_window_position_changed);
        assert!(state.app_bar.is_some());
    }

    #[test]
    fn detach_keeps_state_when_subclass_removal_fails() {
        let hwnd = HWND::default();
        let state = subclass_state_for_test(None);
        reserve_subclass_state(hwnd, state.clone()).unwrap();

        let api = MockWindowSubclassApi::new(false, true);
        assert!(matches!(
            detach_subclass_state_with_api(hwnd, &api),
            Err(AppBarError::Windows(_))
        ));
        assert_eq!(*api.calls.borrow(), ["remove"]);
        assert!(state.borrow().attached);
        assert!(registered_subclass_state(hwnd).is_some());

        release_subclass_state(hwnd, &state);
    }

    #[test]
    fn detach_removes_subclass_before_releasing_app_bar_state() {
        let hwnd = HWND::default();
        let app_bar_api = Rc::new(MockAppBarApi::new(RECT::default(), false));
        let state = subclass_state_for_test(Some(app_bar_for_test(app_bar_api.clone())));
        reserve_subclass_state(hwnd, state.clone()).unwrap();

        let api = MockWindowSubclassApi::new(false, false);
        detach_subclass_state_with_api(hwnd, &api).unwrap();

        assert_eq!(*api.calls.borrow(), ["remove"]);
        assert!(!state.borrow().attached);
        assert!(state.borrow().app_bar.is_none());
        assert!(registered_subclass_state(hwnd).is_none());
        assert!(
            app_bar_api
                .messages
                .borrow()
                .iter()
                .any(|(message, _)| *message == ABM_REMOVE)
        );
    }

    #[test]
    fn deferred_subclass_notifications_are_drained_after_an_operation() {
        let api = Rc::new(MockAppBarApi::new(
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            false,
        ));
        let state = Rc::new(RefCell::new(SubclassState {
            app_bar: Some(app_bar_for_test(api.clone())),
            callback_message: APP_BAR_CALLBACK_MESSAGE,
            attached: true,
            operation_in_progress: false,
            pending_reposition: false,
            pending_window_position_changed: false,
        }));

        let app_bar = begin_app_bar_operation(&state).unwrap();
        {
            let mut state = state.borrow_mut();
            state.pending_reposition = true;
            state.pending_window_position_changed = true;
        }
        finish_app_bar_operation(&state, app_bar);

        let state = state.borrow();
        assert!(state.app_bar.is_some());
        assert!(!state.operation_in_progress);
        assert!(!state.pending_reposition);
        assert!(!state.pending_window_position_changed);
        drop(state);

        let messages = api.messages.borrow();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].0, ABM_QUERYPOS);
        assert_eq!(messages[1].0, ABM_SETPOS);
        assert_eq!(messages[2].0, ABM_WINDOWPOSCHANGED);
    }

    #[test]
    fn registration_time_callbacks_are_deferred_and_replayed() {
        let api = Rc::new(MockAppBarApi::new(
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            false,
        ));
        let state = Rc::new(RefCell::new(SubclassState {
            // This is the state between installing the window subclass and
            // completing AppBar::register_with_callback_message.
            app_bar: None,
            callback_message: APP_BAR_CALLBACK_MESSAGE,
            attached: true,
            operation_in_progress: true,
            pending_reposition: false,
            pending_window_position_changed: false,
        }));

        {
            let mut state = state.borrow_mut();
            assert_eq!(
                state.defer_reentrant_message(APP_BAR_CALLBACK_MESSAGE, ABN_POSCHANGED as usize),
                Some(true)
            );
            assert_eq!(
                state.defer_reentrant_message(WM_WINDOWPOSCHANGED, 0),
                Some(false)
            );
        }

        finish_app_bar_operation(&state, app_bar_for_test(api.clone()));

        let state = state.borrow();
        assert!(state.app_bar.is_some());
        assert!(!state.operation_in_progress);
        drop(state);

        let messages = api.messages.borrow();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].0, ABM_QUERYPOS);
        assert_eq!(messages[1].0, ABM_SETPOS);
        assert_eq!(messages[2].0, ABM_WINDOWPOSCHANGED);
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop; run with --ignored"]
    fn real_window_registers_and_unregisters_a_subclassed_app_bar() {
        let _desktop = INTERACTIVE_DESKTOP
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let window = create_integration_test_window();

        let Some(app_bar) = try_register_integration_app_bar(&window) else {
            unsafe { DestroyWindow(window.0).expect("destroy test window") };
            return;
        };
        app_bar.unregister().expect("unregister AppBar");

        unsafe { DestroyWindow(window.0).expect("destroy test window") };
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop; run with --ignored"]
    fn real_window_forwards_wm_close_to_the_original_window_procedure() {
        let _desktop = INTERACTIVE_DESKTOP
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let window = create_integration_test_window();
        let mut close_count = 0_usize;
        unsafe {
            SetWindowLongPtrW(window.0, GWLP_USERDATA, (&raw mut close_count) as isize);
        }

        let Some(app_bar) = try_register_integration_app_bar(&window) else {
            unsafe { DestroyWindow(window.0).expect("destroy test window") };
            return;
        };
        unsafe {
            SendMessageW(window.0, WM_CLOSE, None, None);
        }
        assert_eq!(close_count, 1);

        app_bar.unregister().expect("unregister AppBar");
        unsafe { DestroyWindow(window.0).expect("destroy test window") };
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop; run with --ignored"]
    fn real_window_destroy_releases_subclassed_app_bar_state() {
        let _desktop = INTERACTIVE_DESKTOP
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let window = create_integration_test_window();
        let Some(app_bar) = try_register_integration_app_bar(&window) else {
            unsafe { DestroyWindow(window.0).expect("destroy test window") };
            return;
        };
        let state = app_bar.state.clone();

        unsafe { DestroyWindow(window.0).expect("destroy test window") };

        let state = state.borrow();
        assert!(!state.attached);
        assert!(state.app_bar.is_none());
        drop(state);
        drop(app_bar);
    }
}
