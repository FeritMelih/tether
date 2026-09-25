//! The pseudo-terminal behind a session, behind a trait: the operating system's through
//! `portable-pty` (ConPTY on Windows, openpty elsewhere), or an in-memory one for tests. A
//! session reads, writes and waits on blocking handles from threads of its own, and resizes,
//! signals and closes through `Control`.

use std::io::{self, Read, Write};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: i64,
    pub signal: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Int,
    Term,
    Hup,
    Kill,
}

impl Signal {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().trim_start_matches("SIG") {
            "INT" => Some(Self::Int),
            "TERM" => Some(Self::Term),
            "HUP" => Some(Self::Hup),
            "KILL" => Some(Self::Kill),
            _ => None,
        }
    }
}

pub trait Control: Send {
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>;
    /// Delivers a signal. On Windows only `Int` has a meaning of its own, and the session
    /// types it; everything else ends the process and every process it started.
    fn signal(&mut self, sig: Signal) -> io::Result<()>;
    /// Lets go of the terminal once the program has exited, which is what ends the output on
    /// Windows: ConPTY holds its end open until the pseudo-console is closed.
    fn close(&mut self);
}

pub trait Wait: Send {
    fn wait(self: Box<Self>) -> Exit;
}

pub struct Opened {
    pub pid: Option<u32>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub control: Box<dyn Control>,
    pub child: Box<dyn Wait>,
}

pub trait PtySystem: Send + Sync {
    /// Starts `argv` (its program already resolved) in a new terminal of the size given, with
    /// exactly the environment given.
    fn open(
        &self,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> io::Result<Opened>;
}

/// The operating system's pseudo-terminals.
pub struct NativePty;

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl PtySystem for NativePty {
    fn open(
        &self,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> io::Result<Opened> {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other)?;
        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(other)?;
        drop(pair.slave);
        let pid = child.process_id();
        #[cfg(windows)]
        let job = child
            .as_raw_handle()
            .and_then(|h| job::Job::holding(h as _));
        let reader = pair.master.try_clone_reader().map_err(other)?;
        let writer = pair.master.take_writer().map_err(other)?;
        let killer = child.clone_killer();
        Ok(Opened {
            pid,
            reader,
            writer,
            control: Box::new(NativeControl {
                master: Some(pair.master),
                killer,
                pid,
                #[cfg(windows)]
                job,
                #[cfg(unix)]
                foreground: None,
            }),
            child: Box::new(NativeChild(child)),
        })
    }
}

struct NativeControl {
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    #[cfg_attr(unix, allow(dead_code))]
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    #[cfg_attr(windows, allow(dead_code))]
    pid: Option<u32>,
    /// The job the program and what it starts run in; none where the job could not be made.
    #[cfg(windows)]
    job: Option<job::Job>,
    /// The foreground job's process group, when a shell runs one: it is ended with the
    /// program's own group.
    #[cfg(unix)]
    foreground: Option<libc::pid_t>,
}

/// A job object holding a session's program, so that ending the session ends everything the
/// program started, however it was started, not only what shares its console. A process that
/// asks to leave the job (a daemon) may. A process the program starts in the moment before it
/// is put in the job is not in it.
#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    };

    pub struct Job(HANDLE);

