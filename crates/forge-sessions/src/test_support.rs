//! The records a log site emitted, for a test that has to read them back.
//!
//! A site's fields are its contract with whoever reads the log, and nothing
//! else in a test can see them: assertions about what a view drew say nothing
//! about what a failure was recorded as, or whether it was recorded at all.

use std::sync::{Arc, Mutex};

/// Every record a site emitted while the capture was installed.
#[derive(Clone, Default)]
struct Caught(Arc<Mutex<Vec<(tracing::Level, String)>>>);

/// Every field the record carried, as `name=value`, so a test reads the
/// `event_name` and the seat off the site that emitted it.
#[derive(Default)]
struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = std::fmt::write(&mut self.0, format_args!("{}={value:?}", field.name()));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Caught {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().expect("capture").push((*event.metadata().level(), fields.0));
    }
}

/// Everything `emit` logs while it runs, as (level, `name=value` fields).
pub fn logged(emit: impl FnOnce()) -> Vec<(tracing::Level, String)> {
    use tracing_subscriber::layer::SubscriberExt;

    let caught = Caught::default();
    let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    emit();
    caught.0.lock().expect("capture").clone()
}
