use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationEvent {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

use std::sync::OnceLock;

pub static NOTIFICATION_PUBLISHER: OnceLock<Box<dyn Fn(NotificationEvent) + Send + Sync>> =
    OnceLock::new();

pub fn publish_notification(event: NotificationEvent) {
    if let Some(publisher) = NOTIFICATION_PUBLISHER.get() {
        publisher(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_publish_notification() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        // Since NOTIFICATION_PUBLISHER is a static OnceLock, set it if not set yet.
        let _ = NOTIFICATION_PUBLISHER.set(Box::new(move |event| {
            called_clone.store(true, Ordering::SeqCst);
            assert_eq!(event.id, 42);
            assert_eq!(event.kind, "test_event");
        }));

        let event = NotificationEvent {
            id: 42,
            kind: "test_event".to_string(),
            payload: serde_json::json!({"key": "value"}),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        };

        publish_notification(event);

        assert!(called.load(Ordering::SeqCst));
    }
}
