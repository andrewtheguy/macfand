//! A multi-sensor fan control daemon for Intel MacBooks.
//!
//! `mbpfan` and friends read the CPU package temperature and nothing else, so
//! any other sensor on the board — the PCH die, memory, the chassis skin — can
//! sit at whatever temperature it likes without the fan ever noticing. macfand
//! runs a PI controller per sensor and commands whichever one is asking for the
//! most air.

mod config;
mod control;
mod sd;
mod sysfs;

use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use config::Config;
use control::{Governor, Reason, Sensor};
use sysfs::{Fan, Source, Temps, discover_fans};

const DEFAULT_CONFIG: &str = "/etc/macfand.toml";

/// Where the exclusive lock on the fans lives. Under `/run` so it is gone after
/// a reboot, and so it needs the same root the fans themselves do.
const LOCK_PATH: &str = "/run/macfand.lock";

/// How often to restate the current state when nothing has changed. Enough to
/// show the daemon is alive in the journal, rare enough not to fill it.
const HEARTBEAT: Duration = Duration::from_secs(300);

/// Signals are checked this often, so a SIGTERM is acted on promptly even
/// though the control loop itself runs once a second.
const SIGNAL_GRANULARITY: Duration = Duration::from_millis(100);

/// How many consecutive failed fan writes to give up after. The SMC's own fan
/// curve is switched off for as long as we run, so a daemon that cannot write
/// is holding the fans at whatever it last managed to command. Exiting hands them back to the
/// SMC, which is a far better state than pretending to be in control of them.
const MAX_WRITE_FAILURES: u32 = 5;

const USAGE: &str = "\
macfand — a multi-sensor fan control daemon for Intel MacBooks

usage: macfand <command> [options]

commands:
  daemon [--config PATH]   run the control loop: a PI controller per configured
                           sensor, driving the fans at whichever one is asking
                           for the most air, so a hot chassis or PCH raises the
                           fan even when the CPU package is comfortable
  show [--config PATH]     list every sensor and fan the machine exposes, with
                           current readings, and exit; does not need root
  restore                  hand every fan back to the SMC and exit

options:
  --config PATH   read this config instead of /etc/macfand.toml; without the
                  flag a missing file is not an error, the built-in defaults
                  are used as-is
  -h, --help      print this and exit
  -V, --version   print the version and exit

Driving the fan needs root, and needs the applesmc and coretemp modules loaded.
While macfand runs it drives the fans itself, in place of the SMC's own curve;
they are handed back to the SMC on exit, including on SIGTERM, SIGINT and
SIGHUP.";

fn main() {
    if let Err(e) = run() {
        eprintln!("macfand: {e:#}");
        std::process::exit(1);
    }
}

/// Exit code for a command line we could not make sense of, kept distinct from
/// the 1 that a failure to actually drive the fans exits with.
const EXIT_USAGE: i32 = 2;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Daemon,
    Show,
    Restore,
}

/// What a command line asked for, once it has been understood.
///
/// Parsing answers with this rather than doing the work itself so that the
/// whole of the command line surface can be tested without a process to exit
/// or an SMC to talk to.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Version,
    Run(Command, Option<PathBuf>),
}

/// Understand a command line, or say why it cannot be.
///
/// The `Err` is a usage error — a message to put in front of the usage text —
/// never a failure of the work itself.
fn parse(args: impl IntoIterator<Item = String>) -> Result<Invocation, String> {
    let mut args = args.into_iter();
    // No command at all is someone finding their way around rather than a
    // mistake, so it gets the help text rather than an error.
    let Some(first) = args.next() else {
        return Ok(Invocation::Help);
    };

    let command = match first.as_str() {
        "-h" | "--help" | "help" => return Ok(Invocation::Help),
        "-V" | "--version" => return Ok(Invocation::Version),
        "daemon" => Command::Daemon,
        "show" => Command::Show,
        "restore" => Command::Restore,
        other => return Err(format!("unknown command `{other}`")),
    };

    // Only the two commands that read a config accept one; `restore` talks to
    // the hardware alone, so a --config there is a mistake worth reporting
    // rather than something to accept and ignore.
    let takes_config = matches!(command, Command::Daemon | Command::Show);
    let mut config_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "--config" if takes_config => {
                let path = args.next().ok_or("--config needs a path")?;
                config_path = Some(PathBuf::from(path));
            }
            other => return Err(format!("{first}: unexpected argument `{other}`")),
        }
    }

    Ok(Invocation::Run(command, config_path))
}

