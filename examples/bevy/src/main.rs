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
    use std::cell::RefCell;

    use bevy::{
        app::AppExit,
        ecs::system::NonSendMarker,
        prelude::*,
        window::{PrimaryWindow, WindowCloseRequested, WindowPlugin, close_when_requested},
        winit::WINIT_WINDOWS,
    };
    use windows_app_bar::{Edge, SubclassedAppBar};

    const APP_BAR_HEIGHT: u32 = 48;

    thread_local! {
        // SubclassedAppBar is thread-affine, like the winit window it owns.
        static APP_BAR: RefCell<Option<SubclassedAppBar>> = const { RefCell::new(None) };
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
            // This must run before Bevy removes the native window in response
            // to a close request or AppExit.
            .add_systems(
                Last,
                unregister_app_bar_before_shutdown.before(close_when_requested),
            )
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

            let app_bar = SubclassedAppBar::register(&**window, 0, Edge::Bottom, APP_BAR_HEIGHT);
            match app_bar {
                Ok(app_bar) => APP_BAR.with(|slot| *slot.borrow_mut() = Some(app_bar)),
                Err(error) => eprintln!("failed to install Bevy AppBar: {error}"),
            }
        });
    }

    fn unregister_app_bar_before_shutdown(
        _main_thread: NonSendMarker,
        mut close_requests: MessageReader<WindowCloseRequested>,
        mut app_exits: MessageReader<AppExit>,
    ) {
        if close_requests.read().next().is_none() && app_exits.read().next().is_none() {
            return;
        }

        // End the RefCell borrow before unregistering, because Win32 calls can
        // synchronously re-enter the window procedure.
        let app_bar = APP_BAR.with(|slot| slot.borrow_mut().take());
        if let Some(app_bar) = app_bar
            && let Err(error) = app_bar.unregister()
        {
            eprintln!("failed to unregister Bevy AppBar during shutdown: {error}");
        }
    }
}

#[cfg(windows)]
fn main() {
    app::run();
}
