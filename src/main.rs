// Yeet: Windows 11-style "End task" for Windows 10 taskbar buttons.
#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    RECT, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Children};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, EnumChildWindows, EnumWindows, GetAncestor, GetClassNameW,
    GetCursorPos, GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, LoadIconW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetWindowPos, SetWindowsHookExW,
    TrackPopupMenu, TrackPopupMenuEx, TranslateMessage, UnhookWindowsHookEx, WindowFromPoint,
    GA_ROOT, GWL_EXSTYLE, GW_OWNER, HHOOK, HWND_TOPMOST, IDI_APPLICATION, MF_CHECKED,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, MSGF_MENU, MSLLHOOKSTRUCT, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, TPMPARAMS, TPM_BOTTOMALIGN, TPM_HORIZONTAL, TPM_LEFTALIGN,
    TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN, TPM_VERTICAL, WH_MOUSE_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY,
    WM_ENTERIDLE, WM_LBUTTONUP, WM_NULL, WM_RBUTTONDOWN, WM_RBUTTONUP, WNDCLASSW, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

const WM_TRAY: u32 = WM_APP + 1;
const WM_SHOW_END_TASK_MENU: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
// Not exported by WindowsAndMessaging in windows 0.58.
const WM_MOUSELEAVE: u32 = 0x02A3;
const ID_ENABLED: u32 = 1001;
const ID_STARTUP: u32 = 1002;
const ID_UNINSTALL: u32 = 1003;
const ID_EXIT: u32 = 1004;
const ID_END_TASK: u32 = 2001;
// A click this many pixels from a taskbar button (the gap between icons) still counts as on it.
const MAX_BUTTON_GAP: i32 = 6;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Yeet";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const PROTECTED: [&str; 14] = [
    "explorer.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "smss.exe",
    "svchost.exe",
    "dwm.exe",
    "sihost.exe",
    "fontdrvhost.exe",
    "ctfmon.exe",
    "runtimebroker.exe",
    "taskhostw.exe",
];

static ENABLED: AtomicBool = AtomicBool::new(true);
// Set when we swallow a Shift+right-button-down, so the matching button-up is swallowed too.
static SWALLOW_UP: AtomicBool = AtomicBool::new(false);
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

