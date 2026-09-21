# windows_app_bar

A Windows-only wrapper around the Shell AppBar API. An AppBar reserves space
at an edge of a selected monitor, such as for a desktop toolbar.

Use `enumerate_monitors()` to select a zero-based `monitor_index`. The index
is the current Win32 monitor enumeration order, not the number shown in
Windows Display Settings.

## Choose an integration style

- `AppBar` is for applications that own their WndProc. Forward native messages
  to `handle_window_message` yourself.
- `SubclassedAppBar` installs a Common Controls window subclass and performs
  that forwarding automatically. It is generally the right choice for an
  existing framework window, including Bevy or winit windows.

Both types must be created and used on the HWND's owning thread.

### Manual WndProc forwarding with `AppBar`

`window` below is the application wrapper around a live HWND and implements
`HasWindowHandle`.

```rust
use windows_app_bar::{AppBar, Edge};

let mut app_bar = AppBar::register(&window, 0, Edge::Bottom, 48)?;

// In the owner WndProc, before forwarding to the application's default logic:
let consumed = app_bar.handle_window_message(message, wparam, lparam)?;
if consumed {
    return 0;
}

// Before DestroyWindow(hwnd):
app_bar.unregister()?;
```

### Automatic forwarding with `SubclassedAppBar`

Keep the returned value in application state for as long as the HWND is an
AppBar. Call `unregister()` before destroying that HWND so cleanup failures can
be handled by the application.

```rust
use windows_app_bar::{Edge, SubclassedAppBar};

struct WindowState {
    app_bar: Option<SubclassedAppBar>,
}

let app_bar = SubclassedAppBar::register(&window, 0, Edge::Bottom, 48)?;
state.app_bar = Some(app_bar);

// Before DestroyWindow(hwnd):
state.app_bar.take().unwrap().unregister()?;
```

If explicit cleanup is missed, `Drop` and `WM_DESTROY` attempt best-effort
cleanup but cannot return an error. `WM_CLOSE` is forwarded to the original
window procedure.

## Example

The [`basic` example](examples/basic/src/main.rs) creates a bottom AppBar using
only Win32 APIs and forwards its window messages to `AppBar`.

```powershell
cargo run -p windows_app_bar_example_basic
```

The [`bevy` example](examples/bevy/src/main.rs) creates an empty, borderless
Bevy window and registers it as a bottom AppBar through `SubclassedAppBar`.

```powershell
cargo run -p windows_app_bar_example_bevy
```