fn run() -> Result<()> {
    let invocation = parse(std::env::args().skip(1)).unwrap_or_else(|message| {
        eprintln!("macfand: {message}\n\n{USAGE}");
        std::process::exit(EXIT_USAGE);
    });

    match invocation {
        Invocation::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Invocation::Version => {
            println!("macfand {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Invocation::Run(Command::Daemon, config) => daemon(config.as_deref()),
        Invocation::Run(Command::Show, config) => show(config.as_deref()),
        Invocation::Run(Command::Restore, _) => restore(),
    }
}

/// Read the config, or fall back to the built-in defaults.
///
/// An explicitly named config that does not exist is an error — the operator
/// asked for that file specifically. The default path missing is not, so the
/// daemon is useful before anything has been written to /etc.
fn load_config(path: Option<&Path>) -> Result<Config> {
    match path {
        Some(p) => Config::load(p),
        None => {
            let default = Path::new(DEFAULT_CONFIG);
            if default.exists() {
                Config::load(default)
            } else {
                eprintln!("macfand: no {DEFAULT_CONFIG}, using built-in defaults");
                Ok(Config::default())
            }
        }
    }
}

/// Resolve configured sensors to sysfs paths, and prove each one reads.
///
/// This is deliberately strict. A fan daemon that starts up having quietly
/// dropped two of its four inputs is worse than one that refuses to start.
fn resolve_sensors(config: &Config) -> Result<Vec<Sensor>> {
    let sources: BTreeSet<Source> = config.sensors.iter().map(|s| s.source).collect();
    // A source that will not discover is fatal only to the sensors that are not
    // optional. A machine with no `coretemp` should start on an optional
    // coretemp sensor exactly as it does on one whose label is missing —
    // otherwise the whole driver has to be present for a sensor declared as
    // needing not to be.
    let tables: Vec<(Source, Result<Temps>)> =
        sources.into_iter().map(|s| (s, Temps::discover(s))).collect();

    let mut sensors = Vec::new();
    for cfg in &config.sensors {
        let discovery =
            &tables.iter().find(|(s, _)| *s == cfg.source).expect("source was discovered").1;
        let table = match discovery {
            Ok(table) => table,
            Err(e) => {
                if cfg.optional {
                    eprintln!(
                        "macfand: optional sensor {} needs {}, which is unavailable ({e:#}), skipping",
                        cfg.key, cfg.source
                    );
                    continue;
                }
                bail!("sensor {} needs {}, which is unavailable: {e:#}", cfg.key, cfg.source);
            }
        };
        let Some(path) = table.path(&cfg.key) else {
            if cfg.optional {
                eprintln!("macfand: optional sensor {} not present on this machine, skipping", cfg.key);
                continue;
            }
            let available: Vec<&str> = table.labels().map(|(l, _)| l).collect();
            bail!(
                "sensor {:?} is not exposed by {}; available labels are: {}",
                cfg.key,
                cfg.source,
                available.join(", ")
            );
        };

        // One good reading now, so a typo'd or dead sensor is a startup failure
        // rather than something that fails safe to maximum an hour later.
        let reading = sysfs::read_temp(path)
            .with_context(|| format!("reading sensor {} at {}", cfg.key, path.display()))?;
        let [lo, hi] = config.plausible_range_c;
        if !(lo..=hi).contains(&reading) {
            if cfg.optional {
                eprintln!(
                    "macfand: optional sensor {} reads {reading:.1}C, outside the plausible range; \
                     it is probably not populated on this board, skipping",
                    cfg.key
                );
                continue;
            }
            bail!(
                "sensor {} reads {reading:.1}C, outside plausible_range_c ([{lo}, {hi}]); \
                 it is probably not populated on this board — mark it optional or remove it",
                cfg.key
            );
        }

        let mut sensor = Sensor::new(cfg.clone(), path);
        sensor.seed(reading);
        sensors.push(sensor);
    }

    if sensors.is_empty() {
        bail!("every configured sensor was missing or implausible; nothing to steer on");
    }
    Ok(sensors)
}

/// Work out the speed range to steer within: the hardware's, narrowed by the
/// config but never widened past what the SMC will accept.
fn speed_range(config: &Config, fans: &[Fan]) -> Result<(u32, u32)> {
    // With more than one fan the usable range is the overlap, since one demand
    // drives them all.
    let hw_min = fans.iter().map(|f| f.hw_min).max().unwrap_or(0);
    let hw_max = fans.iter().map(|f| f.hw_max).min().unwrap_or(0);
    if hw_min >= hw_max {
        bail!("the fans report an unusable speed range ({hw_min}-{hw_max} rpm)");
    }
    let min = config.min_rpm.unwrap_or(hw_min).clamp(hw_min, hw_max);
    let max = config.max_rpm.unwrap_or(hw_max).clamp(min, hw_max);
    if min >= max {
        bail!("min_rpm and max_rpm leave no range to steer in ({min}-{max} rpm)");
    }
    Ok((min, max))
}

fn daemon(config_path: Option<&Path>) -> Result<()> {
    let config = load_config(config_path)?;
    let sensors = resolve_sensors(&config)?;

    let applesmc = Temps::discover(Source::Applesmc)
        .context("locating the SMC to drive the fans")?
        .dir;
    let fans = discover_fans(&applesmc)?;
    let (min_rpm, max_rpm) = speed_range(&config, &fans)?;

    eprintln!(
        "macfand {} steering {} on {} sensor{}, {min_rpm}-{max_rpm} rpm, every {}ms",
        env!("CARGO_PKG_VERSION"),
        fans.iter().map(|f| f.label.as_str()).collect::<Vec<_>>().join(", "),
        sensors.len(),
        if sensors.len() == 1 { "" } else { "s" },
        config.poll_interval_ms,
    );
    for s in &sensors {
        eprintln!(
            "macfand:   {} ({}) from {}C target {}C critical {}C",
            s.key(),
            s.cfg.source,
            s.cfg.baseline_from,
            s.cfg.target,
            s.cfg.critical
        );
    }

    // Registered before we touch the fans, so there is no window in which the
    // SMC's own curve is switched off and a SIGTERM would not be caught.
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .with_context(|| format!("registering a handler for signal {signal}"))?;
    }

    // Before any fan is taken off the SMC's curve, and declared before the
    // guard so that it is still held while the guard hands the fans back.
    let _fan_lock = Lock::acquire(Path::new(LOCK_PATH))?;
    let guard = FanGuard::engage(fans)?;
    // Take over from wherever the SMC has the fan rather than from the floor,
    // so a machine that is already hot is not briefly slowed down on handover.
    // Not knowing means taking over at maximum: the first `dt` is nearly zero,
    // so the slew limit cannot walk a wrong guess back for several seconds, and
    // guessing low on a hot machine is the expensive direction to be wrong in.
    let start_rpm = guard.current_rpm().unwrap_or_else(|| {
        eprintln!(
            "macfand: could not read every fan's tachometer; taking over at {max_rpm} rpm rather \
             than assuming the machine is idle"
        );
        max_rpm
    });
    let mut governor = Governor::new(&config, sensors, min_rpm, max_rpm, start_rpm);
    let notifier = sd::Notifier::from_env();
    notifier.ready();

    let interval = config.poll_interval();
    let mut last_reason: Option<Reason> = None;
    let mut last_logged = Instant::now();
    let mut previous = Instant::now();
    let mut written: Option<u32> = None;
    let mut write_failures: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        // Passed on as it really is. A suspend/resume cycle hands us an
        // enormous dt, and the governor is the thing that knows which parts of
        // its own arithmetic want the real elapsed time and which want it
        // bounded; clamping here took that choice away from it.
        let dt = (now - previous).as_secs_f64();
        previous = now;

        for (key, problem) in governor.sample() {
            eprintln!("macfand: sensor {key}: {problem}");
        }
        let command = governor.step(dt);

        // The deadband keeps us off the SMC when nothing meaningful changed; a
        // critical or failed sensor writes regardless, and so does an
        // outstanding failure. Without that last one, a demand that settles
        // back within the deadband of the last *successful* write is never
        // retried: the fans stay unreconciled — with several of them, some may
        // have taken the write that the others refused — and `write_failures`
        // stays non-zero for good, so the daemon withholds the watchdog forever
        // without ever reaching MAX_WRITE_FAILURES and getting out of the way.
        let urgent = matches!(command.reason, Reason::Critical(_) | Reason::Failed(_));
        let changed = written.is_none_or(|w| w.abs_diff(command.rpm) >= config.deadband_rpm);
        if changed || urgent || write_failures > 0 {
            match guard.set_all(command.rpm) {
                Ok(()) => {
                    written = Some(command.rpm);
                    write_failures = 0;
                }
                // Losing one write is survivable — applesmc returns EBUSY often
                // enough on this hardware to matter. Losing them persistently
                // means we are not driving the fan at all, so we stop claiming
                // to systemd that we are and then get out of the way.
                Err(e) => {
                    eprintln!("macfand: {e:#}");
                    write_failures += 1;
                    if write_failures >= MAX_WRITE_FAILURES {
                        bail!(
                            "giving up after {write_failures} consecutive failed fan writes; \
                             handing the fans back to the SMC"
                        );
                    }
                }
            }
        }

        let reason_changed = last_reason.as_ref() != Some(&command.reason);
        if reason_changed || now.duration_since(last_logged) >= HEARTBEAT {
            eprintln!("macfand: {} rpm — {} [{}]", command.rpm, command.reason, readings(&governor));
            last_reason = Some(command.reason.clone());
            last_logged = now;
        }
        // A daemon that cannot write the fan is not healthy however busy its
        // loop is, so it must not go on telling systemd that it is.
        let healthy = write_failures == 0;
        notifier.status(&format!("{} rpm, {}", command.rpm, command.reason));
        if healthy {
            notifier.watchdog();
        }

        // The watchdog deadline is systemd's and the poll interval is the
        // operator's; a poll slower than the deadline still has to be pinged in
        // between, or systemd kills a daemon that is only being slow on purpose.
        let keepalive = healthy.then(|| notifier.watchdog_interval.map(|every| (&notifier, every)));
        sleep_until(now + interval, &stop, keepalive.flatten());
    }

    notifier.stopping();
    eprintln!("macfand: stopping, handing the fans back to the SMC");
    // `guard` restores on drop, including on the error paths above.
    Ok(())
}

