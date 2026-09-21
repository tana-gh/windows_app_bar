use std::{cell::RefCell, ffi::c_void, marker::PhantomData, rc::Rc};

use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, WebWindowHandle, Win32WindowHandle, WindowHandle,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    UI::{
        Shell::{
            ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABM_WINDOWPOSCHANGED, ABN_POSCHANGED, APPBARDATA,
        },
        WindowsAndMessaging::WM_WINDOWPOSCHANGED,
    },
};

use crate::{
    APP_BAR_CALLBACK_MESSAGE, AppBar, AppBarError, Edge, SubclassedAppBar,
    app_bar::hwnd_from_window,
    platform::AppBarApi,
    subclass::{
        SubclassMessageDispatch, SubclassState, WindowSubclassApi, begin_app_bar_operation,
        complete_subclass_registration, detach_subclass_state_with_api, dispatch_subclass_message,
        finish_app_bar_operation, install_window_subclass_with_api, registered_subclass_state,
        release_subclass_state, reserve_subclass_state,
    },
};

#[path = "tests/integration.rs"]
mod integration;

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
    fn call_next(&self, _hwnd: HWND, _message: u32, _wparam: WPARAM, _lparam: LPARAM) -> LRESULT {
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
fn position_changed_notification_proposes_a_monitor_edge_rect() {
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
    assert_eq!(
        messages[0],
        (
            ABM_QUERYPOS,
            RECT {
                left: 0,
                top: 1050,
                right: 1920,
                bottom: 1080
            }
        )
    );
    assert_eq!(messages[1].0, ABM_SETPOS);
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
fn failed_reposition_keeps_the_previous_configuration() {
    let mut app_bar = app_bar_for_test(Rc::new(MockAppBarApi::new(RECT::default(), true)));
    assert!(app_bar.set_size(40).is_err());
    assert_eq!(app_bar.size(), 30);
    assert!(app_bar.set_edge(Edge::Left).is_err());
    assert_eq!(app_bar.edge(), Edge::Bottom);
    assert!(matches!(
        app_bar.set_monitor_index(1),
        Err(AppBarError::MonitorNotFound { index: 1 })
    ));
}

struct TestWindow(HWND);
impl HasWindowHandle for TestWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle = Win32WindowHandle::new(
            std::num::NonZeroIsize::new(self.0.0 as isize).ok_or(HandleError::Unavailable)?,
        );
        unsafe { Ok(WindowHandle::borrow_raw(RawWindowHandle::Win32(handle))) }
    }
}
struct NonWin32TestWindow;
impl HasWindowHandle for NonWin32TestWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        unsafe {
            Ok(WindowHandle::borrow_raw(RawWindowHandle::Web(
                WebWindowHandle::new(0),
            )))
        }
    }
}

#[test]
fn hwnd_from_window_validates_the_handle_type() {
    let window = TestWindow(HWND(std::ptr::dangling_mut::<c_void>()));
    assert_eq!(hwnd_from_window(&window).unwrap(), window.0);
    assert!(matches!(
        hwnd_from_window(&NonWin32TestWindow),
        Err(AppBarError::NotAWin32Window)
    ));
}

#[test]
fn subclass_installation_is_testable_without_a_real_window() {
    let success = MockWindowSubclassApi::new(false, false);
    install_window_subclass_with_api(HWND::default(), &success).unwrap();
    assert_eq!(*success.calls.borrow(), ["install"]);
    let failure = MockWindowSubclassApi::new(true, false);
    assert!(matches!(
        install_window_subclass_with_api(HWND::default(), &failure),
        Err(AppBarError::Windows(_))
    ));
}

#[test]
fn explicit_unregister_removes_a_registered_app_bar_once() {
    let api = Rc::new(MockAppBarApi::new(RECT::default(), false));
    app_bar_for_test(api.clone()).unregister().unwrap();
    assert_eq!(
        api.messages.borrow().as_slice(),
        &[(ABM_REMOVE, RECT::default())]
    );
}

#[test]
fn duplicate_subclass_registration_keeps_the_original_state() {
    let hwnd = HWND::default();
    let original = subclass_state_for_test(None);
    reserve_subclass_state(hwnd, original.clone()).unwrap();
    assert!(matches!(
        reserve_subclass_state(hwnd, subclass_state_for_test(None)),
        Err(AppBarError::SubclassAlreadyRegistered)
    ));
    assert!(Rc::ptr_eq(
        &registered_subclass_state(hwnd).unwrap(),
        &original
    ));
    release_subclass_state(hwnd, &original);
}

