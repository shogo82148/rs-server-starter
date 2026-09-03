//! Command-line parsing for the `start_server` binary.

use nix::sys::signal::Signal;

/// What the user asked `start_server` to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Bind sockets and supervise a worker (the default).
    Run,
    /// Send `SIGHUP` to a running superdaemon and wait for the new generation.
    Restart,
    /// Send `SIGTERM` to a running superdaemon.
    Stop,
    /// Print help text and exit.
    Help,
    /// Print the version and exit.
    Version,
}

/// Fully-parsed configuration for a `start_server` invocation.
#[derive(Debug)]
pub struct Options {
    pub action: Action,

    pub ports: Vec<String>,
    pub paths: Vec<String>,

    pub interval: u64,
    pub log_file: Option<String>,
    pub pid_file: Option<String>,
    pub dir: Option<String>,
    pub signal_on_hup: Signal,
    pub signal_on_term: Signal,
    pub backlog: Option<i32>,
    pub envdir: Option<String>,
    pub enable_auto_restart: bool,
    pub daemonize: bool,
    pub auto_restart_interval: u64,
    pub kill_old_delay: Option<u64>,
    pub status_file: Option<String>,

    /// The worker command and its arguments.
    pub command: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            action: Action::Run,
            ports: Vec::new(),
            paths: Vec::new(),
            interval: 1,
            log_file: None,
            pid_file: None,
            dir: None,
            signal_on_hup: Signal::SIGTERM,
            signal_on_term: Signal::SIGTERM,
            backlog: None,
            envdir: None,
            enable_auto_restart: false,
            daemonize: false,
            auto_restart_interval: 360,
            kill_old_delay: None,
            status_file: None,
            command: Vec::new(),
        }
    }
}

impl Options {
    /// The effective delay, in seconds, before old workers are signalled on a
    /// graceful restart. Defaults to 5 seconds when auto-restart is enabled and
    /// 0 otherwise, matching Server::Starter.
    pub fn effective_kill_old_delay(&self) -> u64 {
        self.kill_old_delay
            .unwrap_or(if self.enable_auto_restart { 5 } else { 0 })
    }

    /// The listen backlog, defaulting to `SOMAXCONN`.
    pub fn effective_backlog(&self) -> i32 {
        self.backlog.unwrap_or(libc::SOMAXCONN)
    }
}

/// Parses a signal name such as `TERM`, `SIGTERM`, `USR1` into a [`Signal`].
fn parse_signal(name: &str) -> Result<Signal, String> {
    let upper = name.to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    let with_prefix = format!("SIG{bare}");
    // nix's Signal parses the `SIG`-prefixed spelling.
    with_prefix
        .parse::<Signal>()
        .map_err(|_| format!("unknown signal {name:?}"))
}

/// Parses `start_server` command-line arguments (excluding argv[0]).
pub fn parse<I, S>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut opts = Options::default();
    let mut saw_restart = false;
    let mut saw_stop = false;

    let mut it = args.into_iter().map(Into::into).peekable();

    while let Some(arg) = it.next() {
        // `--` terminates options; the rest is the worker command verbatim.
        if arg == "--" {
            opts.command.extend(it.by_ref());
            break;
        }
        // The first bare (non `--foo`) argument begins the worker command; every
        // following token, option-looking or not, belongs to the worker.
        if !arg.starts_with("--") {
            opts.command.push(arg);
            opts.command.extend(it.by_ref());
            break;
        }

        // Split `--name=value` into name and an inlined value.
        let (name, inline_val) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        // Helper to obtain the value of a value-taking option.
        let mut take_value = |name: &str| -> Result<String, String> {
            if let Some(v) = inline_val.clone() {
                return Ok(v);
            }
            it.next()
                .ok_or_else(|| format!("option {name} requires a value"))
        };

        match name.as_str() {
            "--help" => opts.action = Action::Help,
            "--version" => opts.action = Action::Version,
            "--restart" => saw_restart = true,
            "--stop" => saw_stop = true,
            "--enable-auto-restart" => opts.enable_auto_restart = true,
            "--daemonize" => opts.daemonize = true,

            "--port" => opts.ports.push(take_value("--port")?),
            "--path" => opts.paths.push(take_value("--path")?),
            "--log-file" => opts.log_file = Some(take_value("--log-file")?),
            "--pid-file" => opts.pid_file = Some(take_value("--pid-file")?),
            "--dir" => opts.dir = Some(take_value("--dir")?),
            "--envdir" => opts.envdir = Some(take_value("--envdir")?),
            "--status-file" => opts.status_file = Some(take_value("--status-file")?),

            "--interval" => {
                opts.interval = take_value("--interval")?
                    .parse()
                    .map_err(|_| "--interval must be an integer".to_string())?;
            }
            "--backlog" => {
                opts.backlog = Some(
                    take_value("--backlog")?
                        .parse()
                        .map_err(|_| "--backlog must be an integer".to_string())?,
                );
            }
            "--auto-restart-interval" => {
                opts.auto_restart_interval = take_value("--auto-restart-interval")?
                    .parse()
                    .map_err(|_| "--auto-restart-interval must be an integer".to_string())?;
            }
            "--kill-old-delay" => {
                opts.kill_old_delay = Some(
                    take_value("--kill-old-delay")?
                        .parse()
                        .map_err(|_| "--kill-old-delay must be an integer".to_string())?,
                );
            }
            "--signal-on-hup" => {
                opts.signal_on_hup = parse_signal(&take_value("--signal-on-hup")?)?
            }
            "--signal-on-term" => {
                opts.signal_on_term = parse_signal(&take_value("--signal-on-term")?)?
            }

            other => return Err(format!("unknown option {other}")),
        }
    }

    // Resolve the action. --help / --version already win if present.
    if opts.action == Action::Run {
        match (saw_restart, saw_stop) {
            (true, true) => return Err("--restart and --stop are mutually exclusive".into()),
            (true, false) => opts.action = Action::Restart,
            (false, true) => opts.action = Action::Stop,
            (false, false) => {}
        }
    }

    // Validate the run action's requirements.
    if opts.action == Action::Run {
        if opts.command.is_empty() {
            return Err("no server program specified".into());
        }
        if opts.ports.is_empty() && opts.paths.is_empty() {
            return Err("must specify at least one of --port or --path".into());
        }
    }

    Ok(opts)
}

