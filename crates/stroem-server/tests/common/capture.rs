//! Capture `tracing` output in a test: every span field (at creation and on
//! `record`) and every formatted event, so a test can assert that a value
//! never reached the server log. `set_default` is thread-local: use it on a
//! `current_thread` runtime (the `#[tokio::test]` default) and keep the guard
//! alive across the awaited calls. Work on other threads (`spawn_blocking`)
//! is not captured.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
pub struct Captured {
    fields: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<u8>>>,
}

impl Captured {
    /// Every captured span field (`name=value`) and event line.
    pub fn text(&self) -> String {
        let mut out = self.fields.lock().unwrap().join("\n");
        out.push('\n');
        out.push_str(&String::from_utf8_lossy(&self.events.lock().unwrap()));
        out
    }

    /// The captured span fields called `name`, as `name=value`.
    pub fn span_fields(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name}=");
        self.fields
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f.starts_with(&prefix))
            .cloned()
            .collect()
    }
}

struct Visitor<'a>(&'a mut Vec<String>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={:?}", field.name(), value));
    }
}

struct FieldLayer(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FieldLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        attrs.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }

    fn on_record(
        &self,
        _id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        values.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }
}

#[derive(Clone)]
struct Writer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
    type Writer = Writer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Start capturing on this thread until the guard drops.
pub fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry()
        .with(FieldLayer(captured.fields.clone()))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(Writer(captured.events.clone()))
                .with_ansi(false),
        );
    let guard = tracing::subscriber::set_default(subscriber);
    (captured, guard)
}
