//! The terminal `attach` runs in: raw mode so every key reaches the session as a terminal
//! would send it, UTF-8 both ways, and the window's size. On Windows the console gets VT input
//! (keys arrive as the sequences a Unix terminal sends, Ctrl+C as a byte) and VT output; on
//! macOS and Linux termios goes raw. Either way the modes are put back on the way out.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::*;

    pub struct Restore {
        input: HANDLE,
        in_mode: u32,
        output: HANDLE,
        out_mode: u32,
        cp_in: u32,
        cp_out: u32,
    }

    unsafe impl Send for Restore {}

    pub fn raw() -> std::io::Result<Restore> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let (mut in_mode, mut out_mode) = (0u32, 0u32);
            if GetConsoleMode(input, &mut in_mode) == 0
                || GetConsoleMode(output, &mut out_mode) == 0
            {
                return Err(std::io::Error::other("attach needs a console"));
            }
            let restore = Restore {
                input,
                in_mode,
                output,
                out_mode,
                cp_in: GetConsoleCP(),
                cp_out: GetConsoleOutputCP(),
            };
            SetConsoleMode(
                input,
                (in_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT,
            );
            SetConsoleMode(
                output,
                out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
            );
            SetConsoleCP(65001);
            SetConsoleOutputCP(65001);
            Ok(restore)
        }
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.input, self.in_mode);
                SetConsoleMode(self.output, self.out_mode);
                SetConsoleCP(self.cp_in);
                SetConsoleOutputCP(self.cp_out);
            }
        }
    }

    /// The window's visible size as (cols, rows).
    pub fn size() -> Option<(u16, u16)> {
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
                return None;
            }
            Some((
                (info.srWindow.Right - info.srWindow.Left + 1) as u16,
                (info.srWindow.Bottom - info.srWindow.Top + 1) as u16,
            ))
        }
    }

    pub fn set_title(title: &str) {
        let w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            SetConsoleTitleW(w.as_ptr());
        }
    }
}

#[cfg(unix)]
mod imp {
    pub struct Restore {
        saved: libc::termios,
    }

    pub fn raw() -> std::io::Result<Restore> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(std::io::Error::other("attach needs a terminal"));
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Ok(Restore { saved })
        }
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
            }
        }
    }

    pub fn size() -> Option<(u16, u16)> {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) != 0 || ws.ws_col == 0 {
                return None;
            }
            Some((ws.ws_col, ws.ws_row))
        }
    }

    pub fn set_title(title: &str) {
        use std::io::Write;
        let _ = write!(
            std::io::stdout(),
            "\x1b]0;{}\x07",
            title.replace(['\x07', '\x1b'], "")
        );
        let _ = std::io::stdout().flush();
    }
}

pub use imp::{raw, set_title, size};
