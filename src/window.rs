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
    SetWindowPos, ShowWindow, WDA_EXCLUDEFROMCAPTURE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

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

pub fn exclude_from_capture(window: HWND) -> Result<(), Message> {
    unsafe { SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE) }.map_err(|error| {
        msg!(
            "Could not exclude the window from capture: {error}",
            "Не удалось исключить окно из захвата: {error}"
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