/// The usage/help text shown for `--help`.
pub const USAGE: &str = concat!(
    "start_server (rs-server-starter ",
    env!("CARGO_PKG_VERSION"),
    ") - a superdaemon for hot-deploying server programs\n",
    "\n",
    "Usage:\n",
    "    start_server [OPTIONS] -- server-prog server-arg...\n",
    "\n",
    "Options:\n",
    "    --port=(port|host:port|u<port>)[=fd]\n",
    "                       TCP (or UDP with the `u' prefix) port to listen on;\n",
    "                       may be specified multiple times.\n",
    "    --path=path[=fd]   unix socket path to listen on; repeatable.\n",
    "    --interval=seconds seconds to wait between spawning workers (default: 1).\n",
    "    --log-file=file    redirect worker stdout/stderr to file (a `|cmd'\n",
    "                       value pipes the logs to a command instead).\n",
    "    --pid-file=file    write the superdaemon pid to file.\n",
    "    --status-file=file file tracking the running worker generations.\n",
    "    --dir=path         chdir to path before spawning workers.\n",
    "    --signal-on-hup=SIG  signal sent to old workers on restart (default: TERM).\n",
    "    --signal-on-term=SIG signal sent to workers on shutdown  (default: TERM).\n",
    "    --backlog=n        listen backlog size (default: SOMAXCONN).\n",
    "    --envdir=path      load environment variables from a directory of files.\n",
    "    --enable-auto-restart          periodically restart the worker.\n",
    "    --auto-restart-interval=seconds interval for auto-restart (default: 360).\n",
    "    --kill-old-delay=seconds       delay before killing old workers\n",
    "                       (default: 5 with auto-restart, otherwise 0).\n",
    "    --daemonize        run the superdaemon in the background.\n",
    "    --restart          restart a running superdaemon (needs --pid-file and\n",
    "                       --status-file) and exit.\n",
    "    --stop             stop a running superdaemon (needs --pid-file) and exit.\n",
    "    --help             print this help and exit.\n",
    "    --version          print the version and exit.\n",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_run() {
        let o = parse(["--port", "8080", "--", "echo", "hi"]).unwrap();
        assert_eq!(o.action, Action::Run);
        assert_eq!(o.ports, vec!["8080"]);
        assert_eq!(o.command, vec!["echo", "hi"]);
        assert_eq!(o.interval, 1);
    }

    #[test]
    fn inline_values_and_repeats() {
        let o = parse([
            "--port=80",
            "--port=443",
            "--path=/tmp/s.sock",
            "--interval=3",
            "--",
            "prog",
        ])
        .unwrap();
        assert_eq!(o.ports, vec!["80", "443"]);
        assert_eq!(o.paths, vec!["/tmp/s.sock"]);
        assert_eq!(o.interval, 3);
    }

    #[test]
    fn command_without_double_dash() {
        let o = parse(["--port", "80", "myprog", "--flag", "arg"]).unwrap();
        // Everything from the first bare token on belongs to the worker.
        assert_eq!(o.command, vec!["myprog", "--flag", "arg"]);
    }

    #[test]
    fn restart_and_stop_actions() {
        let o = parse(["--restart", "--pid-file=/tmp/p", "--status-file=/tmp/s"]).unwrap();
        assert_eq!(o.action, Action::Restart);
        let o = parse(["--stop", "--pid-file=/tmp/p"]).unwrap();
        assert_eq!(o.action, Action::Stop);
    }

    #[test]
    fn signal_names() {
        let o = parse(["--signal-on-hup=USR1", "--port=1", "--", "p"]).unwrap();
        assert_eq!(o.signal_on_hup, Signal::SIGUSR1);
        let o = parse(["--signal-on-term", "SIGKILL", "--port=1", "--", "p"]).unwrap();
        assert_eq!(o.signal_on_term, Signal::SIGKILL);
    }

    #[test]
    fn requires_port_and_command() {
        assert!(parse(["--", "prog"]).is_err());
        assert!(parse(["--port=80"]).is_err());
    }

    #[test]
    fn kill_old_delay_defaults() {
        let o = parse(["--port=1", "--", "p"]).unwrap();
        assert_eq!(o.effective_kill_old_delay(), 0);
        let o = parse(["--enable-auto-restart", "--port=1", "--", "p"]).unwrap();
        assert_eq!(o.effective_kill_old_delay(), 5);
        let o = parse(["--kill-old-delay=2", "--port=1", "--", "p"]).unwrap();
        assert_eq!(o.effective_kill_old_delay(), 2);
    }
}
