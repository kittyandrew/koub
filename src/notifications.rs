use crate::{context, db, ntfy, prom};
use fluent::types::FluentValue;
use fluent_templates::{Loader, static_loader};
use rocket::tokio;
use std::{collections::HashMap, time::Duration};
use unic_langid::LanguageIdentifier;

static_loader! {
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en",
        // @NOTE: Disable Unicode isolating marks in Fluent output.
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

/// Supported locale codes — must match directories under `locales/`.
pub const SUPPORTED_LOCALES: &[&str] = &["en", "uk"];

/// Formats a duration with days, hours, and minutes using Fluent locale strings.
/// Each part (days, hours, minutes) is looked up via `duration-days`, `duration-hours`,
/// `duration-minutes` keys in the locale files, which handle pluralization via CLDR rules.
pub fn format_duration(lang: &LanguageIdentifier, duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;

    let mut parts = Vec::new();

    if days > 0 {
        let mut args = HashMap::new();
        args.insert("count".to_string(), FluentValue::from(days as i64));
        parts.push(LOCALES.lookup_with_args(lang, "duration-days", &args));
    }

    if hours > 0 {
        let mut args = HashMap::new();
        args.insert("count".to_string(), FluentValue::from(hours as i64));
        parts.push(LOCALES.lookup_with_args(lang, "duration-hours", &args));
    }

    if minutes > 0 || parts.is_empty() {
        let mut args = HashMap::new();
        args.insert("count".to_string(), FluentValue::from(minutes as i64));
        parts.push(LOCALES.lookup_with_args(lang, "duration-minutes", &args));
    }

    parts.join(" ")
}

pub async fn dispatch_notifications(item: db::UserState, context: context::Context, duration: Option<Duration>) {
    let lang: LanguageIdentifier = item.user.language_code.parse().unwrap_or_else(|_| {
        warn!("Invalid language code '{}' for user {}, falling back to 'en'", item.user.language_code, item.user.id);
        "en".parse().unwrap()
    });
    if !SUPPORTED_LOCALES.contains(&lang.language.as_str()) {
        warn!(
            "Unsupported locale '{}' for user {}, notifications will use English fallback",
            item.user.language_code, item.user.id
        );
    }

    // Format the duration message based on status
    let duration_message = match (item.uptime.status, duration) {
        (db::UpStatus::Up, Some(d)) => {
            let mut args = HashMap::new();
            args.insert("duration".to_string(), FluentValue::from(format_duration(&lang, d)));
            LOCALES.lookup_with_args(&lang, "duration-power-was-off", &args)
        }
        (db::UpStatus::Down, Some(d)) => {
            let mut args = HashMap::new();
            args.insert("duration".to_string(), FluentValue::from(format_duration(&lang, d)));
            LOCALES.lookup_with_args(&lang, "duration-power-was-on", &args)
        }
        _ => String::new(),
    };

    // @NOTE: Paused devices never trigger notifications (silent freeze/thaw).
    //  Pause/unpause is intentionally silent — the user initiated it.
    let title = match item.uptime.status {
        db::UpStatus::Uninitialized => LOCALES.lookup(&lang, "notification-device-connected"),
        db::UpStatus::Up => LOCALES.lookup(&lang, "notification-power-on"),
        db::UpStatus::Down => LOCALES.lookup(&lang, "notification-power-off"),
        db::UpStatus::Paused => return,
    };

    if item.ntfy.enabled {
        let notification = ntfy::NtfyNotification {
            topic: item.ntfy.topic,
            title,
            message: duration_message,
            status: match item.uptime.status {
                db::UpStatus::Uninitialized | db::UpStatus::Up => "white_check_mark".to_string(),
                db::UpStatus::Down => "warning".to_string(),
                db::UpStatus::Paused => unreachable!(),
            },
            priority: "high".to_string(), // Configurable for user.
        };

        let ntfy_type = match item.uptime.status {
            db::UpStatus::Uninitialized => "connected",
            db::UpStatus::Up => "up",
            db::UpStatus::Down => "down",
            db::UpStatus::Paused => unreachable!(),
        };
        let ntfy_type = ntfy_type.to_string();
        tokio::spawn(async move {
            info!("Sending ntfy {notification:?} to {u:?}", u = item.ntfy.username);
            match context.ntfy.send_notification(notification).await {
                Ok(_) => prom::NOTIFICATIONS.with_label_values(&[&ntfy_type, "success"]).inc(),
                Err(err) => {
                    warn!("Failed attempting to send ntfy-cation: {err:?}");
                    prom::NOTIFICATIONS.with_label_values(&[&ntfy_type, "failure"]).inc();
                }
            }
        });
    }
}

/// Current UTC time-of-day in minutes (0-1439), for maintenance window checks.
pub fn utc_minute_of_day(epoch_secs: u64) -> i32 {
    ((epoch_secs % 86400) / 60) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uk() -> LanguageIdentifier {
        "uk".parse().unwrap()
    }
    fn en() -> LanguageIdentifier {
        "en".parse().unwrap()
    }

    // Ukrainian duration tests

    #[test]
    fn test_format_duration_uk_minutes_only() {
        let lang = uk();
        assert_eq!(format_duration(&lang, Duration::from_secs(0)), "0 хв");
        assert_eq!(format_duration(&lang, Duration::from_secs(60)), "1 хв");
        assert_eq!(format_duration(&lang, Duration::from_secs(35 * 60)), "35 хв");
        assert_eq!(format_duration(&lang, Duration::from_secs(59 * 60)), "59 хв");
    }

    #[test]
    fn test_format_duration_uk_hours_and_minutes() {
        let lang = uk();
        assert_eq!(format_duration(&lang, Duration::from_secs(3600)), "1 год");
        assert_eq!(format_duration(&lang, Duration::from_secs(3600 + 8 * 60)), "1 год 8 хв");
        assert_eq!(format_duration(&lang, Duration::from_secs(23 * 3600 + 8 * 60)), "23 год 8 хв");
    }

    #[test]
    fn test_format_duration_uk_days_singular() {
        let lang = uk();
        assert_eq!(format_duration(&lang, Duration::from_secs(86400)), "1 день");
        assert_eq!(format_duration(&lang, Duration::from_secs(86400 + 12 * 3600 + 22 * 60)), "1 день 12 год 22 хв");
        assert_eq!(format_duration(&lang, Duration::from_secs(21 * 86400)), "21 день");
        assert_eq!(format_duration(&lang, Duration::from_secs(31 * 86400)), "31 день");
        assert_eq!(format_duration(&lang, Duration::from_secs(101 * 86400)), "101 день");
    }

    #[test]
    fn test_format_duration_uk_days_few() {
        let lang = uk();
        assert_eq!(format_duration(&lang, Duration::from_secs(2 * 86400)), "2 дні");
        assert_eq!(format_duration(&lang, Duration::from_secs(3 * 86400)), "3 дні");
        assert_eq!(format_duration(&lang, Duration::from_secs(4 * 86400)), "4 дні");
        assert_eq!(format_duration(&lang, Duration::from_secs(22 * 86400)), "22 дні");
        assert_eq!(format_duration(&lang, Duration::from_secs(24 * 86400)), "24 дні");
        assert_eq!(format_duration(&lang, Duration::from_secs(102 * 86400)), "102 дні");
    }

    #[test]
    fn test_format_duration_uk_days_many() {
        let lang = uk();
        assert_eq!(format_duration(&lang, Duration::from_secs(5 * 86400)), "5 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(10 * 86400)), "10 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(11 * 86400)), "11 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(12 * 86400)), "12 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(13 * 86400)), "13 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(14 * 86400)), "14 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(15 * 86400)), "15 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(20 * 86400)), "20 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(100 * 86400)), "100 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(111 * 86400)), "111 днів");
        assert_eq!(format_duration(&lang, Duration::from_secs(112 * 86400)), "112 днів");
    }

    #[test]
    fn test_format_duration_uk_full_example() {
        let lang = uk();
        let duration = Duration::from_secs(238 * 86400 + 15 * 3600 + 13 * 60);
        assert_eq!(format_duration(&lang, duration), "238 днів 15 год 13 хв");
    }

    // English duration tests

    #[test]
    fn test_format_duration_en_minutes_only() {
        let lang = en();
        assert_eq!(format_duration(&lang, Duration::from_secs(0)), "0 min");
        assert_eq!(format_duration(&lang, Duration::from_secs(60)), "1 min");
        assert_eq!(format_duration(&lang, Duration::from_secs(35 * 60)), "35 min");
        assert_eq!(format_duration(&lang, Duration::from_secs(59 * 60)), "59 min");
    }

    #[test]
    fn test_format_duration_en_hours_and_minutes() {
        let lang = en();
        assert_eq!(format_duration(&lang, Duration::from_secs(3600)), "1 hr");
        assert_eq!(format_duration(&lang, Duration::from_secs(3600 + 8 * 60)), "1 hr 8 min");
        assert_eq!(format_duration(&lang, Duration::from_secs(23 * 3600 + 8 * 60)), "23 hr 8 min");
    }

    #[test]
    fn test_format_duration_en_days() {
        let lang = en();
        assert_eq!(format_duration(&lang, Duration::from_secs(86400)), "1 day");
        assert_eq!(format_duration(&lang, Duration::from_secs(86400 + 12 * 3600 + 22 * 60)), "1 day 12 hr 22 min");
        assert_eq!(format_duration(&lang, Duration::from_secs(2 * 86400)), "2 days");
        assert_eq!(format_duration(&lang, Duration::from_secs(10 * 86400)), "10 days");
        assert_eq!(format_duration(&lang, Duration::from_secs(21 * 86400)), "21 days");
    }

    #[test]
    fn test_format_duration_en_full_example() {
        let lang = en();
        let duration = Duration::from_secs(238 * 86400 + 15 * 3600 + 13 * 60);
        assert_eq!(format_duration(&lang, duration), "238 days 15 hr 13 min");
    }
}
