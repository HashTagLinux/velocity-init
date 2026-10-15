//! Runtime configuration, parsed once at startup.
//!
//! Every knob PID 1 reads from the process environment is snapshotted
//! here, exactly once, before any service exists: init/inittab paths,
//! the getty executable, the boot concurrency cap, the control-socket
//! and journal candidate lists, and the control-connection idle
//! timeout. Callers then take values from `&RuntimeConfig` instead of
//! reading `std::env` at the use site.
//!
//! Why: environment reads scattered through hot paths (the per-tick
//! idle-connection sweep re-read `VELOCITY_CTL_IDLE_TIMEOUT_SECS` on
//! every pass) are implicit global inputs — invisible in signatures,
//! untestable without mutating process-global state, and racy under
//! `cargo test`'s parallel threads (hence the `ENV_LOCK`s this replaces).
//! A single snapshot struct makes the inputs explicit, and its pure
//! `from_lookup` core is unit-testable with plain maps.
//!
//! Deliberately out of scope (documented, not forgotten):
//! - `RUST_LOG` / `VELOCITY_JOURNAL_LEVEL`: consumed once by the logger
//!   constructors, never on a hot path.
//! - `VELOCITY_UTMP` / `VELOCITY_RUNLEVEL_FILE`: per-call rescue/test
//!   overrides resolved inside `utmp` (which also offers pure `_with`
//!   variants for its tests). The event loop's runlevel writes keep
//!   resolving them at the call site so a rescue operator can redirect
//!   the database without restarting PID 1.
//! - `velocityctl`-side reads (`VELOCITY_SHUTDOWN_PID`, socket/journal
//!   candidates): a separate process that must resolve the same
//!   environment on its own; it shares the `paths` helpers, so both
//!   ends still agree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::control_socket::{parse_idle_timeout, CONN_IDLE_TIMEOUT_ENV};
use crate::inittab::Inittab;
use crate::models::{BootMode, DEFAULT_INIT_DIR};
use crate::paths::{
    control_socket_candidates_with, journal_candidates_with, runtime_dir_candidates_with,
    runtime_dir_for,
};

/// Environment variable overrides, and their compiled-in defaults.
pub const INIT_DIR_ENV: &str = "VELOCITY_INIT_DIR";
pub const INITTAB_ENV: &str = "VELOCITY_INITTAB";
pub const JOBS_ENV: &str = "VELOCITY_INIT_JOBS";
pub const GETTY_ENV: &str = "VELOCITY_INIT_GETTY";
pub const ALLOW_SYSV_NONPID1_ENV: &str = "VELOCITY_ALLOW_SYSV_NONPID1";

/// Default console getty executable (overridden by [`GETTY_ENV`]).
pub const DEFAULT_GETTY: &str = "/sbin/agetty";

/// A single snapshot of every runtime knob (see the module docs).
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Init script directory (CLI `--init-dir` wins over [`INIT_DIR_ENV`]).
    pub init_dir: String,
    /// Inittab path (CLI `--inittab` wins over [`INITTAB_ENV`]).
    pub inittab: String,
    /// Boot concurrency cap per level (CLI `--jobs` wins over [`JOBS_ENV`]).
    pub jobs: usize,
    /// True when [`JOBS_ENV`] was present but unparseable, so [`Self::jobs`]
    /// fell back to `0`. Surfaced (instead of logged inside the pure
    /// [`Self::from_lookup`]) so `main` can warn once without re-reading
    /// the environment.
    pub jobs_env_invalid: bool,
    /// Console getty executable ([`GETTY_ENV`]).
    pub getty_exe: String,
    /// Rescue escape hatch for a non-PID-1 sysv boot ([`ALLOW_SYSV_NONPID1_ENV`]).
    pub allow_sysv_nonpid1: bool,
    /// Control-socket candidates, best first (shared with `velocityctl`
    /// via the `paths` helpers).
    pub socket_candidates: Vec<PathBuf>,
    /// Journal candidates, best first.
    pub journal_candidates: Vec<PathBuf>,
    /// Idle-connection reap deadline ([`CONN_IDLE_TIMEOUT_ENV`]).
    pub ctl_idle_timeout: Duration,
}

