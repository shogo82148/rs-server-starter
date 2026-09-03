# rs-server-starter

A Rust implementation of [Server::Starter](https://github.com/kazuho/p5-Server-Starter)
(`start_server`), the superdaemon for hot-deploying server programs written by
Kazuho Oku.

`start_server` binds the listening sockets **once**, on behalf of your server,
and hands them down to a worker process through an environment variable. When you
ask it to restart, it spawns a fresh generation of the worker sharing the very
same sockets, waits for it to come up, and only then retires the old one — so a
deploy never drops a connection nor rejects a request with "connection refused".

This crate provides two things:

- the **`start_server`** command-line superdaemon, and
- a small **client library** (`server_starter`) that a Rust worker uses to
  recover the inherited sockets.

It is protocol-compatible with the original Perl implementation: the
`SERVER_STARTER_PORT` / `SERVER_STARTER_GENERATION` environment variables, the
status file, the pid file, and the signals all behave the same way, so workers
and tooling written for either side interoperate.

## Installation

```sh
cargo install rs-server-starter
```

This installs the `start_server` binary.

## Quick start

Launch any server program under `start_server`, telling it which ports to bind:

```sh
start_server --port 8080 -- your-server --your-flags
```

`start_server` binds `0.0.0.0:8080`, sets `SERVER_STARTER_PORT=0.0.0.0:8080=3`
in the child's environment, and execs `your-server`. Your server accepts on the
inherited file descriptor rather than binding the port itself.

To hot-deploy a new build, keep a pid file and a status file and use `--restart`:

```sh
# Start (usually from an init script / systemd unit):
start_server \
    --port 8080 \
    --pid-file /var/run/app.pid \
    --status-file /var/run/app.status \
    -- your-server

# Later, after deploying a new binary, gracefully roll to it:
start_server --restart --pid-file /var/run/app.pid --status-file /var/run/app.status

# Shut everything down:
start_server --stop --pid-file /var/run/app.pid
```

`--restart` sends `SIGHUP` and blocks until the new generation has fully taken
over; `--stop` sends `SIGTERM` and lets the superdaemon shut its workers down.

## Writing a worker

A worker recovers the sockets with `server_starter::server_ports()`, which parses
`SERVER_STARTER_PORT` into a map of *bind address → file descriptor*:

```rust
use std::net::TcpListener;
use std::os::unix::io::FromRawFd;

fn main() {
    for (addr, fd) in server_starter::server_ports().unwrap() {
        // `fd` is an already-listen()ing socket inherited from start_server.
        let listener = unsafe { TcpListener::from_raw_fd(fd) };
        println!("serving {addr} on fd {fd}");
        for conn in listener.incoming() {
            // handle conn...
            let _ = conn;
        }
    }
}
```

`server_starter::generation()` returns the current worker generation (starting at
1 and incremented on every restart), which is handy for logging deploys.

A runnable example lives in [`examples/echo_server.rs`](examples/echo_server.rs):

```sh
start_server --port 8080 -- cargo run --release --example echo_server
curl http://localhost:8080/     # reports the worker's pid and generation
```

Any language works, of course — a worker only needs to read `SERVER_STARTER_PORT`
and `accept()` on the listed descriptors, exactly as with the Perl original.

## Graceful restarts

The recommended way to shut a worker down gracefully is to have it, on receiving
its stop signal (`SIGTERM` by default):

1. stop `accept()`ing new connections, and
2. finish the requests already in flight, then exit.

Because the new generation is already accepting on the shared socket before the
old one is signalled, no incoming connection is ever refused during the handover.

## Options

| Option | Default | Description |
| --- | --- | --- |
| `--port=(port\|host:port\|u<port>)[=fd]` | — | TCP (or UDP with the `u` prefix) port to listen on; repeatable. A bare port binds `0.0.0.0`. |
| `--path=path[=fd]` | — | Unix-domain socket path to listen on; repeatable. |
| `--interval=SECONDS` | `1` | Seconds to wait after spawning a worker before declaring it up. |
| `--signal-on-hup=SIG` | `TERM` | Signal sent to old workers on a graceful restart. |
| `--signal-on-term=SIG` | `TERM` | Signal sent to workers when shutting down. |
| `--pid-file=FILE` | — | Write (and lock) the superdaemon's pid here. |
| `--status-file=FILE` | — | Track the running worker generations here. |
| `--dir=PATH` | — | `chdir` here before spawning workers. |
| `--log-file=FILE` | — | Redirect worker stdout/stderr to a file, or to a command with a `\|cmd` value. |
| `--backlog=N` | `SOMAXCONN` | `listen()` backlog. |
| `--envdir=DIR` | — | Load environment variables from a directory (one file per variable). |
| `--enable-auto-restart` | off | Periodically restart the worker on a timer. |
| `--auto-restart-interval=SECONDS` | `360` | Interval used by auto-restart. |
| `--kill-old-delay=SECONDS` | `5` with auto-restart, else `0` | Delay before signalling old workers on restart. |
| `--daemonize` | off | Detach and run in the background. |
| `--restart` | — | Restart a running superdaemon (needs `--pid-file` and `--status-file`). |
| `--stop` | — | Stop a running superdaemon (needs `--pid-file`). |
| `--help` / `--version` | — | Print help / version and exit. |

The worker command follows a `--` separator (or is simply the first non-option
argument).

## The `SERVER_STARTER_PORT` format

A `;`-separated list of `binding=fd` entries, where a binding is `host:port`
(e.g. `0.0.0.0:8080`, `[::1]:8080`) or a unix socket path:

```
0.0.0.0:8080=3;[::1]:8080=4;/tmp/app.sock=5
```

Each `fd` is an inherited, already-`listen()`ing socket.

## How it works

```
                 SIGHUP / --restart
                        │
   ┌────────────┐       ▼        ┌────────────┐
   │ listening  │   start_server │  worker    │  generation N
   │  sockets   │──(fd inherited)│ (gen N)    │
   └────────────┘       │        └────────────┘
        ▲               │ spawn new gen, wait --interval,
        │ shared        │ then signal old gen after --kill-old-delay
        │               ▼        ┌────────────┐
        └───────────────────────►│  worker    │  generation N+1
                                 │ (gen N+1)  │
                                 └────────────┘
```

The superdaemon owns the sockets for its entire lifetime; workers come and go,
each inheriting the same descriptors. If a worker dies unexpectedly, the
superdaemon logs it and immediately spawns a replacement.

## Compatibility notes

- Unix-only (Linux and macOS are tested), as with the original.
- Signal names may be given with or without the `SIG` prefix (`TERM`, `SIGTERM`).

## License

MIT © ICHINOSE Shogo. See [LICENSE](LICENSE).