/// Sleep in slices so a signal is noticed promptly, keeping the watchdog fed
/// while we do.
///
/// Pinging from here rather than once per poll is what decouples the poll
/// interval from `WatchdogSec`. It does not weaken the watchdog: this is still
/// the control loop, so a wedge anywhere else in it — reading sensors, writing
/// the fan — stops the pings exactly as before.
fn sleep_until(deadline: Instant, stop: &AtomicBool, keepalive: Option<(&sd::Notifier, Duration)>) {
    let mut next_ping = keepalive.map(|(_, every)| Instant::now() + every);
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return;
        }
        if let (Some((notifier, every)), Some(at)) = (keepalive, next_ping.as_mut())
            && now >= *at
        {
            notifier.watchdog();
            *at = now + every;
        }
        std::thread::sleep(remaining.min(SIGNAL_GRANULARITY));
    }
}

fn readings(governor: &Governor) -> String {
    governor
        .sensors
        .iter()
        .map(|s| match (s.last_reading(), s.is_failed()) {
            (_, true) => format!("{}=failed", s.key()),
            (Some(t), _) => format!("{}={t:.0}C", s.key()),
            (None, _) => format!("{}=?", s.key()),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// An exclusive claim on the fans, held for as long as the process runs.
///
/// Two daemons driving the fans fight: they overwrite each other's commands
/// from two different configurations, and whichever exits first hands the fans back
/// to the SMC while the other goes on believing it is driving them. `flock`
/// rather than a pidfile because the kernel drops it when the process dies
/// however it dies — including under the SIGKILL that would strand a pidfile
/// and leave the fans unclaimable until someone deleted it by hand.
struct Lock {
    /// Held only for its lifetime — closing the file is what releases the lock.
    _file: std::fs::File,
}

impl Lock {
    fn acquire(path: &Path) -> Result<Lock> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening the lock file {}", path.display()))?;
        // SAFETY: `file` owns the descriptor for the whole call, and the lock
        // it takes lives exactly as long as the `File` that holds it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                bail!(
                    "another macfand already has the fans ({} is locked); \
                     stop it before starting a second one",
                    path.display()
                );
            }
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("locking {}", path.display()));
        }
        Ok(Lock { _file: file })
    }
}

