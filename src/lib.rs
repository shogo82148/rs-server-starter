//! A Rust implementation of [Server::Starter](https://github.com/kazuho/p5-Server-Starter).
//!
//! `Server::Starter` is a superdaemon that binds listening sockets on behalf of a
//! worker program and hands them down to the worker through the environment. When
//! the superdaemon receives `SIGHUP` it spawns a fresh generation of the worker
//! sharing the very same sockets and, once the new generation is up, retires the
//! old one — achieving hot deploys with zero dropped connections.
//!
//! This crate ships both:
//!
//! * the `start_server` command-line superdaemon (see the `start_server` binary), and
//! * a small client library that a worker uses to recover the inherited sockets.
//!
//! # Worker-side usage
//!
//! ```no_run
//! use std::net::TcpListener;
//! use std::os::unix::io::FromRawFd;
//!
//! // `start_server` exposes the bound sockets through the `SERVER_STARTER_PORT`
//! // environment variable. `server_ports` parses it for you.
//! for (address, fd) in server_starter::server_ports().unwrap() {
//!     println!("listening on {address} via fd {fd}");
//!     // The fd is a ready-to-use listening socket inherited from the parent.
//!     let listener = unsafe { TcpListener::from_raw_fd(fd) };
//!     // ... accept() in a loop ...
//!     let _ = listener;
//! }
//! ```

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::os::unix::io::RawFd;

mod control;
mod options;
mod port;
mod server;
mod signals;

pub use control::{restart_server, stop_server};
pub use options::{Action, Options};
pub use server::start_server;

/// Parses `start_server` command-line arguments (excluding the program name).
///
/// On success returns the resolved [`Options`]; on failure a human-readable error
/// message suitable for printing to the user.
pub fn parse_args<S: Into<String> + Clone>(args: &[S]) -> Result<Options, String> {
    options::parse(args.iter().cloned())
}

/// The `--help` usage text for the `start_server` command.
pub fn usage() -> &'static str {
    options::USAGE
}

/// The environment variable through which listening sockets are passed to workers.
pub const PORT_ENV: &str = "SERVER_STARTER_PORT";

/// The environment variable holding the (monotonically increasing) worker generation.
pub const GENERATION_ENV: &str = "SERVER_STARTER_GENERATION";

/// Errors returned by the worker-side helpers of this crate.
#[derive(Debug)]
pub enum Error {
    /// `SERVER_STARTER_PORT` is not present — the program was not launched under `start_server`.
    NotUnderServerStarter,
    /// An entry in `SERVER_STARTER_PORT` was malformed.
    Malformed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotUnderServerStarter => write!(
                f,
                "{PORT_ENV} is not set; the program is not running under start_server"
            ),
            Error::Malformed(s) => write!(f, "malformed {PORT_ENV} entry: {s:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// Parses `SERVER_STARTER_PORT` and returns a map of *bind address* to *file descriptor*.
///
/// The bind address is exactly the key that `start_server` used when binding the
/// socket, e.g. `"0.0.0.0:8080"`, `"[::1]:8080"` or a unix socket path such as
/// `"/tmp/app.sock"`. The file descriptor refers to an already-`listen()`ing
/// socket inherited from the superdaemon; the worker may `accept()` on it directly.
///
/// The returned map is ordered by address to make iteration deterministic.
///
/// # Errors
///
/// Returns [`Error::NotUnderServerStarter`] if the program was not launched by
/// `start_server`, or [`Error::Malformed`] if an entry cannot be parsed.
pub fn server_ports() -> Result<BTreeMap<String, RawFd>, Error> {
    let raw = env::var(PORT_ENV).map_err(|_| Error::NotUnderServerStarter)?;
    parse_ports(&raw)
}

fn parse_ports(raw: &str) -> Result<BTreeMap<String, RawFd>, Error> {
    let mut map = BTreeMap::new();
    for entry in raw.split(';') {
        if entry.is_empty() {
            continue;
        }
        // A unix socket path never contains '=', but to be safe we split on the
        // *last* '=' so that the file descriptor is always the trailing token.
        let (addr, fd) = entry
            .rsplit_once('=')
            .ok_or_else(|| Error::Malformed(entry.to_string()))?;
        let fd: RawFd = fd
            .parse()
            .map_err(|_| Error::Malformed(entry.to_string()))?;
        map.insert(addr.to_string(), fd);
    }
    Ok(map)
}

/// Returns the current worker generation, as advertised by `start_server` through
/// `SERVER_STARTER_GENERATION`, or `None` when the variable is absent or unparsable.
///
/// The generation starts at 1 and increases by one every time the superdaemon
/// spawns a new worker.
pub fn generation() -> Option<u64> {
    env::var(GENERATION_ENV).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_ports() {
        let map = parse_ports("0.0.0.0:8080=3;[::1]:8080=4;/tmp/app.sock=5").unwrap();
        assert_eq!(map.get("0.0.0.0:8080"), Some(&3));
        assert_eq!(map.get("[::1]:8080"), Some(&4));
        assert_eq!(map.get("/tmp/app.sock"), Some(&5));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn ignores_trailing_semicolon() {
        let map = parse_ports("0.0.0.0:80=3;").unwrap();
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn rejects_malformed() {
        assert!(matches!(
            parse_ports("noequalshere"),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            parse_ports("0.0.0.0:80=notanint"),
            Err(Error::Malformed(_))
        ));
    }
}
