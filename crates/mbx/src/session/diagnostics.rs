use super::request_agent;
use mbx_cache_core::{AgentRequest, AgentResponse};
use std::sync::OnceLock;

/// Write to stderr without failing the build when the pipe is closed.
pub(crate) fn note(message: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{message}");
}

/// Whether this process's stderr belongs to the compiler it stands in for.
///
/// Set once at cc- or rustc-shim entry. Build scripts read an intercepted
/// compiler's stderr as part of its answer: cc-rs marks a probed flag unsupported
/// the moment anything lands there. Cargo also saves rustc stderr in its
/// fingerprints and replays it even when no compiler runs. Shim diagnostics
/// must therefore travel through the live session instead.
static STDERR_RESERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Declare that this process replays compiler output on stderr and must not
/// mix its own diagnostics into it.
pub(crate) fn reserve_stderr_for_compiler() {
    STDERR_RESERVED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Report a shim diagnostic without polluting a reserved stderr.
///
/// Delivered to the session's agent, which prints it from the process that
/// owns the build. When the agent cannot take it, the message falls back to
/// this process's stderr only where that stream is not the compiler's --
/// losing a diagnostic costs a little visibility, while poisoning a configure
/// probe costs the build its cache keys.
pub(crate) fn report_shim_warning(message: &str) {
    report_shim_diagnostic(Severity::Warning, message);
}

/// Report a routine shim warning only when the caller's debug filter requests
/// it. Rustc shims run before the process-wide logger is initialized, so this
/// checks the same filter without installing a logger or writing to stderr.
pub(crate) fn report_shim_warning_on_debug(target: &'static str, message: &str) {
    let Some(filter) = debug_filter() else {
        return;
    };
    let matches = filter.matches(
        &log::Record::builder()
            .args(format_args!("{message}"))
            .level(log::Level::Debug)
            .target(target)
            .build(),
    );
    if matches {
        report_shim_warning(message);
    }
}

/// Parse the shim's debug filter once, silently disabling this optional
/// diagnostic when the filter is malformed. The main mbx process can report a
/// malformed filter on its own stderr; a rustc shim must never do so because
/// Cargo treats compiler stderr as part of its fingerprint.
fn debug_filter() -> Option<&'static env_filter::Filter> {
    static FILTER: OnceLock<Option<env_filter::Filter>> = OnceLock::new();
    FILTER
        .get_or_init(|| {
            let value = std::env::var("MBX_LOG").unwrap_or_else(|_| "info".to_string());
            let mut builder = env_filter::Builder::new();
            if builder.try_parse(&value).is_err() {
                None
            } else {
                Some(builder.build())
            }
        })
        .as_ref()
}

/// Report a fatal shim diagnostic without losing it when transport fails.
///
/// The session retains the error's severity when it displays and emits it.
/// The local fallback keeps the fatal reason visible when delivery fails,
/// including when an older agent does not understand the error request.
pub(crate) fn report_shim_error(message: &str) {
    report_shim_diagnostic(Severity::Error, message);
}

#[derive(Clone, Copy)]
enum Severity {
    Warning,
    Error,
}

fn report_shim_diagnostic(severity: Severity, message: &str) {
    let mut message = message.replace(['\n', '\r'], "; ");
    // Stay under the agent's acceptance limit rather than losing the whole
    // diagnostic to it; the start of an error chain names the failure.
    if message.len() > 2048 {
        let end = (0..=2048).rfind(|&index| message.is_char_boundary(index));
        message.truncate(end.unwrap_or_default());
        message.push_str("...");
    }
    let (label, request) = match severity {
        Severity::Warning => (
            "warning",
            AgentRequest::RecordWarning {
                message: message.clone(),
            },
        ),
        Severity::Error => (
            "error",
            AgentRequest::RecordError {
                message: message.clone(),
            },
        ),
    };
    let response = request_agent(&[request])
        .ok()
        .and_then(|responses| responses.into_iter().next());
    let delivered = matches!(
        (severity, response),
        (Severity::Warning, Some(AgentResponse::WarningRecorded))
            | (Severity::Error, Some(AgentResponse::ErrorRecorded))
    );
    if !delivered
        && (matches!(severity, Severity::Error)
            || !STDERR_RESERVED.load(std::sync::atomic::Ordering::Relaxed))
    {
        note(&format!("mbx[{label}]: {message}"));
    }
}
