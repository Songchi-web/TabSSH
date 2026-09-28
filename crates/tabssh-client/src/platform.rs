//! A little Windows console setup, so the same binary behaves in a plain
//! `cmd.exe` window and in Windows Terminal.
//!
//! Two things have to be right before the interface is drawn: the code page has
//! to be UTF-8 (a classic console starts in the machine's OEM code page — often
//! 936 on a Chinese system — which turns the UI's UTF-8 into mojibake), and the
//! console has to understand ANSI escape sequences.  Windows Terminal does both
//! on its own; a classic `conhost` window has to be asked.
//!
//! When the console cannot be taught to draw, that is reported rather than
//! guessed at: [`prepare_console`] returns what it found, and the caller refuses
//! to start the UI with a clear message instead of painting garbage.

use std::sync::atomic::{AtomicU8, Ordering};

/// What the console we are drawing into turned out to be able to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsoleKind {
    /// A modern terminal (Windows Terminal, or any ConPTY host) that speaks VT
    /// itself.  Nothing had to be turned on.
    Modern,
    /// A classic `conhost` window (plain `cmd.exe`) that we turned ANSI support
    /// on for.  This is the common case on Windows 10/11.
    ClassicVt,
    /// A console with no ANSI support at all — the interface cannot be drawn.
    NoVt,
}

impl ConsoleKind {
    fn tag(self) -> u8 {
        match self {
            ConsoleKind::Modern => 1,
            ConsoleKind::ClassicVt => 2,
            ConsoleKind::NoVt => 3,
        }
    }

    fn from_tag(t: u8) -> Option<ConsoleKind> {
        match t {
            1 => Some(ConsoleKind::Modern),
            2 => Some(ConsoleKind::ClassicVt),
            3 => Some(ConsoleKind::NoVt),
            _ => None,
        }
    }
}

/// The console decided by [`prepare_console`]; 0 means "not decided yet".
static KIND: AtomicU8 = AtomicU8::new(0);

/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING`.  Spelled out rather than imported, so
/// the build does not depend on how the SDK headers happen to be bound.
const ENABLE_VT: u32 = 0x0004;

/// Makes the console speak UTF-8 and ANSI, and reports how far it got.
pub fn prepare_console() -> ConsoleKind {
    unsafe {
        use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
        // 65001 is UTF-8.
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
    let kind = detect();
    KIND.store(kind.tag(), Ordering::Relaxed);
    kind
}

/// What the console turned out to be, once [`prepare_console`] has run.
pub fn console_kind() -> Option<ConsoleKind> {
    ConsoleKind::from_tag(KIND.load(Ordering::Relaxed))
}

fn detect() -> ConsoleKind {
    unsafe {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, STD_OUTPUT_HANDLE,
        };
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() {
            return ConsoleKind::NoVt;
        }
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            // Not a console at all — output went to a pipe or a file.  This is
            // checked before anything else, so a stray `WT_SESSION` inherited
            // from the shell cannot make a redirected run look drawable.
            return ConsoleKind::NoVt;
        }
        // A real console: make sure it will interpret ANSI escape sequences.
        if mode & ENABLE_VT == 0 && SetConsoleMode(handle, mode | ENABLE_VT) == 0 {
            return ConsoleKind::NoVt;
        }
        // A ConPTY host (Windows Terminal) announces itself and draws VT
        // natively; a plain `conhost` window is the classic console we just
        // switched ANSI on for.
        if std::env::var_os("WT_SESSION").is_some() || std::env::var_os("TERM_PROGRAM").is_some() {
            ConsoleKind::Modern
        } else {
            ConsoleKind::ClassicVt
        }
    }
}

/// Puts `text` on the Windows clipboard.
///
/// Written through the clipboard API as `CF_UNICODETEXT`, so non-ASCII text
/// survives regardless of the console's code page (spawning `clip.exe` would
/// reinterpret the bytes in whatever the console happens to be in).
pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| {
            if EmptyClipboard() == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let handle = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                GlobalFree(handle);
                return Err(std::io::Error::last_os_error());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            GlobalUnlock(handle);
            // On success the clipboard owns the handle; on failure we must
            // free it ourselves.
            if SetClipboardData(CF_UNICODETEXT, handle).is_null() {
                GlobalFree(handle);
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

/// Reads the text on the Windows clipboard, if it holds any.
///
/// Right-click paste reads straight from the clipboard API rather than spawning
/// a shell — a right-click should feel instant.
pub fn clipboard_text() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let found = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return None;
            }
            let size = GlobalSize(handle);
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                return None;
            }
            // The text is NUL-terminated wide chars; the allocation may be
            // larger, so stop at the first NUL rather than trusting the size.
            let base = ptr as *const u16;
            let mut len = 0;
            while len * 2 < size && *base.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(base, len));
            GlobalUnlock(handle);
            Some(text)
        })();
        CloseClipboard();
        found
    }
}

#[cfg(test)]
mod tests {
    // Manual smoke: writes then reads the real clipboard.  Run once with
    // `cargo test -p tabssh-client clipboard_roundtrip -- --ignored`.
    #[test]
    #[ignore]
    fn clipboard_roundtrip() {
        let text = "tabssh 剪贴板 roundtrip — 你好";
        super::copy_to_clipboard(text).unwrap();
        assert_eq!(super::clipboard_text().as_deref(), Some(text));
    }
}
