//! A borderless Bevy window registered as a Windows Shell AppBar.
//!
//! The winit window procedure is subclassed so Shell AppBar notifications can
//! be forwarded to `windows_app_bar`.  This is Windows-specific integration;
//! normal Bevy window events remain handled by winit's original procedure.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
fn main() {
    eprintln!("This example must be run on Windows.");
}

#[cfg(windows)]
mod app {
    use std::{cell::RefCell, collections::HashMap, mem::transmute};

    use bevy::{
        ecs::system::NonSendMarker,
        prelude::*,
        window::{PrimaryWindow, WindowPlugin},
        winit::WINIT_WINDOWS,
    };
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        UI::WindowsAndMessaging::{
            CallWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_CLOSE, WM_NCDESTROY,
            WM_WINDOWPOSCHANGED, WNDPROC,
        },
    };
    use windows_app_bar::AppBar;

    const APP_BAR_HEIGHT: u32 = 48;

    thread_local! {
        // Winit windows and AppBars are both confined to the event-loop thread.
        static APP_BARS: RefCell<HashMap<isize, Box<AppBarBinding>>> = RefCell::new(HashMap::new());
    }

    struct AppBarBinding {
        app_bar: Option<AppBar>,
        previous_wnd_proc: WNDPROC,
        handling_message: bool,
        pending_window_position_changed: bool,
    }

    pub fn run() {
        App::new()
            .add_plugins(DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "windows_app_bar Bevy example".into(),
                    resolution: (1280, APP_BAR_HEIGHT).into(),
                    decorations: false,
                    resizable: false,
                    ..default()
                }),
                ..default()
            }))
            .add_systems(Startup, install_primary_app_bar)
            .run();
    }

    fn install_primary_app_bar(
        _main_thread: NonSendMarker,
        primary_window: Query<Entity, With<PrimaryWindow>>,
    ) {
        let Ok(entity) = primary_window.single() else {
            eprintln!("primary Bevy window was not found");
            return;
        };

        WINIT_WINDOWS.with_borrow(|windows| {
            let Some(window) = windows.get_window(entity) else {
                eprintln!("winit window was not created");
                return;
            };

            if let Err(error) = install_app_bar(&**window) {
                eprintln!("failed to install Bevy AppBar: {error}");
            }
        });
    }

    fn install_app_bar(window: &impl raw_window_handle::HasWindowHandle) -> Result<(), String> {
        let hwnd = hwnd_from_window(window)?;
        let previous_wnd_proc = unsafe {
            // The winit procedure remains responsible for every message this
            // example does not consume.
            SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                app_bar_window_proc as *const () as usize as isize,
            )
        };
        let previous_wnd_proc = unsafe { transmute::<isize, WNDPROC>(previous_wnd_proc) };

        APP_BARS.with(|bindings| {
            bindings.borrow_mut().insert(
                hwnd.0 as isize,
                Box::new(AppBarBinding {
                    app_bar: None,
                    previous_wnd_proc,
                    handling_message: false,
                    pending_window_position_changed: false,
                }),
            );
        });

        match AppBar::register(window, windows_app_bar::Edge::Bottom, APP_BAR_HEIGHT) {
            Ok(app_bar) => {
                APP_BARS.with(|bindings| {
                    let mut bindings = bindings.borrow_mut();
                    let binding = bindings
                        .get_mut(&(hwnd.0 as isize))
                        .expect("AppBar binding was installed");
                    binding.app_bar = Some(app_bar);
                });
                Ok(())
            }
            Err(error) => {
                remove_app_bar_binding(hwnd);
                Err(error.to_string())
            }
        }
    }

    fn hwnd_from_window(window: &impl raw_window_handle::HasWindowHandle) -> Result<HWND, String> {
        use raw_window_handle::RawWindowHandle;

        let handle = window.window_handle().map_err(|error| error.to_string())?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err("Bevy did not create a Win32 window".into());
        };
        Ok(HWND(handle.hwnd.get() as *mut _))
    }

    unsafe extern "system" fn app_bar_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let (consumed, previous_wnd_proc) = APP_BARS.with(|bindings| {
            let binding = {
                let bindings = bindings.borrow();
                bindings
                    .get(&(hwnd.0 as isize))
                    .map(|binding| &**binding as *const AppBarBinding as *mut AppBarBinding)
            };

            let Some(binding) = binding else {
                return (false, None);
            };

            // The pointer is stable while this entry remains in the map. The
            // binding is removed only after its original WndProc is restored.
            let binding = unsafe { &mut *binding };
            let previous_wnd_proc = binding.previous_wnd_proc;

            if message == WM_CLOSE || message == WM_NCDESTROY {
                remove_app_bar_binding(hwnd);
                return (false, previous_wnd_proc);
            }

            if binding.handling_message {
                if message == WM_WINDOWPOSCHANGED {
                    binding.pending_window_position_changed = true;
                }
                return (false, previous_wnd_proc);
            }

            let Some(app_bar) = binding.app_bar.as_mut() else {
                return (false, previous_wnd_proc);
            };

            binding.handling_message = true;
            let consumed = match app_bar.handle_window_message(message, wparam.0, lparam.0) {
                Ok(consumed) => consumed,
                Err(error) => {
                    eprintln!("AppBar message handling failed: {error}");
                    false
                }
            };
            binding.handling_message = false;

            if binding.pending_window_position_changed {
                binding.pending_window_position_changed = false;
                if let Err(error) = app_bar.handle_window_message(WM_WINDOWPOSCHANGED, 0, 0) {
                    eprintln!("AppBar position notification failed: {error}");
                }
            }

            (consumed, previous_wnd_proc)
        });

        if consumed {
            LRESULT(0)
        } else if let Some(previous_wnd_proc) = previous_wnd_proc {
            unsafe { CallWindowProcW(Some(previous_wnd_proc), hwnd, message, wparam, lparam) }
        } else {
            LRESULT(0)
        }
    }

    fn remove_app_bar_binding(hwnd: HWND) {
        APP_BARS.with(|bindings| {
            let binding = bindings.borrow_mut().remove(&(hwnd.0 as isize));
            if let Some(binding) = binding {
                unsafe {
                    SetWindowLongPtrW(
                        hwnd,
                        GWLP_WNDPROC,
                        transmute::<WNDPROC, isize>(binding.previous_wnd_proc),
                    );
                }
                drop(binding);
            }
        });
    }
}

#[cfg(windows)]
fn main() {
    app::run();
}
