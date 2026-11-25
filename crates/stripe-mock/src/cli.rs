//! The `ductkit-stripe-mock` command line: `serve`, `replay` and `check`.
//!
//! Arguments are parsed by hand; the binary in `main.rs` only calls [`run`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::{ClockMode, Config, MockError, MockStripe, ReplayOptions, RunExport, StartOptions};

const DEFAULT_API_ADDR: &str = "127.0.0.1:8420";
const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:8421";

const USAGE: &str = "usage:
  ductkit-stripe-mock serve --config <file> [--api-addr A] [--control-addr A]
                            [--clock virtual|wall] [--seed N] [--export-on-exit <file>]
  ductkit-stripe-mock replay --run <file> [--api-addr A] [--control-addr A]
                             [--drop-destinations] [--export-on-exit <file>]
  ductkit-stripe-mock check --config <file>

defaults: --api-addr 127.0.0.1:8420, --control-addr 127.0.0.1:8421;
the clock is `wall` unless the config sets `clock.mode` or --clock is given.
";

#[derive(Debug, PartialEq)]
struct Serve {
    config: PathBuf,
    api_addr: SocketAddr,
    control_addr: SocketAddr,
    clock: Option<ClockMode>,
    seed: Option<u64>,
    export_on_exit: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
struct Replay {
    run: PathBuf,
    api_addr: SocketAddr,
    control_addr: SocketAddr,
    drop_destinations: bool,
    export_on_exit: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
enum Command {
    Help,
    Serve(Serve),
    Replay(Replay),
    Check { config: PathBuf },
}

/// Walks `--flag value` pairs and `--switch` flags.
struct Flags {
    args: std::vec::IntoIter<String>,
}

impl Flags {
    fn next_flag(&mut self) -> Option<String> {
        self.args.next()
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        self.args
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))
    }

    fn addr(&mut self, flag: &str) -> Result<SocketAddr, String> {
        let v = self.value(flag)?;
        v.parse()
            .map_err(|_| format!("{flag}: `{v}` is not a socket address like 127.0.0.1:8420"))
    }
}

fn parse(args: Vec<String>) -> Result<Command, String> {
    let mut flags = Flags {
        args: args.into_iter(),
    };
    let Some(command) = flags.next_flag() else {
        return Err("missing command".into());
    };
    match command.as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "serve" => parse_serve(flags),
        "replay" => parse_replay(flags),
        "check" => {
            let mut config = None;
            while let Some(flag) = flags.next_flag() {
                match flag.as_str() {
                    "--config" => config = Some(PathBuf::from(flags.value(&flag)?)),
                    "--help" | "-h" => return Ok(Command::Help),
                    _ => return Err(format!("unknown flag `{flag}`")),
                }
            }
            Ok(Command::Check {
                config: config.ok_or("check needs --config <file>")?,
            })
        }
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_serve(mut flags: Flags) -> Result<Command, String> {
    let mut serve = Serve {
        config: PathBuf::new(),
        api_addr: DEFAULT_API_ADDR.parse().expect("valid default"),
        control_addr: DEFAULT_CONTROL_ADDR.parse().expect("valid default"),
        clock: None,
        seed: None,
        export_on_exit: None,
    };
    let mut config = None;
    while let Some(flag) = flags.next_flag() {
        match flag.as_str() {
            "--config" => config = Some(PathBuf::from(flags.value(&flag)?)),
            "--api-addr" => serve.api_addr = flags.addr(&flag)?,
            "--control-addr" => serve.control_addr = flags.addr(&flag)?,
            "--clock" => {
                serve.clock = Some(match flags.value(&flag)?.as_str() {
                    "virtual" => ClockMode::Virtual,
                    "wall" => ClockMode::Wall,
                    v => return Err(format!("--clock: `{v}` is not `virtual` or `wall`")),
                });
            }
            "--seed" => {
                let v = flags.value(&flag)?;
                let seed: u64 = v
                    .parse()
                    .map_err(|_| format!("--seed: `{v}` is not an unsigned integer"))?;
                // TOML integers are signed 64-bit.
                if i64::try_from(seed).is_err() {
                    return Err(format!("--seed: `{v}` is too large"));
                }
                serve.seed = Some(seed);
            }
            "--export-on-exit" => serve.export_on_exit = Some(PathBuf::from(flags.value(&flag)?)),
            "--help" | "-h" => return Ok(Command::Help),
            _ => return Err(format!("unknown flag `{flag}`")),
        }
    }
    serve.config = config.ok_or("serve needs --config <file>")?;
    Ok(Command::Serve(serve))
}

fn parse_replay(mut flags: Flags) -> Result<Command, String> {
    let mut replay = Replay {
        run: PathBuf::new(),
        api_addr: DEFAULT_API_ADDR.parse().expect("valid default"),
        control_addr: DEFAULT_CONTROL_ADDR.parse().expect("valid default"),
        drop_destinations: false,
        export_on_exit: None,
    };
    let mut run = None;
    while let Some(flag) = flags.next_flag() {
        match flag.as_str() {
            "--run" => run = Some(PathBuf::from(flags.value(&flag)?)),
            "--api-addr" => replay.api_addr = flags.addr(&flag)?,
            "--control-addr" => replay.control_addr = flags.addr(&flag)?,
            "--drop-destinations" => replay.drop_destinations = true,
            "--export-on-exit" => replay.export_on_exit = Some(PathBuf::from(flags.value(&flag)?)),
            "--help" | "-h" => return Ok(Command::Help),
            _ => return Err(format!("unknown flag `{flag}`")),
        }
    }
    replay.run = run.ok_or("replay needs --run <file>")?;
    Ok(Command::Replay(replay))
}

/// Reads a config file for standalone use: `clock.mode` defaults to `wall`
/// (spec 3), then `--clock` and `--seed` override the file.
///
/// The user's text is parsed as written, so parse errors carry its own line
/// numbers. Defaults and overrides are applied to the typed `Config` after that.
fn load_config(
    path: &std::path::Path,
    clock: Option<ClockMode>,
    seed: Option<u64>,
) -> Result<Config, MockError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| MockError::Config(format!("cannot read {}: {e}", path.display())))?;
    let mut config = Config::from_toml_str(&text)?;
    let table: toml::Table = text
        .parse()
        .map_err(|e| MockError::Config(format!("{}: {e}", path.display())))?;
    let file_sets_mode = table
        .get("clock")
        .and_then(toml::Value::as_table)
        .is_some_and(|c| c.contains_key("mode"));
    if let Some(mode) = clock {
        config.clock.mode = mode;
    } else if !file_sets_mode {
        config.clock.mode = ClockMode::Wall;
    }
    if let Some(seed) = seed {
        config.seed = seed;
    }
    config.validate()?;
    Ok(config)
}

