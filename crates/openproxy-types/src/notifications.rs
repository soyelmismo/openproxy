use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationEvent {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

pub static NOTIFICATION_PUBLISHER: once_cell::sync::OnceCell<
    Box<dyn Fn(NotificationEvent) + Send + Sync>,
> = once_cell::sync::OnceCell::new();

pub fn publish_notification(event: NotificationEvent) {
    if let Some(publisher) = NOTIFICATION_PUBLISHER.get() {
        publisher(event);
    }
}
