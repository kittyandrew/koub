use crate::{context, db, db::Pool, notifications, prom};
use rocket::tokio;
use std::time::{Duration, SystemTime};

pub async fn background_handle_down(context: context::Context, db_pool: Pool) {
    loop {
        let mut sleep_for = Duration::new(5, 0);
        let mut states_to_persist = Vec::new();
        {
            // @NOTE: Single write lock to atomically check thresholds and transition
            //  states, preventing TOCTOU race with api_up's touch().
            let mut guard = context.users.write().await;
            let now = SystemTime::now();
            let now_utc_minutes =
                notifications::utc_minute_of_day(now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
            for item in guard.values_mut() {
                let query_at = item.uptime.touched_at + Duration::new(item.user.up_delay as u64, 0);
                if let Ok(remaining) = query_at.duration_since(now) {
                    sleep_for = sleep_for.min(remaining);
                } else if item.uptime.status == db::UpStatus::Paused {
                    // Paused: skip entirely — no down transition while frozen
                } else if item.user.is_in_maintenance_window(now_utc_minutes) {
                    // Maintenance window: suppress down transition
                } else if let Some(duration) = item.uptime.go_down() {
                    prom::UPTIME_STATE.with_label_values(&[&item.user.id.to_string()]).set(i64::from(&item.uptime.status));
                    states_to_persist.push(item.uptime.clone());
                    tokio::spawn(notifications::dispatch_notifications(item.clone(), context.clone(), Some(duration)));
                }
            }
        }
        // Persist state changes to DB
        if !states_to_persist.is_empty() {
            match db_pool.get().await {
                Ok(mut conn) => {
                    for state in &states_to_persist {
                        if let Err(err) = db::update_uptime_state(&mut conn, state).await {
                            warn!("Failed to persist uptime state: {err:?}");
                        }
                    }
                }
                Err(err) => warn!("Failed to get DB connection for state persistence: {err:?}"),
            }
        }
        // @NOTE: This is the default sleep, which handles a case where all clients went
        //  offline, which would be pretty rare at scale, but we must handle this case
        //  anyways, since in case that happens we don't want to run without sleep.
        tokio::time::sleep(sleep_for).await;
    }
}
