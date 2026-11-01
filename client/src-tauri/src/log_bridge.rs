//! The client's tracing side: the `log` facade in `tauri_plugin_log`, and the
//! `tracing` events `forge-capture` writes, are two sinks unless something
//! joins them. This forwards every kept tracing event into the facade, so a
//! capture guard's warning lands in the client's own `forge.log`.

use tauri_plugin_log::log;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

/// Install the bridge as the process's tracing subscriber, once.
pub(crate) fn install() {
    let subscriber = tracing_subscriber::registry().with(Bridge);
    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        log::warn!("the tracing side of the client log was not installed: {error}");
    }
}

struct Bridge;

impl<S: Subscriber> Layer<S> for Bridge {
    /// The shipped policy decides what reaches the file, so an event the
    /// facade would drop is never formatted here either.
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _context: Context<'_, S>) -> bool {
        log_level(*metadata.level()).is_some_and(|level| crate::keeps(level, metadata.target()))
    }

    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let Some(level) = log_level(*metadata.level()) else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        // Bound before the builder: an `Arguments` borrowing a temporary would
        // die with the statement, and the record is logged after it.
        let rendered = fields.finish();
        let args = format_args!("{rendered}");
        let record = log::Record::builder()
            .args(args)
            .level(level)
            .target(metadata.target())
            .module_path(metadata.module_path())
            .file(metadata.file())
            .line(metadata.line())
            .build();
        log::logger().log(&record);
    }
}

fn log_level(level: Level) -> Option<log::Level> {
    match level {
        Level::ERROR => Some(log::Level::Error),
        Level::WARN => Some(log::Level::Warn),
        Level::INFO => Some(log::Level::Info),
        Level::DEBUG => Some(log::Level::Debug),
        Level::TRACE => Some(log::Level::Trace),
    }
}

/// A tracing event's fields, rendered as `message key=value ...`.
#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
}

impl Fields {
    fn finish(self) -> String {
        self.message + &self.rest
    }

    fn push(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.message);
        } else {
            self.rest.push(' ');
            self.rest.push_str(field.name());
            self.rest.push('=');
            self.rest.push_str(value);
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        self.push(field, &rendered);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_capture;

    fn through_bridge(event: impl FnOnce()) -> Vec<log_capture::Line> {
        let subscriber = tracing_subscriber::registry().with(Bridge);
        tracing::subscriber::with_default(subscriber, event);
        log_capture::records()
    }

    /// The capture crate's guard lines are warnings, and this is the seam that
    /// carries one into the client's log with its own target and fields.
    #[test]
    fn a_capture_guard_warning_reaches_the_client_log() {
        let _held = log_capture::capture();

        let lines = through_bridge(|| {
            tracing::warn!(
                target: "forge_capture::stream",
                event_name = "input_callback_panicked",
                input = %"MacBook Pro Microphone",
                "a panic in the audio callback was caught before it could cross the FFI boundary",
            );
        });

        assert_eq!(lines.len(), 1, "the bridge writes one line per kept event");
        assert_eq!(lines[0].level, log::Level::Warn, "the guard's level is kept as it was");
        assert!(
            lines[0].message.starts_with("a panic in the audio callback was caught"),
            "the event's own message leads the line: {}",
            lines[0].message
        );
        assert!(
            lines[0].message.contains("event_name=input_callback_panicked"),
            "a reader can find which guard fired: {}",
            lines[0].message
        );
        assert!(
            lines[0].message.contains("input=MacBook Pro Microphone"),
            "and which device it was: {}",
            lines[0].message
        );
    }

    /// **The shell's own install is what carries these lines** (#2730): the
    /// tests above build the layer directly, so a shell that stopped calling
    /// `install()` would leave them green while `forge.log` lost every capture
    /// warning. nextest runs each test in its own process, so the global set
    /// is safe here.
    #[test]
    fn the_install_routes_a_capture_warning_into_the_client_log() {
        let _held = log_capture::capture();
        install();
        tracing::warn!(
            target: "forge_capture::stream",
            event_name = "capture_thread_panicked",
            "the capture thread panicked",
        );
        let lines = log_capture::records();
        assert_eq!(lines.len(), 1, "install() put the bridge on the global subscriber");
        assert!(
            lines[0].message.contains("event_name=capture_thread_panicked"),
            "the installed bridge carries the guard's own fields: {}",
            lines[0].message
        );
    }

    /// The bridge does not bypass the facade's filter: a debug event from a
    /// target the client keeps at info stays out of the file.
    #[test]
    fn a_tracing_debug_outside_the_named_targets_stays_out_of_the_log() {
        let _held = log_capture::capture();

        let lines = through_bridge(|| {
            tracing::debug!(target: "forge_capture::stream", "a session's own chatter");
        });

        assert_eq!(lines.len(), 0, "the shipped filter drops it");
    }
}
