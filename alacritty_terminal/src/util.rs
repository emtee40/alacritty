use std::ffi::OsStr;
use std::io;
use std::process::{Command, Stdio};

#[cfg(not(windows))]
use std::os::unix::process::CommandExt;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use winapi::um::winbase::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

/// Threading utilities.
pub mod thread {
    /// Like `thread::spawn`, but with a `name` argument.
    pub fn spawn_named<F, T, S>(name: S, f: F) -> std::thread::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
        S: Into<String>,
    {
        std::thread::Builder::new().name(name.into()).spawn(f).expect("thread spawn works")
    }

    pub use std::thread::*;
}

#[cfg(not(windows))]
pub fn start_daemon<I, S>(program: &str, args: I) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    unsafe {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .pre_exec(|| {
                match ::libc::fork() {
                    -1 => return Err(io::Error::last_os_error()),
                    0 => (),
                    _ => ::libc::_exit(0),
                }

                if ::libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                Ok(())
            })
            .spawn()?
            .wait()
            .map(|_| ())
    }
}

#[cfg(windows)]
pub fn start_daemon<I, S>(program: &str, args: I) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Setting all the I/O handles to null and setting the
    // CREATE_NEW_PROCESS_GROUP and CREATE_NO_WINDOW has the effect
    // that console applications will run without opening a new
    // console window.
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}
