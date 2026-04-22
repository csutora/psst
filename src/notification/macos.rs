use anyhow::Context;
use objc2_foundation::{NSArray, NSString};
use objc2_user_notifications::*;

use super::{Notification, Notifier};

/// ensure the process is running from within a .app bundle
///
/// UNUserNotificationCenter requires a proper .app bundle launched with full
/// app context. if we're a bare binary, this creates a .app wrapper next to
/// the executable and relaunches via `open`, which registers the app with
/// the window server and notification system
pub fn ensure_app_bundle() -> anyhow::Result<()> {
    // if we're already inside a .app bundle, nothing to do
    let exe = std::env::current_exe().context("failed to get current executable path")?;
    if exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        return Ok(());
    }

    let app_dir = exe.with_extension("app");
    let contents_dir = app_dir.join("Contents");
    let macos_dir = contents_dir.join("MacOS");
    let binary_name = exe
        .file_name()
        .context("executable has no file name")?;
    let link_path = macos_dir.join(binary_name);

    std::fs::create_dir_all(&macos_dir)
        .with_context(|| format!("failed to create {}", macos_dir.display()))?;

    // always write info.plist (keeps it in sync)
    let plist_path = contents_dir.join("Info.plist");
    std::fs::write(&plist_path, include_str!("../../Info.plist"))
        .context("failed to write Info.plist into app bundle")?;

    // hard-link the binary into the .app (not symlink, gets resolved)
    if link_path.exists() {
        let _ = std::fs::remove_file(&link_path);
    }
    std::fs::hard_link(&exe, &link_path)
        .with_context(|| format!("failed to link binary into app bundle"))?;

    // ad-hoc codesign so the system treats it as a known app for
    // notification authorization
    let _ = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app_dir)
        .output();

    // launch via `open` to get proper app context (window server
    // registration, notification system access)
    let status = std::process::Command::new("open")
        .arg(&app_dir)
        .arg("--args")
        .args(std::env::args().skip(1))
        .status()
        .context("failed to launch app bundle via `open`")?;

    if !status.success() {
        anyhow::bail!("failed to launch app bundle (exit code {:?})", status.code());
    }

    eprintln!("daemon launched via {}", app_dir.display());
    eprintln!("stop with: pkill -f psst.app");
    std::process::exit(0);
}

pub struct MacosNotifier {
    center: objc2::rc::Retained<UNUserNotificationCenter>,
}

// UNUserNotificationCenter is thread-safe for all public methods
unsafe impl Send for MacosNotifier {}
unsafe impl Sync for MacosNotifier {}

impl MacosNotifier {
    pub fn new() -> anyhow::Result<Self> {
        let center = UNUserNotificationCenter::currentNotificationCenter();

        // request authorization (alert + sound), the completion handler
        // receives the result asynchronously; we block briefly to log it
        let (tx, rx) = std::sync::mpsc::channel();
        let handler = block2::RcBlock::new(move |granted: objc2::runtime::Bool, _error: *mut objc2_foundation::NSError| {
            let _ = tx.send(granted.as_bool());
        });

        let options = UNAuthorizationOptions(
            UNAuthorizationOptions::Alert.0
                | UNAuthorizationOptions::Sound.0
                | UNAuthorizationOptions::Badge.0,
        );

        center.requestAuthorizationWithOptions_completionHandler(options, &handler);

        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(true) => tracing::info!("notification authorization granted"),
            Ok(false) => {
                tracing::warn!("notification authorization denied");
                eprintln!("notification permission denied.");
                eprintln!("enable in: system settings > notifications > psst");
            }
            Err(_) => tracing::warn!("notification authorization request timed out"),
        }

        Ok(Self { center })
    }
}

impl Notifier for MacosNotifier {
    fn send(&self, notification: &Notification) -> anyhow::Result<()> {
        let content = UNMutableNotificationContent::new();

        content.setTitle(&NSString::from_str(&notification.title));

        if let Some(ref subtitle) = notification.subtitle {
            content.setSubtitle(&NSString::from_str(subtitle));
        }

        content.setBody(&NSString::from_str(&notification.body));
        content.setThreadIdentifier(&NSString::from_str(&notification.thread_id));

        if let Some(name) = &notification.sound {
            // titlecase so "blow"/"BLOW"/"Blow" all work
            let titled = titlecase(name);
            let aiff = format!("{titled}.aiff");
            let sound = UNNotificationSound::soundNamed(&NSString::from_str(&aiff));
            content.setSound(Some(&sound));
        }

        // identifier = tag, so re-posting with the same tag replaces the notification
        // trigger = None means deliver immediately
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&notification.tag),
            &content,
            None,
        );

        self.center
            .addNotificationRequest_withCompletionHandler(&request, None);

        tracing::debug!(tag = %notification.tag, "notification sent");
        Ok(())
    }

    fn dismiss(&self, tag: &str) -> anyhow::Result<()> {
        let id = NSString::from_str(tag);
        let array = NSArray::from_retained_slice(&[id]);

        self.center
            .removeDeliveredNotificationsWithIdentifiers(&array);

        tracing::debug!(tag, "notification dismissed");
        Ok(())
    }
}

fn titlecase(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => {
            let rest: String = chars.as_str().to_lowercase();
            c.to_ascii_uppercase().to_string() + rest.as_str()
        }
        None => String::new(),
    }
}
