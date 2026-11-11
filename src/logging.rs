use std::path::Path;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Bounded queue for the non-blocking file writer. The tracing-appender
/// default is 128,000 lines; a burst of warn/error logs (e.g. a broken source
/// retried every few seconds) would otherwise let the queue grow to tens of
/// megabytes. 4,096 lines is far above normal burst volume while keeping the
/// worst-case footprint around a few hundred KB; when it fills, excess lines
/// are dropped (lossy mode) instead of growing the queue or blocking callers.
const LOG_BUFFER_LINES: usize = 4096;

/// Initialize tracing with console + daily rotating file output.
/// Returns a guard that must be held for the lifetime of the program.
pub fn init_logging(log_dir: &Path) -> anyhow::Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;

    let file_appender = tracing_appender::rolling::daily(log_dir, "rss-bot.log");
    let (non_blocking, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(LOG_BUFFER_LINES)
        .finish(file_appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .compact(),
        )
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true),
        )
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to initialize tracing subscriber: {}", e))?;

    Ok(guard)
}
