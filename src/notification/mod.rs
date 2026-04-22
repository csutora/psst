#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;

/// a notification to display to the user
pub struct Notification {
    /// deterministic identifier (usually room_id), used for replacement and dismissal
    pub tag: String,
    /// main title (room name or sender for direct messages)
    pub title: String,
    /// optional subtitle (sender name in group rooms)
    pub subtitle: Option<String>,
    /// message body
    pub body: String,
    /// sound name to play, or None for silent
    /// on macos this maps to a sound in /System/Library/Sounds/ (e.g. "Blow")
    /// on linux it's an xdg sound name (e.g. "message-new-instant")
    pub sound: Option<String>,
    /// thread identifier for grouping (usually room_id)
    pub thread_id: String,
}

/// platform notification backend
pub trait Notifier: Send + Sync {
    /// post or update a notification
    fn send(&self, notification: &Notification) -> anyhow::Result<()>;
    /// dismiss a previously posted notification by tag
    fn dismiss(&self, tag: &str) -> anyhow::Result<()>;
}

/// create the platform-appropriate notifier
pub fn create_notifier() -> anyhow::Result<Box<dyn Notifier>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacosNotifier::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxNotifier::new()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("notifications not supported on this platform")
    }
}

/// fire test notifications to verify the backend works
pub async fn test() -> anyhow::Result<()> {
    let notifier = create_notifier()?;

    let sound_name = if cfg!(target_os = "macos") { "Blow" } else { "message-new-instant" };

    eprintln!("sending silent test notification...");
    notifier.send(&Notification {
        tag: "psst-test-silent".to_string(),
        title: "psst".to_string(),
        subtitle: None,
        body: "silent test notification".to_string(),
        sound: None,
        thread_id: "psst-test".to_string(),
    })?;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    eprintln!("sending noisy test notification...");
    notifier.send(&Notification {
        tag: "psst-test-noisy".to_string(),
        title: "psst".to_string(),
        subtitle: None,
        body: "noisy test notification".to_string(),
        sound: Some(sound_name.to_string()),
        thread_id: "psst-test".to_string(),
    })?;

    eprintln!("done. check your notification center.");
    Ok(())
}
