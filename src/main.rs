//! Run the Mac's fans at max for a while, then hand them back to macOS.

#![forbid(unsafe_code)]

#[cfg(not(target_os = "macos"))]
compile_error!("air-conditioning only works on macOS");

mod smc;

use std::fs::{File, TryLockError};
use std::io::Read;
use std::num::NonZeroU16;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{env, io};

use smc::{MaxedFans, Smc};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "\
Run the Mac's fans at max for a while, then hand them back to macOS.

Usage: air-conditioning [MINUTES] [--target CELSIUS]
       air-conditioning off

Arguments:
  MINUTES           How long to hold the fans at max [default: 30]

Options:
  --target CELSIUS  Stop early once the hottest CPU/GPU sensor has held under this for a minute
  -h, --help        Print help
  -V, --version     Print version

Commands:
  off               Hand the fans back to macOS now, ending any run in progress
";

const DEFAULT_MINUTES: NonZeroU16 = NonZeroU16::new(30).unwrap();
/// How often a run checks on the fans, and so how long `off` can take to end it.
const TICK: Duration = Duration::from_secs(5);
/// A minute of ticks under target, so one cool dip doesn't end the run.
const COOL_READINGS: u32 = 12;
/// Held for the length of a run so two can't fight over the fans.
const LOCK_PATH: &str = "/var/run/air-conditioning.lock";
/// Hidden argument that starts the watchdog a run leaves behind it.
const GUARD_ARG: &str = "__guard";

#[derive(Debug, PartialEq)]
enum Action {
    Run {
        minutes: NonZeroU16,
        target: Option<f32>,
    },
    Off,
    Guard,
    Help,
    Version,
}

fn parse(args: impl IntoIterator<Item = String>) -> std::result::Result<Action, String> {
    let args: Vec<String> = args.into_iter().collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Action::Help);
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        return Ok(Action::Version);
    }
    if args == ["off"] {
        return Ok(Action::Off);
    }
    if args == [GUARD_ARG] {
        return Ok(Action::Guard);
    }

    let mut minutes = None;
    let mut target = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let celsius = match arg.strip_prefix("--target") {
            Some("") => Some(args.next().ok_or("--target needs a temperature")?),
            Some(rest) => rest.strip_prefix('=').map(str::to_owned),
            None => None,
        };
        if let Some(celsius) = celsius {
            let parsed = celsius.parse::<f32>().ok().filter(|c| c.is_finite());
            target = Some(parsed.ok_or(format!("not a temperature: {celsius}"))?);
        } else if arg.starts_with('-') {
            return Err(format!("unknown option: {arg}"));
        } else if minutes.is_some() {
            return Err(format!("unexpected argument: {arg}"));
        } else {
            let parsed = arg.parse().map_err(|_| {
                format!(
                    "MINUTES must be a whole number from 1 to {}, got {arg}",
                    u16::MAX
                )
            })?;
            minutes = Some(parsed);
        }
    }
    Ok(Action::Run {
        minutes: minutes.unwrap_or(DEFAULT_MINUTES),
        target,
    })
}

/// The SMC only takes fan writes from root, so re-run under sudo when started without it.
fn become_root() -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        return Ok(());
    }
    let failure = Command::new("sudo")
        .arg("--")
        .arg(env::current_exe()?)
        .args(env::args_os().skip(1))
        .exec();
    Err(format!("can't run sudo: {failure}").into())
}

fn lock_file() -> io::Result<File> {
    File::options().create(true).append(true).open(LOCK_PATH)
}

/// One more reading under target extends the streak; a warm or missing one resets it.
fn cool_streak(streak: u32, hottest: Option<f32>, target: f32) -> u32 {
    match hottest {
        Some(celsius) if celsius < target => streak + 1,
        _ => 0,
    }
}

fn run(minutes: NonZeroU16, target: Option<f32>) -> Result<()> {
    let lock = lock_file()?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err("already running; `air-conditioning off` ends it".into());
        }
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    let _guard = spawn_guard()?;
    let (stop_tx, stop) = mpsc::channel();
    ctrlc::set_handler(move || {
        // The receiver only goes away once the run is over, when there's nothing left to stop.
        let _ = stop_tx.send(());
    })?;

    let smc = Smc::open()?;
    let cooling = match target {
        Some(target) => Some((target, smc.thermometer()?)),
        None => None,
    };
    let fans = MaxedFans::start(&smc).map_err(|e| format!("couldn't take over the fans: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(u64::from(minutes.get()) * 60);
    match &cooling {
        Some((target, thermometer)) => {
            let now = thermometer
                .hottest()
                .map_or("unknown".into(), |celsius| format!("{celsius:.1}°C"));
            println!(
                "fans at max for up to {minutes} min, or until the hottest sensor holds under {target}°C (now {now})"
            );
        }
        None => println!("fans at max for {minutes} min"),
    }

    let mut streak = 0;
    let ending = loop {
        let wait = TICK.min(deadline.saturating_duration_since(Instant::now()));
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break "stopped".to_owned(),
        }
        if !fans.still_held()? {
            break "something else took the fans over".to_owned();
        }
        if let Some((target, thermometer)) = &cooling {
            streak = cool_streak(streak, thermometer.hottest(), *target);
            if streak >= COOL_READINGS {
                break format!("held under {target}°C");
            }
        }
        if Instant::now() >= deadline {
            break format!("{minutes} min up");
        }
    };
    fans.release()?;
    println!("{ending}, fans back on auto");
    Ok(())
}