    // A job handle may be used and closed from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn holding(process: HANDLE) -> Option<Job> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                let limited = SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0;
                (limited && AssignProcessToJobObject(job.0, process) != 0).then_some(job)
            }
        }

        /// Ends every process in the job; whether it could.
        pub fn terminate(&self) -> bool {
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

impl Control for NativeControl {
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let Some(m) = &self.master else { return Ok(()) };
        m.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(other)
    }

    #[cfg(unix)]
    fn signal(&mut self, sig: Signal) -> io::Result<()> {
        let Some(pid) = self.pid else {
            return Err(other("the process id is not known"));
        };
        let n = match sig {
            Signal::Int => libc::SIGINT,
            Signal::Term => libc::SIGTERM,
            Signal::Hup => libc::SIGHUP,
            Signal::Kill => libc::SIGKILL,
        };
        // A program a shell runs in the foreground leads a group of its own, which hears it
        // too; remembered for the kill that follows a hangup, when the terminal may be gone.
        if let Some(g) = self.master.as_ref().and_then(|m| m.process_group_leader()) {
            if g > 0 && g != pid as libc::pid_t {
                self.foreground = Some(g);
            }
        }
        if let Some(g) = self.foreground {
            unsafe {
                libc::killpg(g, n);
            }
        }
        // The program leads its own session and process group, so the whole group hears it.
        if unsafe { libc::killpg(pid as libc::pid_t, n) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(e);
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    fn signal(&mut self, _sig: Signal) -> io::Result<()> {
        if self.job.as_ref().is_some_and(|j| j.terminate()) {
            return Ok(());
        }
        // With no job, the program alone. portable-pty 0.9.0 reports a successful
        // TerminateProcess as an error; what matters is that the process ends, which the
        // waiter sees.
        let _ = self.killer.kill();
        Ok(())
    }

    fn close(&mut self) {
        self.master.take();
    }
}

struct NativeChild(Box<dyn portable_pty::Child + Send + Sync>);

impl Wait for NativeChild {
    fn wait(mut self: Box<Self>) -> Exit {
        match self.0.wait() {
            Ok(status) => Exit {
                code: i64::from(status.exit_code()),
                signal: status.signal().map(String::from),
            },
            Err(e) => Exit {
                code: -1,
                signal: Some(e.to_string()),
            },
        }
    }
}

/// An in-memory terminal for tests: the test plays the program, writing its output, reading
/// what was typed, and choosing when it exits.
pub mod fake {
    use super::*;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct FakePty {
        inner: Arc<Mutex<Option<Program>>>,
    }

    /// The program's side of the most recently opened fake terminal.
    pub struct Program {
        pub argv: Vec<String>,
        pub env: Vec<(String, String)>,
        output: mpsc::Sender<Vec<u8>>,
        pub input: mpsc::Receiver<Vec<u8>>,
        pub sizes: mpsc::Receiver<(u16, u16)>,
        pub signals: mpsc::Receiver<Signal>,
        exit: mpsc::Sender<Exit>,
    }

    impl Program {
        pub fn write(&self, bytes: &[u8]) {
            let _ = self.output.send(bytes.to_vec());
        }

        pub fn exit(&self, code: i64) {
            let _ = self.exit.send(Exit { code, signal: None });
        }

        /// Everything typed so far, waiting up to `ms` for the first of it.
        pub fn typed(&self, ms: u64) -> Vec<u8> {
            let mut out = Vec::new();
            if let Ok(b) = self
                .input
                .recv_timeout(std::time::Duration::from_millis(ms))
            {
                out.extend(b);
            }
            while let Ok(b) = self.input.try_recv() {
                out.extend(b);
            }
            out
        }
    }

    impl FakePty {
        pub fn take(&self) -> Option<Program> {
            self.inner.lock().unwrap().take()
        }
    }

    struct Reader {
        rx: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    impl Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pending.is_empty() {
                match self.rx.recv() {
                    Ok(b) => self.pending = b,
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }

    struct Writer(mpsc::Sender<Vec<u8>>);

    impl Write for Writer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .send(buf.to_vec())
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeControl {
        sizes: mpsc::Sender<(u16, u16)>,
        signals: mpsc::Sender<Signal>,
        /// Dropped on close, which ends the reader once the program's side is gone too.
        output: Option<mpsc::Sender<Vec<u8>>>,
    }

    impl Control for FakeControl {
        fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
            let _ = self.sizes.send((cols, rows));
            Ok(())
        }
        fn signal(&mut self, sig: Signal) -> io::Result<()> {
            let _ = self.signals.send(sig);
            Ok(())
        }
        fn close(&mut self) {
            self.output.take();
        }
    }

    struct FakeChild(mpsc::Receiver<Exit>);

    impl Wait for FakeChild {
        fn wait(self: Box<Self>) -> Exit {
            self.0.recv().unwrap_or(Exit {
                code: -1,
                signal: Some("gone".into()),
            })
        }
    }

    impl PtySystem for FakePty {
        fn open(
            &self,
            argv: &[String],
            _cwd: Option<&Path>,
            env: &[(String, String)],
            _cols: u16,
            _rows: u16,
        ) -> io::Result<Opened> {
            let (out_tx, out_rx) = mpsc::channel();
            let (in_tx, in_rx) = mpsc::channel();
            let (size_tx, size_rx) = mpsc::channel();
            let (sig_tx, sig_rx) = mpsc::channel();
            let (exit_tx, exit_rx) = mpsc::channel();
            *self.inner.lock().unwrap() = Some(Program {
                argv: argv.to_vec(),
                env: env.to_vec(),
                output: out_tx.clone(),
                input: in_rx,
                sizes: size_rx,
                signals: sig_rx,
                exit: exit_tx,
            });
            Ok(Opened {
                pid: Some(4242),
                reader: Box::new(Reader {
                    rx: out_rx,
                    pending: Vec::new(),
                }),
                writer: Box::new(Writer(in_tx)),
                control: Box::new(FakeControl {
                    sizes: size_tx,
                    signals: sig_tx,
                    output: Some(out_tx),
                }),
                child: Box::new(FakeChild(exit_rx)),
            })
        }
    }
}
