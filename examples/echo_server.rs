//! A tiny HTTP worker that demonstrates the client side of rs-server-starter.
//!
//! Run it under the superdaemon:
//!
//! ```sh
//! start_server --port 8080 -- cargo run --example echo_server
//! ```
//!
//! Then `curl http://localhost:8080/` — each response reports the worker's pid
//! and generation, so a `start_server --restart` visibly rolls to a new one.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::io::FromRawFd;

fn main() {
    let pid = std::process::id();
    let generation = server_starter::generation().unwrap_or(0);

    // Recover the sockets bound by the superdaemon.
    let ports = server_starter::server_ports().unwrap_or_else(|e| {
        eprintln!("echo_server: {e}");
        std::process::exit(1);
    });

    let (addr, fd) = ports
        .into_iter()
        .next()
        .expect("start_server passed no listening sockets");
    println!("echo_server pid={pid} generation={generation} listening on {addr} (fd {fd})");

    // SAFETY: `fd` is an inherited, already-listening TCP socket.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Drain the request line(s); we don't actually parse the request.
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);

        let body = format!("hello from pid {pid}, generation {generation}\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
    }
}