/// Parse a truthy environment switch: `1`/`yes`/`y`/`true`/`on` in any
/// case (with surrounding whitespace ignored) mean on; anything else —
/// including `0`/`no`/`false`/`off` and garbage — means off.
///
/// The previous matcher was case-sensitive (`"yes"` but not `"YES"` or
/// `"True"`), so a rescue operator exporting `VELOCITY_ALLOW_SYSV_NONPID1=YES`
/// was silently refused. PID 1 must not hinge on caps-lock.
pub fn parse_truthy(raw: Option<&str>) -> bool {
    match raw.map(str::trim).map(str::to_ascii_lowercase) {
        Some(ref v) if v.as_str() == "1" => true,
        Some(ref v) if v.as_str() == "yes" => true,
        Some(ref v) if v.as_str() == "y" => true,
        Some(ref v) if v.as_str() == "true" => true,
        Some(ref v) if v.as_str() == "on" => true,
        _ => false,
    }
}

/// Parse a `--jobs` / [`JOBS_ENV`] value: surrounding whitespace is
/// ignored, anything unparseable falls back to `0` (unlimited). The
/// boolean reports whether a *present* value was rejected, so callers
/// can warn instead of silently running unbounded.
pub fn parse_jobs(raw: Option<&str>) -> (usize, bool) {
    match raw.map(str::trim) {
        None => (0, false),
        Some(v) if v.is_empty() => (0, false),
        Some(v) => match v.parse::<usize>() {
            Ok(n) => (n, false),
            Err(_) => (0, true),
        },
    }
}

/// Whether an init-directory entry should be skipped during discovery.
///
/// Skips hidden files and `README*` (as before) plus the packaging and
/// editor debris Debian/Devuan actually leave in `/etc/init.d`:
/// `*.dpkg-dist`, `*.dpkg-old`, `*.dpkg-new`, `*.rpmnew`, `*.rpmsave`,
/// `*.rej`, `*.orig`, `*.bak`, `*.swp`, `*.tmp`, `*~` and emacs
/// autosaves (`#name#`). Without this a kept-backup script is parsed
/// (or worse, executed) as a second service.
pub fn should_skip_init_entry(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.starts_with("README") {
        return true;
    }
    if name.starts_with('#') || name.ends_with('#') || name.ends_with('~') {
        return true;
    }
    const SKIP_SUFFIXES: &[&str] = &[
        ".dpkg-dist",
        ".dpkg-old",
        ".dpkg-new",
        ".rpmnew",
        ".rpmsave",
        ".rej",
        ".orig",
        ".bak",
        ".swp",
        ".tmp",
    ];
    SKIP_SUFFIXES.iter().any(|s| name.ends_with(s))
}

// ──────────────────── CLI argument parsing ────────────────────

/// Parsed command-line options: only what the CLI itself says.
/// Environment defaults live in [`RuntimeConfig::from_env`]; `None`
/// means "flag not given — keep the configured value".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliOptions {
    /// `--init-dir <DIR>` value, if given.
    pub init_dir: Option<String>,
    /// `--inittab <FILE>` value, if given.
    pub inittab: Option<String>,
    /// `--mode <MODE>` value (defaults to [`BootMode::Auto`]).
    pub mode: BootMode,
    /// `--dry-run` was passed.
    pub dry_run: bool,
    /// `--boot-id <ID>` value, if given.
    pub boot_id: Option<String>,
    /// `--jobs <N>` value, if given.
    pub jobs: Option<usize>,
    /// `--log-level <LVL>` value, if given.
    pub log_level: Option<String>,
}

/// What the argument vector asks for: either a boot run with options,
/// or an early-exit informational command.
#[derive(Debug, PartialEq, Eq)]
pub enum CliCommand {
    /// Boot (or dry-run) with these options.
    Run(CliOptions),
    /// `--version`: print the bare version and exit.
    PrintVersion,
    /// `--build-id`: print the package build id and exit.
    PrintBuildId,
    /// `--info`: print full version/build/C-runtime/arch and exit.
    PrintInfo,
    /// `--help` / `-h`: print usage and exit.
    PrintHelp,
}

