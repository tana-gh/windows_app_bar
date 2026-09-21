use std::{cell::RefCell, collections::HashMap, marker::PhantomData, rc::Rc};

use log::debug;
use raw_window_handle::HasWindowHandle;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{ABN_POSCHANGED, DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{WM_DESTROY, WM_WINDOWPOSCHANGED},
    },
};

use crate::{APP_BAR_CALLBACK_MESSAGE, AppBar, AppBarError, Edge, app_bar::hwnd_from_window};

const APP_BAR_SUBCLASS_ID: usize = 0x77_41_42_00;

/// Win32 operations performed by the window-subclass lifecycle.
///
/// Keeping these calls behind a narrow boundary lets the state transitions be
/// tested without a real HWND. The callback itself always uses
/// `WindowsWindowSubclassApi`.
pub(crate) trait WindowSubclassApi: std::fmt::Debug {
    fn install(&self, hwnd: HWND) -> windows::core::Result<()>;
    fn remove(&self, hwnd: HWND) -> windows::core::Result<()>;
    fn call_next(&self, hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
}

#[derive(Debug)]
pub(crate) struct WindowsWindowSubclassApi;

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

thread_local! {
    static SUBCLASSED_APP_BARS: RefCell<SubclassRegistry> = RefCell::default();
}

/// Owns the state slots for the AppBar subclasses installed on this thread.
///
/// A slot is reserved before installing the native subclass so that a second
/// Rust value cannot update the same `SetWindowSubclass` procedure/ID pair.
#[derive(Debug, Default)]
pub(crate) struct SubclassRegistry {
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
pub(crate) struct SubclassState {
    pub(crate) app_bar: Option<AppBar>,
    pub(crate) callback_message: u32,
    pub(crate) attached: bool,
    pub(crate) operation_in_progress: bool,
    pub(crate) pending_reposition: bool,
    pub(crate) pending_window_position_changed: bool,
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
pub(crate) enum SubclassMessageDispatch {
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
    pub(crate) hwnd: HWND,
    pub(crate) state: Rc<RefCell<SubclassState>>,
    pub(crate) _thread_affinity: PhantomData<Rc<()>>,
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
    /// Moves the AppBar to another monitor in [`crate::enumerate_monitors`] order.
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

pub(crate) fn install_window_subclass_with_api(
    hwnd: HWND,
    api: &impl WindowSubclassApi,
) -> Result<(), AppBarError> {
    api.install(hwnd)?;
    Ok(())
}

/// Reserves the per-HWND state slot before installing the native subclass.
pub(crate) fn reserve_subclass_state(
    hwnd: HWND,
    state: Rc<RefCell<SubclassState>>,
) -> Result<(), AppBarError> {
    SUBCLASSED_APP_BARS.with(|app_bars| app_bars.borrow_mut().reserve(hwnd, state))
}

/// Removes a reservation only when it still belongs to `state`.
pub(crate) fn release_subclass_state(hwnd: HWND, state: &Rc<RefCell<SubclassState>>) {
    SUBCLASSED_APP_BARS.with(|app_bars| app_bars.borrow_mut().release_if_owned(hwnd, state));
}

/// Completes AppBar registration after the window subclass has been installed.
pub(crate) fn complete_subclass_registration(
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
                state.borrow_mut().abort_registration();
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
pub(crate) fn dispatch_subclass_message(
    state: &Rc<RefCell<SubclassState>>,
    message: u32,
    wparam: usize,
) -> SubclassMessageDispatch {
    state.borrow_mut().dispatch_message(message, wparam)
}

fn detach_subclass_state(hwnd: HWND) -> Result<(), AppBarError> {
    detach_subclass_state_with_api(hwnd, &WindowsWindowSubclassApi)
}

pub(crate) fn detach_subclass_state_with_api(
    hwnd: HWND,
    api: &impl WindowSubclassApi,
) -> Result<(), AppBarError> {
    let Some(state) = registered_subclass_state(hwnd) else {
        return Ok(());
    };
    if !state.borrow().is_attached() {
        return Ok(());
    }
    // This removes only the callback identified by our procedure and ID.
    api.remove(hwnd)?;
    release_subclass_state(hwnd, &state);
    if let Some(mut app_bar) = state.borrow_mut().begin_detach() {
        return app_bar.remove();
    }
    Ok(())
}

pub(crate) fn registered_subclass_state(hwnd: HWND) -> Option<Rc<RefCell<SubclassState>>> {
    SUBCLASSED_APP_BARS.with(|app_bars| app_bars.borrow().state(hwnd))
}

pub(crate) fn begin_app_bar_operation(state: &Rc<RefCell<SubclassState>>) -> Option<AppBar> {
    state.borrow_mut().begin_operation()
}

pub(crate) fn finish_app_bar_operation(state: &Rc<RefCell<SubclassState>>, mut app_bar: AppBar) {
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
