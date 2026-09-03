//! Parsing of `--port`/`--path` specifications and binding of listening sockets.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::{IntoRawFd, RawFd};
use std::path::Path;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// A socket bound by the superdaemon and inherited by workers.
pub struct Listener {
    /// The key advertised in `SERVER_STARTER_PORT`, e.g. `0.0.0.0:8080` or a path.
    pub key: String,
    /// The file descriptor the worker will find the socket at.
    pub fd: RawFd,
}

/// Kind of a parsed port/path specification.
enum Kind {
    Tcp(SocketAddr),
    Udp(SocketAddr),
    Unix(String),
}

struct Spec {
    key: String,
    kind: Kind,
    /// An explicit target file descriptor requested via the `=N` suffix, if any.
    want_fd: Option<RawFd>,
}

/// Binds every `--port` and `--path` specification, in order, returning the
/// resulting listeners. The returned file descriptors have `FD_CLOEXEC` cleared
/// so that they are inherited across `exec()`.
pub fn bind_all(ports: &[String], paths: &[String], backlog: i32) -> io::Result<Vec<Listener>> {
    let mut listeners = Vec::with_capacity(ports.len() + paths.len());
    for p in ports {
        let spec = parse_port(p)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("--port {p}: {e}")))?;
        listeners.push(bind_spec(spec, backlog)?);
    }
    for p in paths {
        let spec = parse_path(p)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("--path {p}: {e}")))?;
        listeners.push(bind_spec(spec, backlog)?);
    }
    Ok(listeners)
}

/// Splits an optional trailing `=<fd>` file-descriptor override off a spec.
fn split_fd(spec: &str) -> Result<(&str, Option<RawFd>), String> {
    match spec.rsplit_once('=') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() => {
            let fd: RawFd = tail.parse().map_err(|_| format!("invalid fd {tail:?}"))?;
            Ok((head, Some(fd)))
        }
        _ => Ok((spec, None)),
    }
}

fn parse_port(raw: &str) -> Result<Spec, String> {
    let (spec, want_fd) = split_fd(raw)?;
    let spec = spec.trim();

    // UDP is written as `u<port>` (a bare UDP port). This is intentionally narrow
    // so that ordinary host names beginning with 'u' are never misread.
    if let Some(rest) = spec.strip_prefix('u') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            let port: u16 = rest
                .parse()
                .map_err(|_| format!("invalid udp port {rest:?}"))?;
            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            return Ok(Spec {
                key: format!("0.0.0.0:{port}"),
                kind: Kind::Udp(addr),
                want_fd,
            });
        }
    }

    // Bare port number -> bind 0.0.0.0.
    if spec.chars().all(|c| c.is_ascii_digit()) && !spec.is_empty() {
        let port: u16 = spec.parse().map_err(|_| format!("invalid port {spec:?}"))?;
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        return Ok(Spec {
            key: format!("0.0.0.0:{port}"),
            kind: Kind::Tcp(addr),
            want_fd,
        });
    }

    // `host:port`, including `[::1]:port` for IPv6. Resolve host names too.
    let addr = resolve(spec)?;
    Ok(Spec {
        key: spec.to_string(),
        kind: Kind::Tcp(addr),
        want_fd,
    })
}

fn parse_path(raw: &str) -> Result<Spec, String> {
    let (path, want_fd) = split_fd(raw)?;
    if path.is_empty() {
        return Err("empty path".into());
    }
    Ok(Spec {
        key: path.to_string(),
        kind: Kind::Unix(path.to_string()),
        want_fd,
    })
}

fn resolve(hostport: &str) -> Result<SocketAddr, String> {
    // `to_socket_addrs` handles `1.2.3.4:80`, `[::1]:80`, and `example.com:80`.
    let mut it = hostport
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {hostport:?}: {e}"))?;
    it.next()
        .ok_or_else(|| format!("no address resolved for {hostport:?}"))
}

