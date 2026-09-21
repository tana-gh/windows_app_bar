#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
fn main() {
    eprintln!("This example must be run on Windows.");
}

#[cfg(windows)]
mod app {
    use std::{mem::MaybeUninit, num::NonZeroIsize};

    use raw_window_handle::{
        HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
    };
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            Graphics::Gdi::{
                BeginPaint, EndPaint, FillRect, GetStockObject, HBRUSH, PAINTSTRUCT, WHITE_BRUSH,
            },
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW,
                HMENU, IDC_ARROW, LoadCursorW, MSG, PostQuitMessage, RegisterClassW, SW_SHOW,
                SetWindowLongPtrW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE,
                WM_DESTROY, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASSW, WS_POPUP,
            },
        },
        core::{Error, HRESULT, Result, w},
    };
    use windows_app_bar::{AppBar, Edge};

    const CLASS_NAME: windows::core::PCWSTR = w!("windows_app_bar_basic");
    const WINDOW_TITLE: windows::core::PCWSTR = w!("windows_app_bar basic example");

    struct Window(HWND);

    impl HasWindowHandle for Window {
        fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
            let hwnd = NonZeroIsize::new(self.0.0 as isize).ok_or(HandleError::Unavailable)?;
            let handle = Win32WindowHandle::new(hwnd);

            // `self` owns no HWND; the event loop keeps the native window alive
            // for the complete lifetime of the AppBar.
            unsafe { Ok(WindowHandle::borrow_raw(RawWindowHandle::Win32(handle))) }
        }
    }

    struct WindowState {
        app_bar: Option<AppBar>,
    }

    pub fn run() -> Result<()> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let cursor = LoadCursorW(None, IDC_ARROW)?;
            let class = WNDCLASSW {
                hInstance: instance.into(),
                hCursor: cursor,
                hbrBackground: white_brush(),
                lpszClassName: CLASS_NAME,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                ..Default::default()
            };
            RegisterClassW(&class);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS_NAME,
                WINDOW_TITLE,
                WS_POPUP,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                Some(HMENU::default()),
                Some(instance.into()),
                None,
            )?;

            // Install state before registering: Shell calls may synchronously
            // reach the WndProc during AppBar registration.
            let state = Box::new(WindowState { app_bar: None });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

            let app_bar = match AppBar::register(&Window(hwnd), 0, Edge::Bottom, 48) {
                Ok(app_bar) => app_bar,
                Err(error) => {
                    let state = Box::from_raw(window_state(hwnd));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let _ = DestroyWindow(hwnd);
                    drop(state);
                    return Err(Error::new(
                        HRESULT(0x8000_4005u32 as i32),
                        error.to_string(),
                    ));
                }
            };
            (*window_state(hwnd)).app_bar = Some(app_bar);

            let _ = ShowWindow(hwnd, SW_SHOW);

            let mut message = MaybeUninit::<MSG>::zeroed();
            while GetMessageW(message.as_mut_ptr(), None, 0, 0).into() {
                let message = message.assume_init();
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        Ok(())
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }

        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState };
        if !state.is_null() {
            let app_bar = unsafe { &mut (*state).app_bar };
            if let Some(app_bar) = app_bar {
                match app_bar.handle_window_message(message, wparam.0, lparam.0) {
                    Ok(true) => return LRESULT(0),
                    Ok(false) => {}
                    Err(error) => eprintln!("AppBar message handling failed: {error}"),
                }
            }
        }

        match message {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
                unsafe { FillRect(hdc, &paint.rcPaint, white_brush()) };
                let _ = unsafe { EndPaint(hwnd, &paint) };
                LRESULT(0)
            }
            WM_CLOSE => {
                if !state.is_null() {
                    // Remove the AppBar before Windows starts destroying its HWND.
                    unsafe { (*state).app_bar.take() };
                }
                unsafe { DestroyWindow(hwnd) }.ok();
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            WM_NCDESTROY => {
                if !state.is_null() {
                    unsafe { drop(Box::from_raw(state)) };
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                }
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    unsafe fn window_state(hwnd: HWND) -> *mut WindowState {
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState }
    }

    fn white_brush() -> HBRUSH {
        unsafe { HBRUSH(GetStockObject(WHITE_BRUSH).0) }
    }
}

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    app::run()
}