/// Takes the fans off the SMC's own curve, and gives them back however we
/// leave.
///
/// Once its curve is switched off the SMC keeps running whatever speed it was
/// last told, so a daemon that exits without clearing the flag strands the fan —
/// stuck at maximum after a hot spell, or worse, stuck at idle. Drop covers a
/// clean exit, a signal and a panic; it cannot cover SIGKILL, which is what the
/// unit's ExecStopPost is for.
struct FanGuard {
    fans: Vec<Fan>,
}

impl FanGuard {
    fn engage(fans: Vec<Fan>) -> Result<FanGuard> {
        // Built first so a failure part way through still restores the fans
        // that were already switched over.
        let guard = FanGuard { fans };
        for fan in &guard.fans {
            fan.set_software_control(true)?;
        }
        Ok(guard)
    }

    /// What the fans are actually doing: the fastest of them, so a handover
    /// never begins by slowing a fan that is already working.
    ///
    /// `None` unless every fan reads. Answering with the fastest of the ones
    /// that happened to read would report a fan at idle as the whole truth
    /// while an unreadable one next to it is flat out.
    fn current_rpm(&self) -> Option<u32> {
        self.fans.iter().try_fold(0, |fastest, fan| Some(fastest.max(fan.rpm().ok()?)))
    }

