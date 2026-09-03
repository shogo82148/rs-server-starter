//! The superdaemon: binds sockets and supervises worker generations.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::kill;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{ForkResult, Pid};

use crate::options::Options;
use crate::port::{self, Listener};
use crate::{signals, GENERATION_ENV, PORT_ENV};

/// Runs the superdaemon to completion. Only returns on a fatal setup error; a
/// normal shutdown (SIGTERM/SIGINT) exits the process from within the loop.
pub fn start_server(opts: Options) -> io::Result<()> {
    // Bind every socket first so that binding errors surface on the console
    // before we potentially detach into the background.
    let listeners = port::bind_all(&opts.ports, &opts.paths, opts.effective_backlog())?;
    let port_env = build_port_env(&listeners);

    if opts.daemonize {
        daemonize()?;
    }
    if let Some(log) = &opts.log_file {
        setup_log_file(log)?;
    }

    // Hold onto the locked pid-file for the whole run so a second instance cannot
    // start against the same file.
    let pid_lock = match &opts.pid_file {
        Some(path) => Some(write_pid_file(path)?),
        None => None,
    };

    let signal_pipe = signals::install()?;

    let mut server = Server {
        opts,
        listeners,
        port_env,
        generation: 0,
        current: None,
        old_workers: BTreeMap::new(),
        last_restart: Instant::now(),
        current_died: false,
        signal_pipe,
        _pid_lock: pid_lock,
    };

    server.run()
}

struct Server {
    opts: Options,
    #[allow(dead_code)]
    listeners: Vec<Listener>,
    port_env: String,
    generation: u64,
    current: Option<Pid>,
    /// Retired workers awaiting exit, keyed by pid, valued by their generation.
    old_workers: BTreeMap<Pid, u64>,
    last_restart: Instant,
    /// Set by the reaper when the current worker died and must be respawned.
    current_died: bool,
    signal_pipe: signals::SignalPipe,
    _pid_lock: Option<OwnedFd>,
}

impl Server {
    fn run(&mut self) -> io::Result<()> {
        self.spawn_worker()?;

        loop {
            self.wait();

            if signals::got_term() {
                self.shutdown();
            }

            let mut restart = signals::take_hup();

            if self.current_died {
                self.current_died = false;
                self.spawn_worker()?;
            }

            if !restart && self.opts.enable_auto_restart {
                let elapsed = self.last_restart.elapsed().as_secs();
                let interval = self.opts.auto_restart_interval;
                let no_old = self.old_workers.is_empty();
                if (elapsed >= interval && no_old) || elapsed >= interval.saturating_mul(2) {
                    log("autorestart triggered");
                    restart = true;
                }
            }

            if restart {
                self.restart()?;
            }
        }
    }

