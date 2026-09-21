use raw_window_handle::HandleError;
use thiserror::Error;

/// Failures returned by [`crate::AppBar`].
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
