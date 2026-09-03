//! Signal handling for the superdaemon, built on the classic self-pipe trick.
//!
//! Signal handlers are extremely restricted in what they may safely do, so ours
//! do the bare minimum: flip an atomic flag and write a single byte to a pipe.
//! The main loop blocks in `poll()` on the read end of that pipe, so any signal
//! wakes it promptly, after which it inspects the flags and reaps children.

use std::io;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};

static PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Set when `SIGHUP` has been received since the flag was last cleared.
pub static GOT_HUP: AtomicBool = AtomicBool::new(false);
/// Set when `SIGTERM` or `SIGINT` has been received.
pub static GOT_TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(sig: libc::c_int) {
    match sig {
        libc::SIGHUP => GOT_HUP.store(true, Ordering::SeqCst),
        libc::SIGTERM | libc::SIGINT => GOT_TERM.store(true, Ordering::SeqCst),
        _ => {}
    }
    // Wake the main loop. `write` is async-signal-safe; a full pipe (EAGAIN) is
    // fine because a pending byte already guarantees a wake-up.
    let fd = PIPE_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [0u8; 1];
        unsafe {
            libc::write(fd, byte.as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// The read/write ends of the self-pipe, kept alive for the process lifetime.
pub struct SignalPipe {
    pub read: OwnedFd,
    _write: OwnedFd,
}

impl SignalPipe {
    pub fn read_fd(&self) -> RawFd {
        use std::os::unix::io::AsRawFd;
        self.read.as_raw_fd()
    }
}

/// Installs handlers for the signals the superdaemon cares about and returns the
/// self-pipe whose read end the main loop should poll.
pub fn install() -> io::Result<SignalPipe> {
    // O_CLOEXEC so the pipe is not leaked into workers; O_NONBLOCK so draining
    // the read end and writing from the handler never block. `pipe2` is not
    // portable (absent on macOS), so create a plain pipe and set the flags.
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        set_flag(fd, libc::F_SETFD, libc::FD_CLOEXEC)?;
        set_status_flag(fd, libc::O_NONBLOCK)?;
    }
    // SAFETY: the fds were just created by pipe() and are owned by us.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    use std::os::unix::io::AsRawFd;
    PIPE_WRITE_FD.store(write.as_raw_fd(), Ordering::SeqCst);

    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for sig in [
        Signal::SIGHUP,
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGCHLD,
        Signal::SIGALRM,
    ] {
        // SAFETY: `handler` is async-signal-safe (atomic store + write()).
        unsafe { signal::sigaction(sig, &action) }.map_err(io::Error::from)?;
    }

    // Writing to a pipe whose worker end closed must not kill the superdaemon.
    let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe { signal::sigaction(Signal::SIGPIPE, &ignore) }.map_err(io::Error::from)?;

    Ok(SignalPipe {
        read,
        _write: write,
    })
}

/// Sets a file-descriptor flag (F_SETFD) such as `FD_CLOEXEC`.
fn set_flag(fd: RawFd, which: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    let cur = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if cur < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, which, cur | flag) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets a file-status flag (F_SETFL) such as `O_NONBLOCK`.
fn set_status_flag(fd: RawFd, flag: libc::c_int) -> io::Result<()> {
    let cur = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if cur < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, cur | flag) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Atomically reads and clears the `SIGHUP` flag.
pub fn take_hup() -> bool {
    GOT_HUP.swap(false, Ordering::SeqCst)
}

/// Reads (without clearing) whether a terminating signal has been received.
pub fn got_term() -> bool {
    GOT_TERM.load(Ordering::SeqCst)
}
