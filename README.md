# windows_app_bar
Windows AppBar API

## Example

The `basic` example creates a bottom AppBar using only Win32 APIs and forwards
its window messages to `AppBar`.

```powershell
cargo run -p windows_app_bar_example_basic
```

The `bevy` example creates an empty, borderless Bevy window and registers it
as a bottom AppBar.

```powershell
cargo run -p windows_app_bar_example_bevy
```