    /// Blocks until a signal arrives or a child changes state, then drains the
    /// wake-up pipe and reaps any dead children.
    fn wait(&mut self) {
        // When auto-restart is on we must wake at least once a second to check the
        // elapsed time; otherwise we can sleep until something actually happens.
        let timeout_ms: libc::c_int = if self.opts.enable_auto_restart {
            1000
        } else {
            -1
        };

        let mut pfd = libc::pollfd {
            fd: self.signal_pipe.read_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a single valid pollfd; EINTR simply returns early, which is fine.
        unsafe {
            libc::poll(&mut pfd, 1, timeout_ms);
        }

        drain_pipe(self.signal_pipe.read_fd());
        self.reap();
    }

    /// Reaps every child that has changed state without blocking.
    fn reap(&mut self) {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(status) => {
                    if let Some(pid) = status.pid() {
                        self.on_exit(pid, status);
                    } else {
                        break;
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                // ECHILD (no children) or any other error: nothing more to reap.
                Err(_) => break,
            }
        }
    }

    fn on_exit(&mut self, pid: Pid, status: WaitStatus) {
        if self.current == Some(pid) {
            log(&format!(
                "worker {pid} died unexpectedly with {}, restarting",
                describe_status(status)
            ));
            self.current = None;
            self.current_died = true;
        } else if self.old_workers.remove(&pid).is_some() {
            log(&format!(
                "old worker {pid} is now dead ({})",
                describe_status(status)
            ));
            self.update_status();
        }
        // Unknown pids (e.g. a log-pipe helper) are ignored on purpose.
    }

    /// Spawns a fresh worker, retrying until one stays up for `--interval` seconds.
    fn spawn_worker(&mut self) -> io::Result<Pid> {
        loop {
            self.generation += 1;

            let mut cmd = Command::new(&self.opts.command[0]);
            cmd.args(&self.opts.command[1..]);
            cmd.env(PORT_ENV, &self.port_env);
            cmd.env(GENERATION_ENV, self.generation.to_string());
            if let Some(dir) = &self.opts.dir {
                cmd.current_dir(dir);
            }
            for (k, v) in self.load_envdir() {
                cmd.env(k, v);
            }
            // Guard against a child inheriting the wake-up pipe's write end via any
            // library that dups fds; our own fds are already CLOEXEC, but be explicit.
            unsafe {
                cmd.pre_exec(|| {
                    // Reset SIGPIPE to its default in the worker (we ignore it).
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                    Ok(())
                });
            }

            match cmd.spawn() {
                Ok(child) => {
                    let pid = Pid::from_raw(child.id() as i32);
                    // We reap workers ourselves via waitpid; keep std out of it.
                    std::mem::forget(child);

                    if self.worker_survived(pid) {
                        self.current = Some(pid);
                        self.last_restart = Instant::now();
                        log(&format!(
                            "started new worker {pid} (generation {})",
                            self.generation
                        ));
                        self.update_status();
                        return Ok(pid);
                    }
                    log(&format!(
                        "new worker {pid} seems to have failed to start; restarting"
                    ));
                }
                Err(e) => {
                    log(&format!(
                        "failed to exec {:?}: {e}; retrying in {}s",
                        self.opts.command[0], self.opts.interval
                    ));
                    thread::sleep(Duration::from_secs(self.opts.interval.max(1)));
                }
            }
        }
    }

    /// Waits `--interval` seconds and reports whether the worker is still alive.
    fn worker_survived(&mut self, pid: Pid) -> bool {
        if self.opts.interval > 0 {
            thread::sleep(Duration::from_secs(self.opts.interval));
        }
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(status) => {
                log(&format!(
                    "new worker {pid} exited immediately with {}",
                    describe_status(status)
                ));
                false
            }
            // If it already vanished, treat it as failed to start.
            Err(_) => false,
        }
    }

    /// Graceful restart: retire the current worker, start a new one, then signal
    /// the old worker(s) after the configured delay.
    fn restart(&mut self) -> io::Result<()> {
        log("received HUP, spawning a new worker");
        if let Some(cur) = self.current.take() {
            self.old_workers.insert(cur, self.generation);
        }
        self.spawn_worker()?;

        let delay = self.opts.effective_kill_old_delay();
        if delay > 0 {
            thread::sleep(Duration::from_secs(delay));
        }

        let sig = self.opts.signal_on_hup;
        let victims: Vec<Pid> = self.old_workers.keys().copied().collect();
        if !victims.is_empty() {
            log(&format!("sending {sig} to old workers {victims:?}"));
            for pid in victims {
                let _ = kill(pid, sig);
            }
        }
        Ok(())
    }

