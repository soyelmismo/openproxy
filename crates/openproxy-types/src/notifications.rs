use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationEvent {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

use std::sync::OnceLock;

pub static NOTIFICATION_PUBLISHER: OnceLock<
    Box<dyn Fn(NotificationEvent) + Send + Sync>,
> = OnceLock::new();

pub fn publish_notification(event: NotificationEvent) {
    if let Some(publisher) = NOTIFICATION_PUBLISHER.get() {
        publisher(event);
    }
}
