//! Alert channels (M6b).

use super::types::{AlertRecord, AlertTrigger, ChannelKind};
use super::AlertChannel;
use crate::storage::SqliteStore;
use tauri::Emitter;

#[cfg(target_os = "macos")]
mod macos_notification {
    use block2::DynBlock;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AnyThread};
    use objc2_foundation::{NSObject, NSObjectProtocol};
    use objc2_user_notifications::{
        UNNotification, UNNotificationPresentationOptions, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };
    use std::sync::Once;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and this class has
        // no ivars or Drop implementation.
        #[unsafe(super(NSObject))]
        struct IctNotificationDelegate;

        // SAFETY: NSObjectProtocol has no additional safety requirements.
        unsafe impl NSObjectProtocol for IctNotificationDelegate {}

        // SAFETY: The method signature matches UNUserNotificationCenterDelegate.
        unsafe impl UNUserNotificationCenterDelegate for IctNotificationDelegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                tracing::debug!("macOS notification delivered while app is in foreground");
                completion.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }
        }
    );

    impl IctNotificationDelegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc();
            // SAFETY: NSObject's init method has this signature.
            unsafe { msg_send![this, init] }
        }
    }

    /// The notification center stores a weak delegate, so retain this object
    /// for the process lifetime. Installation itself only needs to happen once.
    pub fn install_foreground_delegate(center: &UNUserNotificationCenter) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let delegate = IctNotificationDelegate::new();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            let _ = Retained::into_raw(delegate);
        });
    }
}

/// Install the macOS foreground delegate during application setup, before the
/// app finishes launching as required by UserNotifications.
#[cfg(target_os = "macos")]
pub fn prepare_desktop_notifications() {
    use objc2_user_notifications::UNUserNotificationCenter;

    let center = UNUserNotificationCenter::currentNotificationCenter();
    macos_notification::install_foreground_delegate(&center);
}

#[cfg(not(target_os = "macos"))]
pub fn prepare_desktop_notifications() {}

/// Send a desktop notification.
///
/// On macOS, use Apple's current UserNotifications framework directly. The
/// Tauri plugin still relies on the deprecated NSUserNotification API there
/// and may report success even when macOS silently drops the notification.
/// Other platforms continue to use the Tauri notification plugin.
///
/// Whether a macOS notification is temporary or persistent is a per-app
/// System Settings preference; the application cannot override it.
pub fn send_desktop_notification(
    app: &tauri::AppHandle,
    title: &str,
    body: &str,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        return send_macos_notification(title, body);
    }

    #[cfg(not(target_os = "macos"))]
    {
        use tauri_plugin_notification::NotificationExt;

        app.notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "macos")]
fn send_macos_notification(title: &str, body: &str) -> Result<(), String> {
    use block2::RcBlock;
    use objc2_foundation::{NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let center = UNUserNotificationCenter::currentNotificationCenter();
    macos_notification::install_foreground_delegate(&center);

    // Asking repeatedly is safe: macOS only presents the permission prompt the
    // first time, and subsequently returns the stored per-app preference.
    let (auth_tx, auth_rx) = mpsc::sync_channel(1);
    let auth_completion = RcBlock::new(move |granted, error: *mut NSError| {
        let result = if let Some(error) = unsafe { error.as_ref() } {
            Err(error.localizedDescription().to_string())
        } else if bool::from(granted) {
            Ok(())
        } else {
            Err("macOS 通知权限未开启".to_owned())
        };
        let _ = auth_tx.send(result);
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert
            | UNAuthorizationOptions::Sound
            | UNAuthorizationOptions::Badge,
        &auth_completion,
    );
    auth_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "等待 macOS 通知授权超时".to_owned())??;

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));

    let sequence = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let identifier = NSString::from_str(&format!("ict-radar-{sequence}"));
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);

    let (send_tx, send_rx) = mpsc::sync_channel(1);
    let send_completion = RcBlock::new(move |error: *mut NSError| {
        let result = unsafe { error.as_ref() }
            .map(|error| Err(error.localizedDescription().to_string()))
            .unwrap_or(Ok(()));
        let _ = send_tx.send(result);
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&send_completion));
    send_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "等待 macOS 通知投递超时".to_owned())?
}

/// Inbox channel: persists to alerts_fired + emits ict:alert:fired.
pub struct InboxChannel {
    store: SqliteStore,
    app: tauri::AppHandle,
}

impl InboxChannel {
    pub fn new(store: SqliteStore, app: tauri::AppHandle) -> Self {
        Self { store, app }
    }
}

impl AlertChannel for InboxChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Inbox
    }

    fn deliver(&self, alert: &AlertRecord) -> Result<(), String> {
        self.store.insert_alert(alert).map_err(|e| e.to_string())?;
        let topic = match alert.trigger {
            AlertTrigger::C2Confirmed => "ict:alert:fired",
            AlertTrigger::Validated => "ict:reversal:recorded",
        };
        let _ = self.app.emit(topic, alert);
        Ok(())
    }
}

/// Desktop notification channel.
pub struct DesktopNotifyChannel {
    app: tauri::AppHandle,
}

impl DesktopNotifyChannel {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl AlertChannel for DesktopNotifyChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::DesktopNotify
    }

    fn deliver(&self, alert: &AlertRecord) -> Result<(), String> {
        let dir_arrow = match alert.candidate_direction {
            crate::detector::types::Direction::Bullish => "↑",
            crate::detector::types::Direction::Bearish => "↓",
        };
        let trade_str = alert
            .trade_symbols
            .iter()
            .map(|s| s.split(':').last().unwrap_or(s))
            .collect::<Vec<_>>()
            .join(" / ");
        let group = match alert.watchlist_id.as_str() {
            "eu-gu-dxy" => "EU/GU",
            "aud-nzd-dxy" => "AUD/NZD",
            "chf-cad-dxy" => "CHF/CAD",
            other => other,
        };
        let title = format!("[{group}] {} C2 已确认 {}", trade_str, dir_arrow);
        let body = format!(
            "请打开图表确认进场时机 · Case {} · 评分 {:.2}",
            alert.c2_case, alert.deterministic_score,
        );

        send_desktop_notification(&self.app, &title, &body)
    }
}