    /// Terminates all workers and exits the process.
    fn shutdown(&mut self) -> ! {
        log("received TERM, shutting down");
        if let Some(cur) = self.current.take() {
            self.old_workers.insert(cur, self.generation);
        }
        let sig = self.opts.signal_on_term;
        for pid in self.old_workers.keys() {
            let _ = kill(*pid, sig);
        }

        // Wait for every worker to exit before leaving.
        while !self.old_workers.is_empty() {
            match waitpid(Pid::from_raw(-1), None) {
                Ok(status) => {
                    if let Some(pid) = status.pid() {
                        self.old_workers.remove(&pid);
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
            }
        }

        self.update_status();
        if let Some(path) = &self.opts.pid_file {
            let _ = fs::remove_file(path);
        }
        log("exiting");
        std::process::exit(0);
    }

    /// Writes the `--status-file`, if configured, listing each live generation.
    fn update_status(&self) {
        let Some(path) = &self.opts.status_file else {
            return;
        };

        let mut entries: Vec<(u64, Pid)> = Vec::new();
        if let Some(cur) = self.current {
            entries.push((self.generation, cur));
        }
        for (pid, gen) in &self.old_workers {
            entries.push((*gen, *pid));
        }
        entries.sort();

        let mut body = String::new();
        for (gen, pid) in entries {
            body.push_str(&format!("{gen}:{pid}\n"));
        }

        // Write atomically via a temp file next to the target, then rename.
        let tmp = format!("{path}.{}", std::process::id());
        if let Ok(mut f) = fs::File::create(&tmp) {
            if f.write_all(body.as_bytes()).is_ok() && f.sync_all().is_ok() {
                let _ = fs::rename(&tmp, path);
            } else {
                let _ = fs::remove_file(&tmp);
            }
        }
    }

    /// Loads environment variables from `--envdir`, if set: one variable per file,
    /// named after the file, valued by the file's first line.
    fn load_envdir(&self) -> Vec<(String, String)> {
        let Some(dir) = &self.opts.envdir else {
            return Vec::new();
        };
        let mut vars = Vec::new();
        let Ok(read) = fs::read_dir(dir) else {
            return vars;
        };
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            if let Ok(contents) = fs::read_to_string(entry.path()) {
                let value = contents.lines().next().unwrap_or("").to_string();
                vars.push((name.to_string(), value));
            }
        }
        vars
    }
}

/// Builds the `SERVER_STARTER_PORT` value from the bound listeners.
fn build_port_env(listeners: &[Listener]) -> String {
    listeners
        .iter()
        .map(|l| format!("{}={}", l.key, l.fd))
        .collect::<Vec<_>>()
        .join(";")
}

/// Detaches the process from the controlling terminal via the double-fork dance.
fn daemonize() -> io::Result<()> {
    // SAFETY: the process is single-threaded at this point, so fork is safe.
    match unsafe { nix::unistd::fork() }.map_err(io::Error::from)? {
        ForkResult::Parent { .. } => std::process::exit(0),
        ForkResult::Child => {}
    }
    nix::unistd::setsid().map_err(io::Error::from)?;
    match unsafe { nix::unistd::fork() }.map_err(io::Error::from)? {
        ForkResult::Parent { .. } => std::process::exit(0),
        ForkResult::Child => {}
    }

    // Point stdin at /dev/null. stdout/stderr are redirected to /dev/null too,
    // unless --log-file overrides them afterwards.
    if let Ok(devnull) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let fd = devnull.as_raw_fd();
        unsafe {
            libc::dup2(fd, libc::STDIN_FILENO);
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
        }
    }
    Ok(())
}

/// Redirects stdout and stderr to `spec`. A leading `|` pipes them to a command.
fn setup_log_file(spec: &str) -> io::Result<()> {
    let target_fd: RawFd = if let Some(cmd) = spec.strip_prefix('|') {
        // Spawn `sh -c <cmd>` reading the logs on its stdin.
        let (read_fd, write_fd) = make_pipe()?;
        let read = unsafe { OwnedFd::from_raw_fd(read_fd) };
        Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::from(read))
            .spawn()?;
        write_fd
    } else {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(spec)?;
        file.into_raw_fd()
    };

    unsafe {
        libc::dup2(target_fd, libc::STDOUT_FILENO);
        libc::dup2(target_fd, libc::STDERR_FILENO);
        if target_fd != libc::STDOUT_FILENO && target_fd != libc::STDERR_FILENO {
            libc::close(target_fd);
        }
    }
    Ok(())
}

fn make_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// Opens and exclusively locks the pid file, writing our pid into it. The
/// returned descriptor must be kept alive to hold the lock.
fn write_pid_file(path: &str) -> io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o644)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;

    // Fail fast if another superdaemon already holds the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("another start_server already holds {path:?}"),
        ));
    }

    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file.into())
}

use std::os::unix::io::FromRawFd;

/// Drains all pending bytes from the wake-up pipe.
fn drain_pipe(fd: RawFd) {
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

fn describe_status(status: WaitStatus) -> String {
    match status {
        WaitStatus::Exited(_, code) => format!("exit status {code}"),
        WaitStatus::Signaled(_, sig, _) => format!("signal {sig}"),
        other => format!("{other:?}"),
    }
}

/// Emits a diagnostic line in the style of Server::Starter.
fn log(msg: &str) {
    eprintln!("start_server (pid:{}): {}", std::process::id(), msg);
}
