//! Small Win32 pieces around the windows: the PATH broadcast, the fatal message box, the exe's
//! icon on a window, finding the setup window, and Windows sign-out.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, FindWindowW, GetWindowThreadProcessId, LoadImageW, MessageBoxW, SendMessageTimeoutW, SendMessageW,
    SetWindowLongPtrW, GWLP_WNDPROC, HWND_BROADCAST, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, MB_ICONERROR,
    MB_OK, SMTO_ABORTIFHUNG, SM_CXICON, SM_CXSMICON, WM_ENDSESSION, WM_SETICON, WM_SETTINGCHANGE, WNDPROC,
};

/// Tells running programs (Explorer first) that the user environment changed, so a new
/// Explorer address bar or terminal sees the new PATH (UserPath.Broadcast).
pub fn broadcast_environment() {
    let what = HSTRING::from("Environment");
    // SAFETY: the string outlives the call; SMTO_ABORTIFHUNG with a 2 s cap never hangs us.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(what.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        )
    };
}

/// A plain error box, for when there is no window to say it in.
pub fn error_box(title: &str, text: &str) {
    // SAFETY: valid wide strings for the call; no owner window.
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR) };
}

/// Puts the exe's own icon (resource 1, from build.rs) on a window, big and small, at the
/// window's DPI.
pub fn set_window_icon(hwnd: isize) {
    let hwnd = HWND(hwnd as *mut _);
    // SAFETY: the null module name is this exe; the handle is not freed.
    let Ok(module) = (unsafe { GetModuleHandleW(PCWSTR::null()) }) else { return };
    // SAFETY: a window handle of ours.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
        // SAFETY: no pointers.
        let size = unsafe { GetSystemMetricsForDpi(metric, dpi) };
        // SAFETY: MAKEINTRESOURCE(1) as the name; an icon handle shared for the process life.
        let icon = unsafe {
            LoadImageW(Some(module.into()), PCWSTR(std::ptr::without_provenance(1)) /* MAKEINTRESOURCE(1) */, IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
        };
        if let Ok(icon) = icon {
            // SAFETY: a window of ours and a valid icon handle.
            unsafe { SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize))) };
        }
    }
}

/// Opens a web link or a file with its default program (the browser for both here), as
/// Process.Start with UseShellExecute did. The caller checks what it opens.
pub fn open(target: &str) {
    let _ = try_open(target);
}

/// [`open`], false when Windows found nothing to open it with (or it failed).
fn try_open(target: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // SAFETY: valid wide strings that outlive the call; no owner window.
    let r = unsafe { ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecute's documented success: a value above 32.
    r.0 as isize > 32
}

/// An agent's own file in whatever opens it; a `.rules` file often has no app, so then
/// Explorer shows it selected (SettingsPane.OpenFile).
pub fn open_file_or_select(path: &std::path::Path) {
    if !try_open(&path.to_string_lossy()) {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }
}

/// Our own top-level window with this title, if there is one.
pub fn find_own_window(title: &str) -> Option<isize> {
    // SAFETY: valid wide string; no class filter.
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), &HSTRING::from(title)) }.ok()?;
    let mut pid = 0u32;
    // SAFETY: a window handle (any process) and a live out-parameter.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid == std::process::id()).then_some(hwnd.0 as isize)
}

static OLD_PROC: AtomicIsize = AtomicIsize::new(0);
static ON_END: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Windows sign-out or shutdown (WM_ENDSESSION with the session really ending) runs `end`,
/// which quits for real, rather than meeting a close that only hides. Once, for the main window,
/// on the thread that owns it.
pub fn on_session_end(hwnd: isize, end: impl Fn() + Send + Sync + 'static) {
    if ON_END.set(Box::new(end)).is_err() {
        return;
    }
    let hwnd = HWND(hwnd as *mut _);
    // SAFETY: called on the window's own thread; the old procedure is kept and every message
    // is passed on to it.
    let old = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, session_proc as *const () as isize) };
    OLD_PROC.store(old, Ordering::SeqCst);
}

extern "system" fn session_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_ENDSESSION && wparam.0 != 0 {
        if let Some(end) = ON_END.get() {
            end();
        }
    }
    let old = OLD_PROC.load(Ordering::SeqCst);
    // SAFETY: `old` is the window procedure SetWindowLongPtrW returned (never 0 for a window
    // that exists), so it has the WNDPROC signature.
    let old: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(old) };
    // SAFETY: passes the message on unchanged to the procedure it was meant for.
    unsafe { CallWindowProcW(old, hwnd, msg, wparam, lparam) }
}

/// A window of ours by handle, as a dialog's owner (rfd's `set_parent`).
pub struct Owner(pub isize);

impl raw_window_handle::HasWindowHandle for Owner {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let h = std::num::NonZeroIsize::new(self.0).ok_or(raw_window_handle::HandleError::Unavailable)?;
        let raw = raw_window_handle::RawWindowHandle::Win32(raw_window_handle::Win32WindowHandle::new(h));
        // SAFETY: the handle is a live window of ours for as long as the dialog it owns is up
        // (the dialog is modal and the window only closes from our own UI thread).
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

impl raw_window_handle::HasDisplayHandle for Owner {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::windows())
    }
}
