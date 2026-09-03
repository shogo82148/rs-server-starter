//! Implementation of the `--restart` and `--stop` control commands, which act on
//! an already-running superdaemon identified by its `--pid-file`.

use std::fs;
use std::io;
use std::thread;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::options::Options;

/// Reads the pid stored in `--pid-file`.
fn read_pid(path: &str) -> io::Result<Pid> {
    let contents = fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot read pid file {path:?}: {e}")))?;
    let pid: i32 = contents.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "pid file does not contain a pid",
        )
    })?;
    Ok(Pid::from_raw(pid))
}

/// Returns the maximum generation currently listed in the status file, or 0.
fn max_generation(path: &str) -> u64 {
    let Ok(contents) = fs::read_to_string(path) else {
        return 0;
    };
    contents
        .lines()
        .filter_map(|line| line.split(':').next())
        .filter_map(|g| g.trim().parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

/// Parses the status file into `(generation, pid)` pairs.
fn read_status(path: &str) -> Vec<(u64, i32)> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let (gen, pid) = line.split_once(':')?;
            Some((gen.trim().parse().ok()?, pid.trim().parse().ok()?))
        })
        .collect()
}

/// Sends `SIGHUP` to the running superdaemon and waits until the new worker
/// generation has fully taken over (all older generations have exited).
///
/// Requires both `--pid-file` and `--status-file`.
pub fn restart_server(opts: &Options) -> io::Result<()> {
    let pid_file = opts.pid_file.as_deref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "--restart requires --pid-file")
    })?;
    let status_file = opts.status_file.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "--restart requires --status-file",
        )
    })?;

    let pid = read_pid(pid_file)?;
    let base_generation = max_generation(status_file);

    kill(pid, Signal::SIGHUP)
        .map_err(|e| io::Error::other(format!("failed to signal {pid}: {e}")))?;

    // Wait until a generation newer than the one we observed is the *only* one
    // running, which means the restart has completed and old workers are gone.
    loop {
        thread::sleep(Duration::from_secs(1));

        // Bail out if the superdaemon died instead of restarting.
        if kill(pid, None).is_err() {
            return Err(io::Error::other(
                "the superdaemon exited without completing the restart",
            ));
        }

        let status = read_status(status_file);
        if status.is_empty() {
            continue;
        }
        let newest = status.iter().map(|(g, _)| *g).max().unwrap_or(0);
        let all_current = status.iter().all(|(g, _)| *g == newest);
        if newest > base_generation && all_current {
            return Ok(());
        }
    }
}

/// Sends `SIGTERM` to the running superdaemon. Requires `--pid-file`.
pub fn stop_server(opts: &Options) -> io::Result<()> {
    let pid_file = opts
        .pid_file
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--stop requires --pid-file"))?;
    let pid = read_pid(pid_file)?;

    kill(pid, Signal::SIGTERM)
        .map_err(|e| io::Error::other(format!("failed to signal {pid}: {e}")))?;
    Ok(())
}
