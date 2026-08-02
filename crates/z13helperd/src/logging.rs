//! Tiny dependency-free JSON tracing subscriber for journald/stderr.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);

pub fn init() {
    let _ = tracing::subscriber::set_global_default(JsonSubscriber);
}

struct JsonSubscriber;

impl Subscriber for JsonSubscriber {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(NEXT_SPAN.fetch_add(1, Ordering::Relaxed).max(1))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let record = serde_json::json!({
            "level": event.metadata().level().as_str(),
            "target": event.metadata().target(),
            "fields": visitor.fields,
        });
        let mut stderr = std::io::stderr().lock();
        let _ = serde_json::to_writer(&mut stderr, &record);
        let _ = stderr.write_all(b"\n");
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[derive(Default)]
struct JsonVisitor {
    fields: BTreeMap<String, Value>,
}

impl Visit for JsonVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().into(), format!("{value:?}").into());
    }
}