fn bind_spec(spec: Spec, backlog: i32) -> io::Result<Listener> {
    let fd = match spec.kind {
        Kind::Tcp(addr) => bind_tcp(addr, backlog)?,
        Kind::Udp(addr) => bind_udp(addr)?,
        Kind::Unix(ref path) => bind_unix(path, backlog)?,
    };

    // Honour an explicit `=<fd>` request by moving the socket there.
    let fd = match spec.want_fd {
        Some(want) if want != fd => {
            dup_to(fd, want)?;
            want
        }
        _ => fd,
    };

    clear_cloexec(fd)?;
    Ok(Listener { key: spec.key, fd })
}

fn bind_tcp(addr: SocketAddr, backlog: i32) -> io::Result<RawFd> {
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if addr.is_ipv6() {
        // Keep IPv6 sockets IPv6-only so that an explicit `[::]` and `0.0.0.0`
        // pair can coexist, matching the behaviour users expect from a bind list.
        socket.set_only_v6(true)?;
    }
    socket.bind(&SockAddr::from(addr))?;
    socket.listen(backlog)?;
    Ok(socket.into_raw_fd())
}

fn bind_udp(addr: SocketAddr) -> io::Result<RawFd> {
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SockAddr::from(addr))?;
    Ok(socket.into_raw_fd())
}

fn bind_unix(path: &str, backlog: i32) -> io::Result<RawFd> {
    // Remove a stale socket file left behind by a previous, crashed run.
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() {
            let _ = std::fs::remove_file(path);
        }
    }
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    let addr = SockAddr::unix(Path::new(path))?;
    socket.bind(&addr)?;
    socket.listen(backlog)?;
    Ok(socket.into_raw_fd())
}

/// Duplicates `src` onto the exact descriptor `dst`, replacing whatever was there.
fn dup_to(src: RawFd, dst: RawFd) -> io::Result<()> {
    // SAFETY: dup2 is a well-defined syscall; the descriptors are valid.
    if unsafe { libc::dup2(src, dst) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // The original descriptor is no longer needed once duplicated.
    unsafe { libc::close(src) };
    Ok(())
}

/// Clears the close-on-exec flag so the descriptor survives `exec()` in workers.
fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = flags & !libc::FD_CLOEXEC;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_port_binds_ipv4_any() {
        let s = parse_port("8080").unwrap();
        assert_eq!(s.key, "0.0.0.0:8080");
        assert!(matches!(s.kind, Kind::Tcp(_)));
        assert!(s.want_fd.is_none());
    }

    #[test]
    fn fd_override_is_split_off() {
        let s = parse_port("8080=7").unwrap();
        assert_eq!(s.key, "0.0.0.0:8080");
        assert_eq!(s.want_fd, Some(7));
    }

    #[test]
    fn ipv6_host_port() {
        let s = parse_port("[::1]:9000").unwrap();
        assert_eq!(s.key, "[::1]:9000");
        assert!(matches!(s.kind, Kind::Tcp(a) if a.is_ipv6()));
    }

    #[test]
    fn udp_bare_port() {
        let s = parse_port("u5353").unwrap();
        assert_eq!(s.key, "0.0.0.0:5353");
        assert!(matches!(s.kind, Kind::Udp(_)));
    }

    #[test]
    fn path_spec() {
        let s = parse_path("/tmp/app.sock=9").unwrap();
        assert_eq!(s.key, "/tmp/app.sock");
        assert_eq!(s.want_fd, Some(9));
        assert!(matches!(s.kind, Kind::Unix(_)));
    }

    #[test]
    fn actually_binds_a_tcp_port() {
        // port 0 => ephemeral; just make sure the whole path works.
        let listeners = bind_all(&["127.0.0.1:0".to_string()], &[], 128).unwrap();
        assert_eq!(listeners.len(), 1);
        assert!(listeners[0].fd >= 0);
        unsafe { libc::close(listeners[0].fd) };
    }
}