// Windows 10 adds " - 1 running window" (or "windows") to the button name; the window title does not have it.
fn strip_running_suffix(name: &str) -> &str {
    if let Some(index) = name.rfind(" - ") {
        let tail = &name[index + 3..];
        if tail.ends_with("running window") || tail.ends_with("running windows") {
            return &name[..index];
        }
    }
    name
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn is_protected(pid: u32, exe: &str) -> bool {
    if pid == 0 || pid == 4 || pid == std::process::id() {
        return true;
    }
    let lower = exe.to_ascii_lowercase();
    PROTECTED.iter().any(|name| *name == lower)
}

unsafe fn shift_down() -> bool {
    GetAsyncKeyState(VK_SHIFT.0 as i32) < 0
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

unsafe fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

// Low-level mouse hook: swallows Shift + right-click on the taskbar and asks the
// hidden window to show the End task menu. Does no real work itself. It runs on its
// own thread, so slow work on the window thread (UI Automation, the open menu) can
// never delay mouse input or make Windows drop the hook.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        if msg == WM_RBUTTONUP && SWALLOW_UP.swap(false, Ordering::SeqCst) {
            return LRESULT(1);
        }
        if msg == WM_RBUTTONDOWN && ENABLED.load(Ordering::SeqCst) && shift_down() {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            if taskbar_root_at(info.pt).is_some() {
                SWALLOW_UP.store(true, Ordering::SeqCst);
                let main = HWND(MAIN_HWND.load(Ordering::SeqCst) as *mut c_void);
                let _ = PostMessageW(main, WM_SHOW_END_TASK_MENU, WPARAM(0), LPARAM(0));
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

// Finds the taskbar root (Shell_TrayWnd or Shell_SecondaryTrayWnd) under a point.
unsafe fn taskbar_root_at(pt: POINT) -> Option<HWND> {
    let hit = WindowFromPoint(pt);
    if hit.0.is_null() {
        return None;
    }
    let root = GetAncestor(hit, GA_ROOT);
    let class = class_name(root);
    (class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd").then_some(root)
}

unsafe extern "system" fn find_task_list_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if class_name(hwnd) == "MSTaskListWClass" {
        *(lparam.0 as *mut HWND) = hwnd;
        return BOOL(0);
    }
    BOOL(1)
}

// The window that holds the taskbar buttons, inside a taskbar root.
unsafe fn task_list_of(root: HWND) -> Option<HWND> {
    let mut found = HWND::default();
    let _ = EnumChildWindows(root, Some(find_task_list_proc), LPARAM(&mut found as *mut HWND as isize));
    (!found.0.is_null()).then_some(found)
}

// How far a point is outside a rectangle, in pixels (0 when inside).
fn distance_to(rect: &RECT, pt: POINT) -> i32 {
    let dx = (rect.left - pt.x).max(pt.x - (rect.right - 1)).max(0);
    let dy = (rect.top - pt.y).max(pt.y - (rect.bottom - 1)).max(0);
    dx.max(dy)
}

// Name of the taskbar button at the point. Picks the button whose bounding rectangle
// contains the point, or the nearest one within a few pixels, so a click in the gap
// between two icons still resolves instead of depending on one exact hit-test pixel.
unsafe fn button_name_at(task_list: HWND, pt: POINT) -> Option<String> {
    let automation: IUIAutomation =
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
    let list = automation.ElementFromHandle(task_list).ok()?;
    let condition = automation.CreateTrueCondition().ok()?;
    let buttons = list.FindAll(TreeScope_Children, &condition).ok()?;
    let mut best: Option<(i32, String)> = None;
    for index in 0..buttons.Length().ok()? {
        let Ok(button) = buttons.GetElement(index) else { continue };
        let Ok(rect) = button.CurrentBoundingRectangle() else { continue };
        let distance = distance_to(&rect, pt);
        if distance > MAX_BUTTON_GAP || best.as_ref().is_some_and(|(d, _)| distance >= *d) {
            continue;
        }
        let name = button.CurrentName().map(|n| n.to_string()).unwrap_or_default();
        if !name.is_empty() {
            best = Some((distance, name));
        }
    }
    best.map(|(_, name)| name)
}

struct SearchState {
    // Button name with the "- N running windows" suffix removed, e.g. "Notepad".
    name: String,
    // Second pass: also accept a window whose program file name (without .exe) matches.
    match_program: bool,
    found: Vec<HWND>,
}

// Button names look like the window title ("Calculator") or the program name ("Notepad"),
// while the window title is often "Untitled - Notepad". Accept the window if its title
// equals the name, starts or ends with it, or (second pass) its program name matches.
unsafe fn window_matches(hwnd: HWND, state: &SearchState) -> bool {
    let title = window_text(hwnd);
    let name = state.name.as_str();
    if title.is_empty() {
        return false;
    }
    if title == name || title.ends_with(&format!(" - {name}")) || title.starts_with(&format!("{name} - ")) {
        return true;
    }
    if state.match_program {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if let Some(exe) = exe_name_of(pid) {
            let stem = exe.strip_suffix(".exe").unwrap_or(&exe);
            return stem.eq_ignore_ascii_case(name);
        }
    }
    false
}

// True for windows that get a taskbar button: visible, not a tool window, and either
// unowned or explicitly marked as an app window.
unsafe fn is_taskbar_window(hwnd: HWND) -> bool {
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if !IsWindowVisible(hwnd).as_bool() || ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
    owner.0.is_null() || ex_style & WS_EX_APPWINDOW.0 != 0
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut SearchState);
    if is_taskbar_window(hwnd) && window_matches(hwnd, state) {
        state.found.push(hwnd);
    }
    BOOL(1)
}

// Finds every top-level window behind a taskbar button name. A grouped button
// ("Notepad - 3 running windows") matches all of its windows.
unsafe fn find_windows_for_button(name: &str) -> Vec<HWND> {
    for match_program in [false, true] {
        let mut state = SearchState { name: name.to_string(), match_program, found: Vec::new() };
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut state as *mut SearchState as isize));
        if !state.found.is_empty() {
            return state.found;
        }
    }
    Vec::new()
}

unsafe fn exe_name_of(pid: u32) -> Option<String> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        PWSTR(buf.as_mut_ptr()),
        &mut size,
    )
    .is_ok();
    let _ = CloseHandle(handle);
    if !ok {
        return None;
    }
    let full = String::from_utf16_lossy(&buf[..size as usize]);
    full.rsplit('\\').next().map(|s| s.to_string())
}