/// Starts a watchdog that hands the fans back once this process is gone, however it goes,
/// including `kill -9`. Its own process group keeps Ctrl-C and Ctrl-\\ from reaching it.
/// Keep the returned child alive for the whole run: dropping it closes the pipe the watchdog waits on.
fn spawn_guard() -> Result<Child> {
    let guard = Command::new(env::current_exe()?)
        .arg(GUARD_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(guard)
}

fn guard() -> Result<()> {
    // The pipe hits end-of-file when the run exits; a read error means the same.
    let _ = io::stdin().read_to_end(&mut Vec::new());
    // Taking the lock waits out the run's teardown, and any run that started since.
    lock_file()?.lock()?;
    Smc::open()?.release_fans()
}

fn off() -> Result<()> {
    let smc = Smc::open()?;
    let lock = lock_file()?;
    // A run that is still starting can take the fans back after a release, so keep releasing
    // until the run has noticed and let go of the lock.
    loop {
        smc.release_fans()?;
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => sleep(TICK),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    println!("fans back on auto");
    Ok(())
}

fn main() -> ExitCode {
    let action = match parse(env::args().skip(1)) {
        Ok(action) => action,
        Err(e) => {
            eprintln!("error: {e}\n\nRun `air-conditioning --help` for usage.");
            return ExitCode::from(2);
        }
    };
    let result = match action {
        Action::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Action::Version => {
            println!("air-conditioning {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::Off => become_root().and_then(|()| off()),
        Action::Guard => guard(),
        Action::Run { minutes, target } => become_root().and_then(|()| run(minutes, target)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_for(minutes: u16, target: Option<f32>) -> Action {
        let minutes = NonZeroU16::new(minutes).unwrap();
        Action::Run { minutes, target }
    }

    #[test]
    fn parses_the_command_line() {
        let cases = [
            ("", Ok(run_for(30, None))),
            ("10", Ok(run_for(10, None))),
            ("--target 55", Ok(run_for(30, Some(55.0)))),
            ("10 --target 55.5", Ok(run_for(10, Some(55.5)))),
            ("--target=55 10", Ok(run_for(10, Some(55.0)))),
            ("off", Ok(Action::Off)),
            ("__guard", Ok(Action::Guard)),
            ("10 --help", Ok(Action::Help)),
            ("-V", Ok(Action::Version)),
            (
                "0",
                Err("MINUTES must be a whole number from 1 to 65535, got 0".into()),
            ),
            (
                "70000",
                Err("MINUTES must be a whole number from 1 to 65535, got 70000".into()),
            ),
            ("10 20", Err("unexpected argument: 20".into())),
            ("10 off", Err("unexpected argument: off".into())),
            ("--target", Err("--target needs a temperature".into())),
            ("--target warm", Err("not a temperature: warm".into())),
            ("--target nan", Err("not a temperature: nan".into())),
            ("--targets 5", Err("unknown option: --targets".into())),
        ];
        for (line, expected) in cases {
            assert_eq!(
                parse(line.split_whitespace().map(str::to_owned)),
                expected,
                "`{line}`"
            );
        }
    }

    #[test]
    fn only_an_unbroken_run_of_cool_readings_counts() {
        let target = 55.0;
        let cases: [(&[Option<f32>], u32); 4] = [
            (&[Some(54.9), Some(50.0)], 2),
            (&[Some(50.0), Some(55.0)], 0),
            (&[Some(50.0), None, Some(50.0)], 1),
            (&[Some(50.0), Some(70.0), Some(50.0), Some(50.0)], 2),
        ];
        for (readings, expected) in cases {
            let streak = readings
                .iter()
                .fold(0, |streak, &hottest| cool_streak(streak, hottest, target));
            assert_eq!(streak, expected, "{readings:?}");
        }
    }
}
