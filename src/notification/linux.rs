use std::collections::HashMap;
use std::sync::Mutex;

use super::{Notification, Notifier};

pub struct LinuxNotifier {
    /// track notification server ids for replacement/dismissal: tag -> id
    active: Mutex<HashMap<String, u32>>,
}

impl LinuxNotifier {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
        }
    }
}

impl Notifier for LinuxNotifier {
    fn send(&self, notification: &Notification) -> anyhow::Result<()> {
        let mut n = notify_rust::Notification::new();
        n.appname("psst");
        n.summary(&notification.title);
        n.body(&notification.body);

        if let Some(name) = &notification.sound {
            n.sound_name(name);
        }

        // reuse existing id to replace an active notification for the same room
        let mut active = self.active.lock().unwrap();
        if let Some(&existing_id) = active.get(&notification.tag) {
            n.id(existing_id);
        }

        let handle = n
            .show()
            .map_err(|e| anyhow::anyhow!("failed to show notification: {e}"))?;

        let id = handle.id();
        active.insert(notification.tag.clone(), id);

        tracing::debug!(tag = %notification.tag, id, "notification sent");
        Ok(())
    }

    fn dismiss(&self, tag: &str) -> anyhow::Result<()> {
        let mut active = self.active.lock().unwrap();
        if let Some(id) = active.remove(tag) {
            // close by re-showing with the same id and immediately closing the handle
            let handle = notify_rust::Notification::new()
                .appname("psst")
                .id(id)
                .show()
                .map_err(|e| anyhow::anyhow!("failed to dismiss notification: {e}"))?;
            handle.close();
            tracing::debug!(tag, id, "notification dismissed");
        }
        Ok(())
    }
}