    fn set_all(&self, rpm: u32) -> Result<()> {
        for fan in &self.fans {
            fan.set_rpm(rpm)?;
        }
        Ok(())
    }
}

impl Drop for FanGuard {
    fn drop(&mut self) {
        for fan in &self.fans {
            if let Err(e) = fan.set_software_control(false) {
                eprintln!("macfand: could not hand {} back to the SMC: {e:#}", fan.label);
            }
        }
    }
}

fn restore() -> Result<()> {
    let applesmc = Temps::discover(Source::Applesmc)?.dir;
    // Every fan is attempted even after one fails. This is the unit's
    // ExecStopPost recovery path, so a single busy or broken fan must not be
    // what leaves the rest of them stranded off the SMC's curve.
    let mut stranded = Vec::new();
    for fan in discover_fans(&applesmc)? {
        match fan.set_software_control(false) {
            Ok(()) => println!("{} handed back to the SMC", fan.label),
            Err(e) => {
                eprintln!("macfand: {e:#}");
                stranded.push(fan.label);
            }
        }
    }
    if !stranded.is_empty() {
        bail!("still not back on the SMC's curve: {}", stranded.join(", "));
    }
    Ok(())
}

fn show(config_path: Option<&Path>) -> Result<()> {
    let config = load_config(config_path)?;
    let configured: BTreeSet<(Source, &str)> =
        config.sensors.iter().map(|s| (s.source, s.key.as_str())).collect();

    for source in [Source::Coretemp, Source::Applesmc] {
        let table = match Temps::discover(source) {
            Ok(t) => t,
            Err(e) => {
                println!("{source}: unavailable ({e})");
                continue;
            }
        };
        println!("\n{source} ({})", table.dir.display());
        for (label, path) in table.labels() {
            let reading = match sysfs::read_temp(path) {
                Ok(t) => format!("{t:6.1} C"),
                Err(_) => "     ?  ".to_string(),
            };
            let [lo, hi] = config.plausible_range_c;
            let note = match sysfs::read_temp(path) {
                Ok(t) if !(lo..=hi).contains(&t) => "  (implausible — likely not populated)",
                _ if configured.contains(&(source, label)) => "  (steering on this)",
                _ => "",
            };
            println!("  {label:<14} {reading}{note}");
        }
    }

    let applesmc = Temps::discover(Source::Applesmc)?.dir;
    println!("\nfans ({})", applesmc.display());
    for fan in discover_fans(&applesmc)? {
        let rpm = fan.rpm().map(|r| r.to_string()).unwrap_or_else(|_| "?".into());
        let driven_by_software = std::fs::read_to_string(&fan.control_mode)
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        println!(
            "  {:<14} {rpm:>5} rpm   range {}-{} rpm   {}",
            fan.label,
            fan.hw_min,
            fan.hw_max,
            if driven_by_software { "macfand" } else { "SMC" }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Invocation {
        parse(args.iter().map(|a| a.to_string())).expect("this command line must be understood")
    }

    fn rejected(args: &[&str]) -> String {
        parse(args.iter().map(|a| a.to_string())).expect_err("this command line must be rejected")
    }

    #[test]
    fn each_command_stands_on_its_own() {
        assert_eq!(parsed(&["daemon"]), Invocation::Run(Command::Daemon, None));
        assert_eq!(parsed(&["show"]), Invocation::Run(Command::Show, None));
        assert_eq!(parsed(&["restore"]), Invocation::Run(Command::Restore, None));
    }

    #[test]
    fn nothing_at_all_is_help_rather_than_an_error() {
        // Bare `macfand` used to start the daemon. It must not do so now that
        // the daemon has a name: an operator typing it expects to be told what
        // the commands are, not to have the SMC taken off its own fan curve.
        assert_eq!(parsed(&[]), Invocation::Help);
    }

    #[test]
    fn help_and_version_are_spelled_every_usual_way() {
        for args in [&["-h"][..], &["--help"], &["help"]] {
            assert_eq!(parsed(args), Invocation::Help, "{args:?}");
        }
        for args in [&["-V"][..], &["--version"]] {
            assert_eq!(parsed(args), Invocation::Version, "{args:?}");
        }
    }

    #[test]
    fn help_asked_for_after_a_command_is_still_help() {
        // Not a daemon run with a stray flag — asking `macfand daemon --help`
        // must never be what drives the fans.
        assert_eq!(parsed(&["daemon", "--help"]), Invocation::Help);
        assert_eq!(parsed(&["show", "-h"]), Invocation::Help);
        assert_eq!(parsed(&["restore", "--help"]), Invocation::Help);
    }

    #[test]
    fn the_commands_that_read_a_config_take_one() {
        let expected = Some(PathBuf::from("/tmp/macfand.toml"));
        assert_eq!(
            parsed(&["daemon", "--config", "/tmp/macfand.toml"]),
            Invocation::Run(Command::Daemon, expected.clone())
        );
        assert_eq!(
            parsed(&["show", "--config", "/tmp/macfand.toml"]),
            Invocation::Run(Command::Show, expected)
        );
    }

    #[test]
    fn a_config_restore_cannot_use_is_refused_rather_than_ignored() {
        // Accepting it silently would let an operator believe `restore` had
        // read their file and done something specific to it.
        assert!(rejected(&["restore", "--config", "/tmp/macfand.toml"]).contains("--config"));
    }

    #[test]
    fn a_config_flag_with_no_path_is_a_usage_error() {
        // And a usage error, not a run that quietly falls back to the default
        // config: the operator named a file they meant to be used.
        assert!(rejected(&["daemon", "--config"]).contains("needs a path"));
    }

    #[test]
    fn unknown_commands_and_stray_arguments_are_refused() {
        assert!(rejected(&["bogus"]).contains("bogus"));
        assert!(rejected(&["daemon", "extra"]).contains("extra"));
        // A path where the command belongs is the likeliest typo of all, and
        // must not be mistaken for a command.
        assert!(rejected(&["/etc/macfand.toml"]).contains("unknown command"));
    }

    #[test]
    fn the_flags_the_subcommands_replaced_are_gone() {
        // There is no compatibility path back to the old spelling; these must
        // fail loudly rather than being quietly accepted again some day.
        for args in [&["--show"][..], &["--restore"], &["--config", "/tmp/macfand.toml"]] {
            let message = rejected(args);
            assert!(message.contains("unknown command"), "{args:?} gave: {message}");
        }
    }

    #[test]
    fn the_fans_cannot_be_claimed_twice() {
        let path = std::env::temp_dir().join(format!("macfand-lock-test-{}", std::process::id()));
        let held = Lock::acquire(&path).expect("the first claim must succeed");
        assert!(Lock::acquire(&path).is_err(), "a second macfand must not get the fans");
        drop(held);
        // Releasing it must actually release it, or a restart never comes back.
        Lock::acquire(&path).expect("the lock must be free once dropped");
        let _ = std::fs::remove_file(&path);
    }
}