#[test]
fn registration_failure_removes_the_installed_subclass() {
    let hwnd = HWND::default();
    let state = subclass_state_for_test(None);
    state.borrow_mut().operation_in_progress = true;
    reserve_subclass_state(hwnd, state.clone()).unwrap();
    let api = MockWindowSubclassApi::new(false, false);
    assert!(matches!(
        complete_subclass_registration(
            hwnd,
            &state,
            Err(AppBarError::ShellOperationFailed {
                operation: "registration"
            }),
            &api
        ),
        Err(AppBarError::ShellOperationFailed { .. })
    ));
    assert_eq!(*api.calls.borrow(), ["remove"]);
    assert!(registered_subclass_state(hwnd).is_none());
}

#[test]
fn registration_failure_preserves_both_errors_when_rollback_fails() {
    let hwnd = HWND::default();
    let state = subclass_state_for_test(None);
    state.borrow_mut().operation_in_progress = true;
    reserve_subclass_state(hwnd, state.clone()).unwrap();
    let api = MockWindowSubclassApi::new(false, true);
    assert!(matches!(
        complete_subclass_registration(
            hwnd,
            &state,
            Err(AppBarError::ShellOperationFailed {
                operation: "registration"
            }),
            &api
        ),
        Err(AppBarError::SubclassRegistrationCleanup { .. })
    ));
    assert!(state.borrow().attached);
    release_subclass_state(hwnd, &state);
}

#[test]
fn dispatch_defers_reentrant_messages_and_forwards_others() {
    let state = subclass_state_for_test(Some(app_bar_for_test(Rc::new(MockAppBarApi::new(
        RECT::default(),
        false,
    )))));
    state.borrow_mut().operation_in_progress = true;
    assert!(matches!(
        dispatch_subclass_message(&state, APP_BAR_CALLBACK_MESSAGE, ABN_POSCHANGED as usize),
        SubclassMessageDispatch::Consume
    ));
    assert!(matches!(
        dispatch_subclass_message(&state, WM_WINDOWPOSCHANGED, 0),
        SubclassMessageDispatch::Forward
    ));
    assert!(state.borrow().pending_reposition);
    assert!(state.borrow().pending_window_position_changed);
}

#[test]
fn detach_keeps_state_when_subclass_removal_fails() {
    let hwnd = HWND::default();
    let state = subclass_state_for_test(None);
    reserve_subclass_state(hwnd, state.clone()).unwrap();
    assert!(matches!(
        detach_subclass_state_with_api(hwnd, &MockWindowSubclassApi::new(false, true)),
        Err(AppBarError::Windows(_))
    ));
    assert!(state.borrow().attached);
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
fn deferred_notifications_are_drained_after_an_operation() {
    let api = Rc::new(MockAppBarApi::new(
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        },
        false,
    ));
    let state = subclass_state_for_test(Some(app_bar_for_test(api.clone())));
    let app_bar = begin_app_bar_operation(&state).unwrap();
    {
        let mut state = state.borrow_mut();
        state.pending_reposition = true;
        state.pending_window_position_changed = true;
    }
    finish_app_bar_operation(&state, app_bar);
    assert_eq!(
        api.messages
            .borrow()
            .iter()
            .map(|(message, _)| *message)
            .collect::<Vec<_>>(),
        vec![ABM_QUERYPOS, ABM_SETPOS, ABM_WINDOWPOSCHANGED]
    );
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
    let state = subclass_state_for_test(None);
    state.borrow_mut().operation_in_progress = true;
    assert!(matches!(
        dispatch_subclass_message(&state, APP_BAR_CALLBACK_MESSAGE, ABN_POSCHANGED as usize),
        SubclassMessageDispatch::Consume
    ));
    assert!(matches!(
        dispatch_subclass_message(&state, WM_WINDOWPOSCHANGED, 0),
        SubclassMessageDispatch::Forward
    ));
    finish_app_bar_operation(&state, app_bar_for_test(api.clone()));
    assert_eq!(
        api.messages
            .borrow()
            .iter()
            .map(|(message, _)| *message)
            .collect::<Vec<_>>(),
        vec![ABM_QUERYPOS, ABM_SETPOS, ABM_WINDOWPOSCHANGED]
    );
}

#[test]
fn subclassed_accessors_read_the_registered_app_bar() {
    let app_bar = app_bar_for_test(Rc::new(MockAppBarApi::new(RECT::default(), false)));
    let subclassed = SubclassedAppBar {
        hwnd: HWND::default(),
        state: subclass_state_for_test(Some(app_bar)),
        _thread_affinity: PhantomData,
    };
    assert_eq!(subclassed.edge(), Edge::Bottom);
    assert_eq!(subclassed.size(), 30);
    assert!(subclassed.is_registered());
}