// Makes our hidden window the foreground window so the popup menu gets keyboard focus and
// closes when the user clicks elsewhere. Windows blocks SetForegroundWindow from a background
// process, so we borrow the input thread of whatever window is currently in front.
unsafe fn take_foreground(hwnd: HWND) {
    let current = GetForegroundWindow();
    let current_thread = GetWindowThreadProcessId(current, None);
    let our_thread = GetCurrentThreadId();
    let attached = current_thread != 0 && current_thread != our_thread
        && AttachThreadInput(our_thread, current_thread, true).as_bool();
    let _ = SetForegroundWindow(hwnd);
    if attached {
        let _ = AttachThreadInput(our_thread, current_thread, false);
    }
}

// One process that can be ended from a taskbar button, with its windows on that button.
struct Target {
    pid: u32,
    exe: String,
    title: String,
    windows: Vec<HWND>,
}

// Finds the processes behind the taskbar button at the point, leaving out protected ones.
unsafe fn resolve_button(task_list: HWND, pt: POINT) -> Vec<Target> {
    let Some(button) = button_name_at(task_list, pt) else { return Vec::new() };
    let windows = find_windows_for_button(strip_running_suffix(&button));
    let mut targets: Vec<Target> = Vec::new();
    for window in windows {
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        if let Some(target) = targets.iter_mut().find(|t| t.pid == pid) {
            target.windows.push(window);
            continue;
        }
        let Some(exe) = exe_name_of(pid) else { continue };
        if is_protected(pid, &exe) {
            continue;
        }
        targets.push(Target { pid, exe, title: window_text(window), windows: vec![window] });
    }
    targets
}

// Shows End task for the taskbar button under the cursor, if its app can be ended.
// One process gives "End task (name.exe)"; a grouped button whose windows belong to
// several processes gives one "End task (window title)" item per process.
// If there is nothing to end, nothing is shown.
unsafe fn show_end_task_menu(hwnd: HWND) {
    let mut pt = POINT::default();
    if !ENABLED.load(Ordering::SeqCst) || GetCursorPos(&mut pt).is_err() {
        return;
    }
    let Some(root) = taskbar_root_at(pt) else { return };
    let Some(task_list) = task_list_of(root) else { return };
    let targets = resolve_button(task_list, pt);
    if targets.is_empty() {
        return;
    }

    let Ok(menu) = CreatePopupMenu() else { return };
    for (index, target) in targets.iter().enumerate() {
        let text = if targets.len() == 1 {
            target.exe.clone()
        } else if targets.iter().filter(|t| t.title == target.title).count() > 1 {
            // Same title on several windows (e.g. "Untitled - Notepad"): add the PID to tell them apart.
            format!("{}, PID {}", target.title, target.pid)
        } else {
            target.title.clone()
        };
        let label = to_wide(&format!("End task ({text})"));
        let _ = AppendMenuW(menu, MF_STRING, ID_END_TASK as usize + index, PCWSTR(label.as_ptr()));
    }

    // Open the menu just outside the taskbar, on the side facing the desktop, the way the
    // taskbar's own menus open. rcExclude keeps it from ever covering the taskbar.
    let mut bar = RECT::default();
    let _ = GetWindowRect(root, &mut bar);
    let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(MonitorFromWindow(root, MONITOR_DEFAULTTONEAREST), &mut monitor);
    let screen = monitor.rcMonitor;
    let horizontal = bar.right - bar.left >= bar.bottom - bar.top;
    let (flags, x, y) = if horizontal {
        if bar.top > screen.top {
            (TPM_VERTICAL | TPM_BOTTOMALIGN | TPM_LEFTALIGN, pt.x, bar.top)
        } else {
            (TPM_VERTICAL | TPM_TOPALIGN | TPM_LEFTALIGN, pt.x, bar.bottom)
        }
    } else if bar.left > screen.left {
        (TPM_HORIZONTAL | TPM_RIGHTALIGN | TPM_TOPALIGN, bar.left, pt.y)
    } else {
        (TPM_HORIZONTAL | TPM_LEFTALIGN | TPM_TOPALIGN, bar.right, pt.y)
    };
    let params = TPMPARAMS { cbSize: size_of::<TPMPARAMS>() as u32, rcExclude: bar };

    // We swallowed the click, so the taskbar still thinks the mouse is hovering and keeps its
    // thumbnail preview open. Tell it the mouse left, which closes the preview like a real click.
    let _ = PostMessageW(task_list, WM_MOUSELEAVE, WPARAM(0), LPARAM(0));
    take_foreground(hwnd);
    let cmd = TrackPopupMenuEx(
        menu,
        (flags | TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
        x,
        y,
        hwnd,
        Some(&params),
    )
    .0 as u32;
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    if let Some(target) = cmd.checked_sub(ID_END_TASK).and_then(|i| targets.get(i as usize)) {
        end_task(target);
    }
}

// Asks the window to close, then force-kills the process after 2 seconds if it is still running.
// The wait happens on its own thread so the message loop and the mouse hook keep running.
unsafe fn end_task(target: &Target) {
    for window in &target.windows {
        let _ = PostMessageW(*window, WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let pid = target.pid;
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        unsafe { force_kill(pid) };
    });
}

unsafe fn force_kill(pid: u32) {
    let Ok(handle) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) else {
        return;
    };
    if WaitForSingleObject(handle, 0) == WAIT_TIMEOUT {
        let _ = TerminateProcess(handle, 1);
    }
    let _ = CloseHandle(handle);
}