/// Split `--flag=value` into `("--flag", Some("value"))`; a bare
/// `--flag` yields `("--flag", None)`. A single leading `-h` (but not
/// `-hx`) is passed through unchanged so the `-h` alias keeps working.
fn split_flag_value(arg: &str) -> (&str, Option<&str>) {
    if arg.starts_with("--") {
        if let Some(eq) = arg.find('=') {
            return (&arg[..eq], Some(&arg[eq + 1..]));
        }
    }
    (arg, None)
}

/// Parse an argv slice (without the program name) into a [`CliCommand`].
///
/// Accepts both `--flag value` and `--flag=value` forms; the latter
/// previously fell into `unknown argument`, which is why scripts that
/// export `init="--jobs=4"` failed to boot. Error strings match the
/// historical `--flag requires …` wording so operator runbooks keep
/// working. Pure and allocation-light: unit tests pass vecs directly
/// instead of spawning the binary (the old parser lived in `main` and
/// had zero tests).
pub fn parse_cli_args<I, S>(args: I) -> Result<CliCommand, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let owned: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut opts = CliOptions::default();
    let mut i = 0;
    // Take the value for a flag either from `--flag=value` or from the
    // next argv element. Returns the value or the historical error.
    let take_value = |inline: Option<&str>,
                      rest: &[String],
                      flag: &str,
                      what: &str|
     -> Result<String, String> {
        if let Some(v) = inline {
            if v.is_empty() {
                return Err(format!("--{flag} requires {what}"));
            }
            return Ok(v.to_string());
        }
        rest.first()
            .cloned()
            .ok_or_else(|| format!("--{flag} requires {what}"))
    };

    while i < owned.len() {
        let arg = owned[i].clone();
        let (flag, inline) = split_flag_value(&arg);
        match flag {
            "--dry-run" => {
                if inline.is_some() {
                    return Err("--dry-run takes no value (use --dry-run)".to_string());
                }
                opts.dry_run = true;
            }
            "--init-dir" => {
                opts.init_dir = Some(take_value(inline, &owned[i + 1..], "init-dir", "a directory argument")?);
                if inline.is_none() {
                    i += 1;
                }
            }
            "--inittab" => {
                opts.inittab = Some(take_value(inline, &owned[i + 1..], "inittab", "a file argument")?);
                if inline.is_none() {
                    i += 1;
                }
            }
            "--mode" => {
                let raw = take_value(inline, &owned[i + 1..], "mode", "a mode argument")?;
                opts.mode = BootMode::parse(&raw).map_err(|e| e.to_string())?;
                if inline.is_none() {
                    i += 1;
                }
            }
            "--jobs" => {
                let raw = take_value(inline, &owned[i + 1..], "jobs", "a number")?;
                opts.jobs = Some(
                    raw.trim()
                        .parse::<usize>()
                        .map_err(|_| "--jobs must be a non-negative integer".to_string())?,
                );
                if inline.is_none() {
                    i += 1;
                }
            }
            "--log-level" => {
                opts.log_level = Some(take_value(inline, &owned[i + 1..], "log-level", "an argument")?);
                if inline.is_none() {
                    i += 1;
                }
            }
            "--boot-id" => {
                opts.boot_id = Some(take_value(inline, &owned[i + 1..], "boot-id", "an identifier argument")?);
                if inline.is_none() {
                    i += 1;
                }
            }
            "--version" => return Ok(CliCommand::PrintVersion),
            "--build-id" => return Ok(CliCommand::PrintBuildId),
            "--info" => return Ok(CliCommand::PrintInfo),
            "--help" | "-h" => return Ok(CliCommand::PrintHelp),
            unknown => {
                return Err(format!("unknown argument: {unknown} (try --help)"));
            }
        }
        i += 1;
    }
    Ok(CliCommand::Run(opts))
}

