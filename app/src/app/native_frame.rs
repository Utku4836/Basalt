//! Integrate the client toolbar into the native Windows frame. WS_CAPTION and
//! the system caption buttons remain owned by Windows, including Snap layouts.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Dwm::{
        DWMWA_CAPTION_BUTTON_BOUNDS, DwmDefWindowProc, DwmExtendFrameIntoClientArea,
        DwmGetWindowAttribute, DwmSetWindowAttribute,
    },
    Graphics::Gdi::{ClientToScreen, ScreenToClient},
    UI::{
        Controls::MARGINS,
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const SUBCLASS_ID: usize = 0x43524f4d;

/// The caption button under the pointer (0 minimize, 1 maximize, 2 close, -1 none) and
/// whether it is held down. Windows owns these buttons' input; the app draws them.
static CAPTION_HOVER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static CAPTION_PRESSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn caption_state() -> (i32, bool) {
    use std::sync::atomic::Ordering::Relaxed;
    (CAPTION_HOVER.load(Relaxed), CAPTION_PRESSED.load(Relaxed))
}

fn caption_button(hit: usize) -> i32 {
    match hit as u32 {
        HTMINBUTTON => 0,
        HTMAXBUTTON => 1,
        HTCLOSE => 2,
        _ => -1,
    }
}
/// Controlled native-event regression on this process's own test window.
/// Does not change Windows display settings or other applications.
#[cfg(debug_assertions)]
pub fn simulate_dpi_for_test(window: &slint::winit_030::winit::window::Window, dpi: u32) -> bool {
    let Some(handle) = hwnd(window) else {
        return false;
    };
    if !(48..=768).contains(&dpi) {
        return false;
    }
    let mut bounds = rect();
    unsafe {
        if GetWindowRect(handle, &mut bounds) == 0 {
            return false;
        }
        SendMessageW(
            handle,
            WM_DPICHANGED,
            (dpi | dpi << 16) as WPARAM,
            (&mut bounds as *mut RECT) as LPARAM,
        );
    }
    true
}
pub const HEADER_HEIGHT: f64 = 32.0;
struct FrameState {
    header_height: f64,
    control_left: f64,
    control_width: f64,
    background: u32,
    corners: u32,
}
pub fn set_appearance(window: &slint::winit_030::winit::window::Window, rgb: u32, rounded: bool) {
    let Some(handle) = hwnd(window) else {
        return;
    };
    let state = unsafe { GetPropW(handle, property_name().as_ptr()) };
    if !state.is_null() {
        unsafe {
            let state = &mut *(state as *mut FrameState);
            let background = ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff);
            let corners = if rounded { 2 } else { 1 };
            if state.background == background && state.corners == corners {
                return;
            }
            state.background = background;
            state.corners = corners;
            apply_appearance(handle);
        }
    }
}
pub fn set_control_region(window: &slint::winit_030::winit::window::Window, left: f64, width: f64) {
    if !left.is_finite() || !width.is_finite() || left < 0.0 || !(0.0..=16384.0).contains(&width) {
        return;
    }
    let Some(handle) = hwnd(window) else {
        return;
    };
    let state = unsafe { GetPropW(handle, property_name().as_ptr()) };
    if !state.is_null() {
        unsafe {
            let state = &mut *(state as *mut FrameState);
            state.control_left = left;
            state.control_width = width;
        }
    }
}
fn header_height(handle: HWND) -> f64 {
    let state = unsafe { GetPropW(handle, property_name().as_ptr()) };
    if state.is_null() {
        HEADER_HEIGHT
    } else {
        unsafe { (*(state as *const FrameState)).header_height }
    }
}
pub fn set_header_height(window: &slint::winit_030::winit::window::Window, height: f64) {
    if !height.is_finite() || !(32.0..=72.0).contains(&height) {
        return;
    }
    let Some(handle) = hwnd(window) else {
        return;
    };
    let state = unsafe { GetPropW(handle, property_name().as_ptr()) };
    if !state.is_null() {
        unsafe {
            (*(state as *mut FrameState)).header_height = height;
            apply_appearance(handle);
        }
    }
}
fn rect() -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }
}
fn property_name() -> Vec<u16> {
    "workspace.frontend.integrated-frame"
        .encode_utf16()
        .chain(Some(0))
        .collect()
}
fn hwnd(window: &slint::winit_030::winit::window::Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}
pub fn install(window: &slint::winit_030::winit::window::Window) -> Result<f32, std::io::Error> {
    let handle =
        hwnd(window).ok_or_else(|| std::io::Error::other("Windows window handle is missing"))?;
    let property = property_name();
    if !unsafe { GetPropW(handle, property.as_ptr()) }.is_null() {
        return Ok(caption_width(handle));
    }
    let state = Box::into_raw(Box::new(FrameState {
        header_height: HEADER_HEIGHT,
        control_left: 0.0,
        control_width: 0.0,
        background: 0x00271a15,
        corners: 1,
    }));
    // The box is owned by the subclass and released at WM_NCDESTROY.
    unsafe {
        if SetWindowSubclass(handle, Some(frame_proc), SUBCLASS_ID, state as usize) == 0 {
            drop(Box::from_raw(state));
            return Err(std::io::Error::last_os_error());
        }
        SetPropW(handle, property.as_ptr(), state.cast());
        apply_appearance(handle);
        apply_icons(handle);
        SetWindowPos(
            handle,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
    Ok(caption_width(handle))
}
fn caption_width(handle: HWND) -> f32 {
    let mut bounds = rect();
    let dpi = unsafe { GetDpiForWindow(handle) }.max(96) as f32 / 96.0;
    let result = unsafe {
        DwmGetWindowAttribute(
            handle,
            DWMWA_CAPTION_BUTTON_BOUNDS as u32,
            (&mut bounds as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if result >= 0 && bounds.right > bounds.left {
        ((bounds.right - bounds.left) as f32 / dpi).max(138.0)
    } else {
        138.0
    }
}
/// Give the window the program's icon at the sizes Windows draws it for this
/// display, each picked from its own drawing in the icon file rather than
/// scaled down from a large picture.
unsafe fn apply_icons(handle: HWND) {
    const PROGRAM_ICON: usize = 1;
    unsafe {
        let module = GetWindowLongPtrW(handle, GWLP_HINSTANCE) as _;
        let dpi = GetDpiForWindow(handle).max(96);
        for (kind, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let size = GetSystemMetricsForDpi(metric, dpi);
            let icon = LoadImageW(module, PROGRAM_ICON as *const u16, IMAGE_ICON, size, size, 0);
            if icon.is_null() {
                continue;
            }
            let previous = SendMessageW(handle, WM_SETICON, kind as usize, icon as isize);
            if previous != 0 {
                DestroyIcon(previous as HICON);
            }
        }
    }
}
unsafe fn apply_appearance(handle: HWND) {
    let enabled = 1_i32;
    let state = unsafe { GetPropW(handle, property_name().as_ptr()) };
    let background = if state.is_null() {
        0x00271a15_u32
    } else {
        unsafe { (*(state as *const FrameState)).background }
    };
    let foreground = 0x00deccc4_u32;
    let corners = if state.is_null() {
        1_u32
    } else {
        unsafe { (*(state as *const FrameState)).corners }
    };
    unsafe {
        DwmSetWindowAttribute(handle, 20, (&enabled as *const i32).cast(), 4);
        DwmSetWindowAttribute(handle, 35, (&background as *const u32).cast(), 4);
        DwmSetWindowAttribute(handle, 36, (&foreground as *const u32).cast(), 4);
        DwmSetWindowAttribute(handle, 33, (&corners as *const u32).cast(), 4);
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyBottomHeight: 0,
            cyTopHeight: (header_height(handle) * f64::from(GetDpiForWindow(handle).max(96)) / 96.0)
                .round() as i32,
        };
        DwmExtendFrameIntoClientArea(handle, &margins);
    }
}
unsafe extern "system" fn frame_proc(
    handle: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    match message {
        WM_NCDESTROY => unsafe {
            RemovePropW(handle, property_name().as_ptr());
            RemoveWindowSubclass(handle, Some(frame_proc), id);
            drop(Box::from_raw(data as *mut FrameState));
            DefSubclassProc(handle, message, wparam, lparam)
        },
        WM_NCCALCSIZE if wparam != 0 => unsafe {
            let top = (*(lparam as *mut NCCALCSIZE_PARAMS)).rgrc[0].top;
            // Keep the native side/bottom resize frame. Remove only its caption.
            DefSubclassProc(handle, message, wparam, lparam);
            let dpi = GetDpiForWindow(handle).max(96);
            let inset = if IsZoomed(handle) != 0 {
                GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                    + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
            } else {
                0
            };
            (*(lparam as *mut NCCALCSIZE_PARAMS)).rgrc[0].top = top + inset;
            0
        },
        WM_NCMOUSEMOVE => unsafe {
            use std::sync::atomic::Ordering::Relaxed;
            let button = caption_button(wparam);
            if CAPTION_HOVER.swap(button, Relaxed) != button && button >= 0 {
                // Told when the pointer leaves the frame, to clear the hover.
                use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: handle,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
            }
            if button < 0 {
                CAPTION_PRESSED.store(false, Relaxed);
            }
            DefSubclassProc(handle, message, wparam, lparam)
        },
        WM_NCMOUSELEAVE | WM_MOUSEMOVE => unsafe {
            use std::sync::atomic::Ordering::Relaxed;
            CAPTION_HOVER.store(-1, Relaxed);
            CAPTION_PRESSED.store(false, Relaxed);
            DefSubclassProc(handle, message, wparam, lparam)
        },
        WM_NCLBUTTONDOWN | WM_NCLBUTTONUP => unsafe {
            use std::sync::atomic::Ordering::Relaxed;
            CAPTION_PRESSED.store(message == WM_NCLBUTTONDOWN && caption_button(wparam) >= 0, Relaxed);
            DefSubclassProc(handle, message, wparam, lparam)
        },
        WM_ACTIVATE | WM_DWMCOMPOSITIONCHANGED | WM_DPICHANGED => unsafe {
            let result = DefSubclassProc(handle, message, wparam, lparam);
            apply_appearance(handle);
            if message == WM_DPICHANGED {
                apply_icons(handle);
            }
            result
        },
        WM_NCHITTEST => unsafe {
            let mut dwm_result = 0;
            if DwmDefWindowProc(handle, message, wparam, lparam, &mut dwm_result) != 0
                && matches!(dwm_result,x if x==HTMINBUTTON as isize || x==HTMAXBUTTON as isize || x==HTCLOSE as isize)
            {
                return dwm_result;
            }
            let mut point = POINT {
                x: (lparam as u32 & 0xffff) as i16 as i32,
                y: ((lparam as u32 >> 16) & 0xffff) as i16 as i32,
            };
            ScreenToClient(handle, &mut point);
            let mut client = rect();
            GetClientRect(handle, &mut client);
            let dpi = GetDpiForWindow(handle).max(96) as f64 / 96.0;
            let border = (GetSystemMetricsForDpi(SM_CYSIZEFRAME, GetDpiForWindow(handle).max(96))
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, GetDpiForWindow(handle).max(96)))
            .max(4);
            if IsZoomed(handle) == 0 && point.y < border {
                return if point.x < border {
                    HTTOPLEFT as isize
                } else if point.x > client.right - border {
                    HTTOPRIGHT as isize
                } else {
                    HTTOP as isize
                };
            }
            if point.y >= 0 && f64::from(point.y) < header_height(handle) * dpi {
                let state = GetPropW(handle, property_name().as_ptr());
                if !state.is_null() {
                    let state = &*(state as *const FrameState);
                    let x = f64::from(point.x) / dpi;
                    if x >= state.control_left && x < state.control_left + state.control_width {
                        return HTCLIENT as isize;
                    }
                }
                if f64::from(point.x)
                    > f64::from(client.right) - f64::from(caption_width(handle)) * dpi
                {
                    // DWM will normally resolve these. Retain a native fallback
                    // while the frame is being initialized or changing DPI.
                    let right = f64::from(client.right - point.x) / dpi;
                    return if right < 46.0 {
                        HTCLOSE as isize
                    } else if right < 92.0 {
                        HTMAXBUTTON as isize
                    } else {
                        HTMINBUTTON as isize
                    };
                }
                return HTCAPTION as isize;
            }
            DefSubclassProc(handle, message, wparam, lparam)
        },
        _ => unsafe { DefSubclassProc(handle, message, wparam, lparam) },
    }
}
pub fn geometry(window: &slint::winit_030::winit::window::Window) -> Option<serde_json::Value> {
    let handle = hwnd(window)?;
    let mut outer = rect();
    let mut client = rect();
    let mut origin = POINT { x: 0, y: 0 };
    unsafe {
        GetWindowRect(handle, &mut outer);
        GetClientRect(handle, &mut client);
        ClientToScreen(handle, &mut origin);
    }
    let scale = f64::from(unsafe { GetDpiForWindow(handle) }.max(96)) / 96.0;
    let hit = |x: i32, y: i32| {
        let screen_x = origin.x + x;
        let screen_y = origin.y + y;
        let position = ((screen_x as u16 as u32) | ((screen_y as u16 as u32) << 16)) as isize;
        unsafe { SendMessageW(handle, WM_NCHITTEST, 0, position) }
    };
    let caption_color_bgr = {
        let state = unsafe { GetPropW(handle, property_name().as_ptr()) };
        if state.is_null() {
            None
        } else {
            Some(unsafe { (*(state as *const FrameState)).background })
        }
    };
    Some(
        serde_json::json!({"subclass_installed":!unsafe{GetPropW(handle,property_name().as_ptr())}.is_null(),"client_top_inset":origin.y-outer.top,"native_caption_width":caption_width(handle),"caption_color_bgr":caption_color_bgr,"style":unsafe{GetWindowLongPtrW(handle,GWL_STYLE)},"client_size":[client.right,client.bottom],"maximized":unsafe{IsZoomed(handle)!=0},"adaptive_header_height":header_height(handle),"hit_tests":{"lower_header_drag":hit((400.0*scale) as i32,((header_height(handle)-4.0)*scale) as i32),"toolbar":hit((18.0*scale) as i32,(16.0*scale) as i32),"terminal_menu":hit((110.0*scale) as i32,(16.0*scale) as i32),"view_menu":hit((185.0*scale) as i32,(16.0*scale) as i32),"commands":hit(client.right-((f64::from(caption_width(handle))+50.0)*scale) as i32,(16.0*scale) as i32),"drag":hit((400.0*scale) as i32,(16.0*scale) as i32),"minimize":hit(client.right-(115.0*scale) as i32,(16.0*scale) as i32),"maximize":hit(client.right-(69.0*scale) as i32,(16.0*scale) as i32),"close":hit(client.right-(23.0*scale) as i32,(16.0*scale) as i32)}}),
    )
}

/// Capture this application's own full window, including DWM caption controls.
/// A renderer snapshot contains only Slint's client layers and cannot prove
/// that the native caption controls are visible.
pub fn capture(
    window: &slint::winit_030::winit::window::Window,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows_sys::Win32::Graphics::Gdi::*;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
    }
    let handle = hwnd(window).ok_or("Windows handle is missing")?;
    let mut bounds = rect();
    unsafe {
        GetWindowRect(handle, &mut bounds);
    }
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    if width <= 0 || height <= 0 || width > 16384 || height > 16384 {
        return Err("Invalid window capture bounds".into());
    }
    let screen = unsafe { GetDC(handle) };
    let memory = unsafe { CreateCompatibleDC(screen) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if screen.is_null() || memory.is_null() || bitmap.is_null() {
        unsafe {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !memory.is_null() {
                DeleteDC(memory);
            }
            if !screen.is_null() {
                ReleaseDC(handle, screen);
            }
        }
        return Err(std::io::Error::last_os_error().into());
    }
    let previous = unsafe { SelectObject(memory, bitmap) };
    let captured = unsafe { PrintWindow(handle, memory, 2) };
    unsafe {
        SelectObject(memory, previous);
    }
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let lines = unsafe {
        GetDIBits(
            screen,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(handle, screen);
    }
    if captured == 0 || lines != height {
        return Err("Full native window capture failed".into());
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(std::fs::File::create(path)?),
        width as u32,
        height as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(())
}
