use std::{
    num::NonZeroIsize,
    sync::{Mutex, Once},
};

use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
            HMENU, RegisterClassW, SendMessageW, SetWindowLongPtrW, WINDOW_EX_STYLE, WM_CLOSE,
            WNDCLASSW, WS_POPUP,
        },
    },
    core::w,
};

use crate::{AppBarError, Edge, SubclassedAppBar};

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
    unsafe { SetWindowLongPtrW(window.0, GWLP_USERDATA, (&raw mut close_count) as isize) };

    let Some(app_bar) = try_register_integration_app_bar(&window) else {
        unsafe { DestroyWindow(window.0).expect("destroy test window") };
        return;
    };
    unsafe { SendMessageW(window.0, WM_CLOSE, None, None) };
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