impl RuntimeConfig {
    /// Snapshot the process environment. Call once, at startup, before
    /// any thread or service exists; afterwards pass `&RuntimeConfig`
    /// down instead of reading env at use sites.
    pub fn from_env() -> Self {
        let lookup = |key: &str| std::env::var(key).ok();
        let runtime_dir = runtime_dir_for(runtime_dir_candidates_with(&lookup));
        Self::from_lookup(&lookup, &runtime_dir)
    }

    /// Pure core of [`RuntimeConfig::from_env`]: resolve every knob from
    /// an explicit lookup and an already-resolved runtime directory, so
    /// unit tests pass maps and temp paths instead of mutating
    /// process-global state (no `ENV_LOCK` needed) or probing the real
    /// filesystem.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        runtime_dir: &Path,
    ) -> Self {
        // Whitespace-only values are treated as unset: an exported but
        // empty `VELOCITY_INIT_DIR="   "` must fall back to the default,
        // not become a three-space directory that `read_dir` fails on.
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let (jobs, jobs_env_invalid) = parse_jobs(get(JOBS_ENV).as_deref());
        Self {
            init_dir: get(INIT_DIR_ENV).unwrap_or_else(|| DEFAULT_INIT_DIR.to_string()),
            inittab: get(INITTAB_ENV).unwrap_or_else(|| Inittab::DEFAULT_PATH.to_string()),
            jobs,
            jobs_env_invalid,
            getty_exe: get(GETTY_ENV).unwrap_or_else(|| DEFAULT_GETTY.to_string()),
            allow_sysv_nonpid1: parse_truthy(lookup(ALLOW_SYSV_NONPID1_ENV).as_deref()),
            socket_candidates: control_socket_candidates_with(&lookup, runtime_dir),
            journal_candidates: journal_candidates_with(&lookup, runtime_dir),
            ctl_idle_timeout: parse_idle_timeout(get(CONN_IDLE_TIMEOUT_ENV).as_deref()),
        }
    }
}

