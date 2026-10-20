use crate::debug_log;
use crate::{i18n::Message, msg};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowDisplayAffinity, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

pub fn app_icon(recording: bool) -> eframe::egui::IconData {
    let mut icon = eframe::icon_data::from_png_bytes(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/app-icon.png"
    )))
    .expect("generated application icon");
    if recording {
        let size = icon.width.min(icon.height) as f32;
        let center = size * 0.76;
        for y in 0..icon.height {
            for x in 0..icon.width {
                let distance = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
                if distance <= size * 0.23 {
                    let color = if distance <= size * 0.18 {
                        [239, 52, 64, 255]
                    } else {
                        [255, 255, 255, 255]
                    };
                    let offset = ((y * icon.width + x) * 4) as usize;
                    icon.rgba[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
    }
    icon
}

pub fn native_handle(cc: &eframe::CreationContext<'_>) -> Result<HWND, Message> {
    match cc
        .window_handle()
        .map_err(|error| Message::from(error.to_string()))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
        _ => Err(msg!(
            "The native Windows handle is unavailable",
            "Оконный дескриптор Windows недоступен"
        )),
    }
}

pub fn set_capture_excluded(window: HWND, excluded: bool) -> Result<(), Message> {
    unsafe {
        SetWindowDisplayAffinity(
            window,
            if excluded {
                WDA_EXCLUDEFROMCAPTURE
            } else {
                WDA_NONE
            },
        )
    }
    .map_err(|error| {
        msg!(
            "Could not update window capture visibility: {error}",
            "Не удалось изменить видимость окна при захвате: {error}"
        )
    })
}

pub fn set_taskbar_visible(window: HWND, visible: bool) {
    unsafe {
        // Explorer caches taskbar/Alt+Tab eligibility for an already visible
        // top-level window. Microsoft documents that a dynamic switch to a
        // non-taskbar style must be wrapped in a hide/change/show cycle.
        let _ = ShowWindow(window, SW_HIDE);

        let mut style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        if visible {
            style &= !WS_EX_TOOLWINDOW.0;
            style |= WS_EX_APPWINDOW.0;
        } else {
            style &= !WS_EX_APPWINDOW.0;
            style |= WS_EX_TOOLWINDOW.0;
        }
        SetWindowLongPtrW(window, GWL_EXSTYLE, style as isize);
        let result = SetWindowPos(
            window,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        // SW_SHOWNOACTIVATE keeps the current foreground application focused.
        // The window stays WS_VISIBLE, so eframe continues polling hotkeys and
        // recorder events while the 1x1 viewport is parked off-screen.
        let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        let taskbar_result = set_taskbar_tab(window, visible);
        debug_log::write(format!(
            "taskbar visibility set to {visible}; frame update={}; shell update={}",
            result.is_ok(),
            taskbar_result.is_ok()
        ));
    }
}

unsafe fn set_taskbar_tab(
    window: windows::Win32::Foundation::HWND,
    visible: bool,
) -> windows::core::Result<()> {
    // eframe does not promise a COM apartment on its UI thread. Initialize one
    // for this short Shell call; if another apartment already exists,
    // CoCreateInstance can still use it and we simply must not uninitialize it.
    let com_result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let should_uninitialize = com_result.is_ok();

    let result = (|| unsafe {
        let taskbar: ITaskbarList = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)?;
        taskbar.HrInit()?;
        if visible {
            taskbar.AddTab(window)
        } else {
            taskbar.DeleteTab(window)
        }
    })();

    if should_uninitialize {
        unsafe { CoUninitialize() };
    }
    result
}
