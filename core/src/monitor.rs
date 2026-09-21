use std::mem::size_of;

use windows::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO},
};

use crate::{AppBarError, MonitorInfo};

/// Enumerates monitors in the order accepted by [`crate::AppBar::register`]
/// and [`crate::SubclassedAppBar::register`].
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, AppBarError> {
    let mut handles = Vec::<HMONITOR>::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor_handle),
            LPARAM((&mut handles as *mut Vec<HMONITOR>) as isize),
        )
        .ok()?;
    }

    handles
        .into_iter()
        .enumerate()
        .map(|(index, handle)| {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            unsafe { GetMonitorInfoW(handle, &mut info).ok()? };
            Ok(MonitorInfo {
                index,
                bounds: info.rcMonitor.into(),
            })
        })
        .collect()
}

unsafe extern "system" fn collect_monitor_handle(
    monitor: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    // `data` points to `handles`, which remains alive for the synchronous
    // EnumDisplayMonitors call above.
    unsafe { (&mut *(data.0 as *mut Vec<HMONITOR>)).push(monitor) };
    true.into()
}