/// Resolves on Ctrl-C, on SIGTERM (unix), or when `POST /control/shutdown` arrives.
struct StopSignals {
    interrupt: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl StopSignals {
    /// Installs the handlers now. Call it before the ready line is printed: a
    /// harness may signal the process the moment it reads that line, and a
    /// signal that arrives before a handler exists kills the process.
    async fn install() -> StopSignals {
        let mut interrupt: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async {
            // Without a signal handler only the control API can stop us.
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        });
        // `ctrl_c()` registers its handler on first poll, so poll once here.
        std::future::poll_fn(|cx| {
            let _ = interrupt.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        StopSignals {
            interrupt,
            // Registered on creation.
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .ok(),
        }
    }

    async fn wait(mut self, mock: &MockStripe) {
        let requested = mock.shutdown_requested();
        #[cfg(unix)]
        let terminate = async {
            match self.terminate.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! {
            () = &mut self.interrupt => {}
            () = terminate => {}
            () = requested => {}
        }
    }
}

/// Prints the ready line, waits for a stop, exports, and shuts down.
async fn serve_until_stopped(
    mock: MockStripe,
    control_addr: SocketAddr,
    export_on_exit: Option<PathBuf>,
) -> ExitCode {
    if !control_addr.ip().is_loopback() {
        eprintln!(
            "warning: the control API on {control_addr} has no authentication and is not bound to loopback"
        );
    }
    let signals = StopSignals::install().await;
    let ready = serde_json::json!({
        "api_url": mock.api_url(),
        "control_url": mock.control_url(),
        "seed": mock.seed(),
    });
    {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{ready}");
        let _ = out.flush();
    }
    signals.wait(&mock).await;
    let mut code = ExitCode::SUCCESS;
    if let Some(path) = export_on_exit
        && let Err(e) = mock.export_run().save(&path)
    {
        eprintln!("error: cannot write export {}: {e}", path.display());
        code = ExitCode::from(1);
    }
    mock.shutdown().await;
    code
}

fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {e}");
    ExitCode::from(1)
}

async fn run_command(command: Command) -> ExitCode {
    match command {
        Command::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Check { config } => match load_config(&config, None, None) {
            Ok(_) => {
                println!("ok");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Command::Serve(s) => {
            let config = match load_config(&s.config, s.clock, s.seed) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let options = StartOptions {
                api_addr: Some(s.api_addr),
                control_addr: Some(s.control_addr),
            };
            match MockStripe::start_with(config, options).await {
                Ok(mock) => serve_until_stopped(mock, s.control_addr, s.export_on_exit).await,
                Err(e) => fail(e),
            }
        }
        Command::Replay(r) => {
            let export = match RunExport::load(&r.run) {
                Ok(e) => e,
                Err(e) => return fail(e),
            };
            let options = StartOptions {
                api_addr: Some(r.api_addr),
                control_addr: Some(r.control_addr),
            };
            let replay = ReplayOptions {
                drop_destinations: r.drop_destinations,
            };
            match MockStripe::replay_with(export, replay, options).await {
                Ok(mock) => serve_until_stopped(mock, r.control_addr, r.export_on_exit).await,
                Err(e) => fail(e),
            }
        }
    }
}

/// Runs the command line `args` (without the program name) and returns the exit code.
pub async fn run(args: Vec<String>) -> ExitCode {
    match parse(args) {
        Ok(command) => run_command(command).await,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
