use crate::{causality, message, priority::Priority};

/// An item to be sent in a streaming request.
///
/// Items can have their own priority and dependencies, allowing fine-grained
/// control over ordering and scheduling within the stream.
#[derive(Clone)]
pub struct Item {
    #[expect(dead_code)]
    message: message::Message,
    priority: Option<Priority>,
    dependency: Option<causality::Dependency>,
}

impl Item {
    /// Creates a new item with the given message.
    ///
    /// # Arguments
    ///
    /// * `message` - The message payload for this item
    pub fn new(message: message::Message) -> Self {
        Self {
            message,
            priority: None,
            dependency: None,
        }
    }

    /// Sets the priority for this specific item.
    ///
    /// If not set, the item inherits the stream's priority.
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Adds a causality dependency for this specific item.
    ///
    /// This item will wait for the dependency to be satisfied before being sent.
    pub fn depends_on(mut self, dependency: causality::Dependency) -> Self {
        self.dependency = Some(dependency);
        self
    }
}