// ──────────────────────────── unit tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A lookup over a plain map: no process environment involved, so
    /// these tests are pure and need no serialisation lock.
    fn lookup_of(map: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        |key: &str| map.get(key).cloned()
    }

    fn empty() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn defaults_apply_with_an_empty_environment() {
        let map = empty();
        let cfg = RuntimeConfig::from_lookup(lookup_of(&map), Path::new("/tmp/velocity-test-rt"));
        assert_eq!(cfg.init_dir, DEFAULT_INIT_DIR);
        assert_eq!(cfg.inittab, Inittab::DEFAULT_PATH);
        assert_eq!(cfg.jobs, 0);
        assert_eq!(cfg.getty_exe, DEFAULT_GETTY);
        assert!(!cfg.allow_sysv_nonpid1);
        assert_eq!(
            cfg.socket_candidates[0],
            PathBuf::from(crate::paths::DEFAULT_SOCKET_PATH)
        );
        assert_eq!(
            cfg.journal_candidates[0],
            PathBuf::from(crate::paths::DEFAULT_JOURNAL_PATH)
        );
        assert_eq!(cfg.ctl_idle_timeout, crate::control_socket::CONN_IDLE_TIMEOUT);
    }

    #[test]
    fn env_overrides_are_honoured() {
        let map: HashMap<String, String> = [
            (INIT_DIR_ENV, "/opt/init.d"),
            (INITTAB_ENV, "/tmp/inittab"),
            (JOBS_ENV, "4"),
            (GETTY_ENV, "/bin/fakegetty"),
            (ALLOW_SYSV_NONPID1_ENV, "yes"),
            ("VELOCITY_INIT_SOCKET", "/tmp/pinned.sock"),
            ("VELOCITY_JOURNAL", "/tmp/pinned-journal"),
            (CONN_IDLE_TIMEOUT_ENV, "3"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cfg = RuntimeConfig::from_lookup(lookup_of(&map), Path::new("/tmp/velocity-test-rt"));
        assert_eq!(cfg.init_dir, "/opt/init.d");
        assert_eq!(cfg.inittab, "/tmp/inittab");
        assert_eq!(cfg.jobs, 4);
        assert_eq!(cfg.getty_exe, "/bin/fakegetty");
        assert!(cfg.allow_sysv_nonpid1);
        assert_eq!(cfg.socket_candidates, vec![PathBuf::from("/tmp/pinned.sock")]);
        assert_eq!(
            cfg.journal_candidates,
            vec![PathBuf::from("/tmp/pinned-journal")]
        );
        assert_eq!(cfg.ctl_idle_timeout, Duration::from_secs(3));
    }

    #[test]
    fn empty_and_malformed_values_fall_back_to_defaults() {
        let map: HashMap<String, String> = [
            (INIT_DIR_ENV, ""),
            (JOBS_ENV, "not-a-number"),
            (ALLOW_SYSV_NONPID1_ENV, "0"),
            (CONN_IDLE_TIMEOUT_ENV, "0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cfg = RuntimeConfig::from_lookup(lookup_of(&map), Path::new("/tmp/velocity-test-rt"));
        assert_eq!(cfg.init_dir, DEFAULT_INIT_DIR);
        assert_eq!(cfg.jobs, 0);
        assert!(cfg.jobs_env_invalid, "malformed jobs must be flagged for a warning");
        assert!(!cfg.allow_sysv_nonpid1);
        assert_eq!(cfg.ctl_idle_timeout, crate::control_socket::CONN_IDLE_TIMEOUT);
    }

    #[test]
    fn truthy_parsing_is_case_insensitive_and_trims() {
        for v in ["1", "yes", "YES", "Yes", "y", "Y", "true", "TRUE", "True", "on", "ON", "  yes  "] {
            assert!(parse_truthy(Some(v)), "{v:?} must be truthy");
        }
        for v in [None, Some(""), Some("0"), Some("no"), Some("NO"), Some("false"), Some("off"), Some("garbage"), Some("   ")] {
            assert!(!parse_truthy(v), "{v:?} must be falsy");
        }
    }

    #[test]
    fn jobs_parsing_trims_and_flags_invalid() {
        assert_eq!(parse_jobs(None), (0, false));
        assert_eq!(parse_jobs(Some("")), (0, false));
        assert_eq!(parse_jobs(Some("   ")), (0, false));
        assert_eq!(parse_jobs(Some("4")), (4, false));
        assert_eq!(parse_jobs(Some("  4  ")), (4, false));
        assert_eq!(parse_jobs(Some("0")), (0, false));
        assert_eq!(parse_jobs(Some("not-a-number")), (0, true));
        assert_eq!(parse_jobs(Some("-1")), (0, true));
        assert_eq!(parse_jobs(Some("4.5")), (0, true));
    }

    #[test]
    fn whitespace_only_env_falls_back_to_defaults() {
        let map: HashMap<String, String> = [
            (INIT_DIR_ENV, "   "),
            (INITTAB_ENV, "  "),
            (JOBS_ENV, "  6  "),
            (GETTY_ENV, "  "),
            (ALLOW_SYSV_NONPID1_ENV, "  YES  "),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cfg = RuntimeConfig::from_lookup(lookup_of(&map), Path::new("/tmp/velocity-test-rt"));
        assert_eq!(cfg.init_dir, DEFAULT_INIT_DIR);
        assert_eq!(cfg.inittab, Inittab::DEFAULT_PATH);
        assert_eq!(cfg.jobs, 6);
        assert!(!cfg.jobs_env_invalid);
        assert_eq!(cfg.getty_exe, DEFAULT_GETTY);
        assert!(cfg.allow_sysv_nonpid1);
    }

    #[test]
    fn init_entry_skip_list_covers_packaging_debris() {
        // Historical behaviour.
        assert!(should_skip_init_entry(""));
        assert!(should_skip_init_entry(".hidden"));
        assert!(should_skip_init_entry("README"));
        assert!(should_skip_init_entry("README.Debian"));
        // New: packaging / editor debris must not become services.
        for name in [
            "ssh.dpkg-dist",
            "ssh.dpkg-old",
            "ssh.dpkg-new",
            "ssh.rpmnew",
            "ssh.rpmsave",
            "ssh.rej",
            "ssh.orig",
            "ssh.bak",
            "ssh.swp",
            "ssh.tmp",
            "ssh~",
            "#ssh#",
            "#ssh",
        ] {
            assert!(should_skip_init_entry(name), "{name} must be skipped");
        }
        // Real scripts still pass.
        for name in ["ssh", "cron", "network", "getty@tty1", "my-service", "S01boot"] {
            assert!(!should_skip_init_entry(name), "{name} must be kept");
        }
    }

    #[test]
    fn cli_parses_space_separated_flags() {
        let cmd = parse_cli_args([
            "--init-dir",
            "/opt/init.d",
            "--mode",
            "sysv",
            "--jobs",
            "4",
            "--dry-run",
            "--boot-id",
            "test-123",
            "--log-level",
            "debug",
            "--inittab",
            "/tmp/inittab",
        ])
        .expect("parse");
        let CliCommand::Run(opts) = cmd else {
            panic!("expected Run");
        };
        assert_eq!(opts.init_dir.as_deref(), Some("/opt/init.d"));
        assert_eq!(opts.inittab.as_deref(), Some("/tmp/inittab"));
        assert_eq!(opts.mode, BootMode::Sysv);
        assert!(opts.dry_run);
        assert_eq!(opts.boot_id.as_deref(), Some("test-123"));
        assert_eq!(opts.jobs, Some(4));
        assert_eq!(opts.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn cli_parses_equals_form() {
        let cmd = parse_cli_args([
            "--init-dir=/opt/init.d",
            "--mode=native",
            "--jobs=2",
            "--log-level=debug",
            "--boot-id=abc",
            "--inittab=/tmp/inittab",
        ])
        .expect("parse");
        let CliCommand::Run(opts) = cmd else {
            panic!("expected Run");
        };
        assert_eq!(opts.init_dir.as_deref(), Some("/opt/init.d"));
        assert_eq!(opts.mode, BootMode::Native);
        assert_eq!(opts.jobs, Some(2));
        assert_eq!(opts.log_level.as_deref(), Some("debug"));
        assert_eq!(opts.boot_id.as_deref(), Some("abc"));
        assert_eq!(opts.inittab.as_deref(), Some("/tmp/inittab"));
    }

    #[test]
    fn cli_rejects_unknown_and_missing_values() {
        assert!(parse_cli_args(["--bogus"]).is_err());
        assert!(parse_cli_args(["--init-dir"]).is_err());
        assert!(parse_cli_args(["--init-dir="]).is_err());
        assert!(parse_cli_args(["--jobs", "not-a-number"]).is_err());
        assert!(parse_cli_args(["--jobs=-1"]).is_err());
        assert!(parse_cli_args(["--mode", "bogus"]).is_err());
        assert!(parse_cli_args(["--dry-run=x"]).is_err());
    }

    #[test]
    fn cli_informational_flags_win() {
        assert_eq!(parse_cli_args(["--version"]), Ok(CliCommand::PrintVersion));
        assert_eq!(parse_cli_args(["--build-id"]), Ok(CliCommand::PrintBuildId));
        assert_eq!(parse_cli_args(["--info"]), Ok(CliCommand::PrintInfo));
        assert_eq!(parse_cli_args(["--help"]), Ok(CliCommand::PrintHelp));
        assert_eq!(parse_cli_args(["-h"]), Ok(CliCommand::PrintHelp));
    }

    #[test]
    fn cli_mode_aliases_match_bootmode_parse() {
        let mode_of = |args: &[&str]| match parse_cli_args(args).expect("parse") {
            CliCommand::Run(o) => o.mode,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(mode_of(&["--mode", "auto"]), BootMode::Auto);
        assert_eq!(mode_of(&["--mode=lsb"]), BootMode::Native);
        assert_eq!(mode_of(&["--mode=sysvinit"]), BootMode::Sysv);
    }
}
