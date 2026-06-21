use crate::cli::{GlobalArgs, LogLevelArg};
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Install the file (and optionally stderr) tracing subscriber when `--log-level`
/// asks for it. Returns the appender's [`WorkerGuard`], which the caller keeps
/// alive for the whole process so buffered logs flush on exit. Returns `None`
/// (installs nothing) when logging is off or `$HOME` is unset — the `tracing`
/// macros across the crates then stay no-ops with zero overhead.
pub fn init_logging(global: &GlobalArgs) -> Option<WorkerGuard> {
    let level = global.log_level?;
    if level == LogLevelArg::Off {
        return None;
    }
    let dir = logs_dir()?;
    std::fs::create_dir_all(&dir).ok()?;

    // A single accumulating file (no rotation), non-blocking so the appender
    // thread never stalls the agent/TUI loop.
    let (file_writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::never(&dir, "stepper.log"));
    let file_layer = fmt::layer().with_writer(file_writer).with_ansi(false);

    // The TUI owns stdout's inline viewport, so stderr logs would corrupt it:
    // only a headless run (`-p`) with `--print-logs` also streams to stderr.
    let stderr_layer = (global.print.is_some() && global.print_logs).then(|| fmt::layer().with_writer(std::io::stderr));

    // `--log-level` sets the global level, but a non-empty `RUST_LOG` wins so power
    // users can scope it per crate — e.g. `RUST_LOG=stepper_core=debug,h2=off` to
    // cut the noisy h2/hyper debug spam that a bare `debug` would otherwise pull in.
    // An empty/whitespace `RUST_LOG=` must NOT silently override `--log-level` (an
    // empty directive is parsed as ERROR-only), so treat it as unset.
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) if !v.trim().is_empty() => EnvFilter::new(v),
        _ => EnvFilter::new(level.as_str()),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();
    Some(guard)
}

/// `~/.stepper/logs`, or `None` when `$HOME` is unset.
fn logs_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".stepper").join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_dir_is_under_home_dot_stepper() {
        // Read-only on $HOME (no env mutation, so no test race). Skip if unset.
        if std::env::var_os("HOME").is_some() {
            let dir = logs_dir().expect("HOME is set");
            assert!(dir.ends_with("logs"));
            assert!(dir.parent().is_some_and(|p| p.ends_with(".stepper")), "got {dir:?}");
        }
    }
}
