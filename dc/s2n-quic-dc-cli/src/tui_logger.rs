// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::shared_state::SharedState;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    Layer,
};

/// A tracing layer that captures log events and sends them to SharedState for TUI display
pub struct TuiLayer {
    state: SharedState,
}

impl TuiLayer {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

impl<S> Layer<S> for TuiLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Format the event as a string
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);

        let level = event.metadata().level();
        let target = event.metadata().target();
        let message = visitor.message;

        let log_line = format!("[{}] {}: {}", level, target, message);
        self.state.add_log(log_line);
    }
}

#[derive(Default)]
struct LogVisitor {
    message: String,
}

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
            // Remove quotes if present
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        }
    }
}

/// Initialize tracing for TUI mode - logs go to SharedState
pub fn init_tui_tracing(state: SharedState) {
    let layer = TuiLayer::new(state);
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}

/// Initialize tracing for non-TUI mode - logs go to stdout
pub fn init_normal_tracing() {
    tracing_subscriber::fmt::init();
}