fn startup_enabled() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", RUN_VALUE])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn set_startup(on: bool) {
    let mut command = Command::new("reg");
    if on {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        command.args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &format!("\"{exe}\""), "/f"]);
    } else {
        command.args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    }
    let _ = command.creation_flags(CREATE_NO_WINDOW).status();
}

// Removes the startup entry and the exe. The exe is deleted by a short delayed command
// because a running exe cannot delete itself.
unsafe fn uninstall(hwnd: HWND) {
    set_startup(false);
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new("cmd")
            .raw_arg(format!("/C ping -n 3 127.0.0.1 >nul & del /F /Q \"{}\"", exe.display()))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    remove_tray_icon(hwnd);
    let _ = DestroyWindow(hwnd);
}

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default(),
        ..Default::default()
    };
    let tip = to_wide("Yeet");
    let count = tip.len().min(nid.szTip.len());
    nid.szTip[..count].copy_from_slice(&tip[..count]);
    nid
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let nid = tray_data(hwnd);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = tray_data(hwnd);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let enabled_flag = if ENABLED.load(Ordering::SeqCst) { MF_CHECKED } else { MF_UNCHECKED };
    let startup_flag = if startup_enabled() { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(menu, MF_STRING | enabled_flag, ID_ENABLED as usize, w!("Enabled"));
    let _ = AppendMenuW(menu, MF_STRING | startup_flag, ID_STARTUP as usize, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_STRING, ID_UNINSTALL as usize, w!("Uninstall"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT as usize, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    take_foreground(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None).0 as u32;
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    match cmd {
        ID_ENABLED => {
            let now = !ENABLED.load(Ordering::SeqCst);
            ENABLED.store(now, Ordering::SeqCst);
        }
        ID_STARTUP => set_startup(!startup_enabled()),
        ID_UNINSTALL => uninstall(hwnd),
        ID_EXIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::SeqCst);
    match msg {
        WM_SHOW_END_TASK_MENU => {
            show_end_task_menu(hwnd);
            return LRESULT(0);
        }
        WM_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            }
            return LRESULT(0);
        }
        WM_ENTERIDLE if wparam.0 as u32 == MSGF_MENU => {
            // The open menu is idle: keep it at the top of the topmost band so the taskbar's
            // own popups (thumbnail previews, tooltips) can never cover it.
            let _ = SetWindowPos(
                HWND(lparam.0 as *mut c_void),
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            return LRESULT(0);
        }
        WM_COMMAND => return LRESULT(0),
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ if taskbar_created != 0 && msg == taskbar_created => {
            // Explorer restarted: the old tray icon is gone, so add it back.
            add_tray_icon(hwnd);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Single instance: if the named mutex already exists, another copy is running.
        let _mutex = match CreateMutexW(None, true, w!("yeet-single-instance")) {
            Ok(handle) => handle,
            Err(_) => return,
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        let module = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let instance = HINSTANCE(module.0);
        let class_name = w!("yeet-hidden");

        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&class);

        // A normal (not message-only) hidden window, so it receives the TaskbarCreated broadcast.
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("Yeet"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        ) {
            Ok(handle) => handle,
            Err(_) => return,
        };

        MAIN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        TASKBAR_CREATED_MSG.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);
        add_tray_icon(hwnd);

        // The mouse hook gets its own thread and message loop; see mouse_hook.
        thread::spawn(|| {
            let module = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HINSTANCE(module.0), 0) else {
                return;
            };
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}
            let _ = UnhookWindowsHookEx(hook);
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
