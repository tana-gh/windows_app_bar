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

mod app_bar;
mod error;
mod monitor;
mod platform;
mod subclass;
mod types;

pub use app_bar::AppBar;
pub use error::AppBarError;
pub use monitor::enumerate_monitors;
pub use subclass::SubclassedAppBar;
pub use types::{APP_BAR_CALLBACK_MESSAGE, Edge, MonitorInfo, MonitorRect};

#[cfg(test)]
mod tests;
