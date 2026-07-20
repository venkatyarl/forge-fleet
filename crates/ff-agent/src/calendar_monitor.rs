//! Leader-gated iCalendar monitoring and event-to-task scheduling.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use serde_json::Value;
use sqlx::{PgPool, Row};
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, PartialEq)]
struct CalendarEvent {
    uid: String,
    summary: String,
    starts_at: DateTime<Utc>,
    ends_at: Option<DateTime<Utc>>,
    location: Option<String>,
}

/// Poll every due calendar monitor and enqueue one task per imminent event.
pub async fn evaluate_calendars(pg: &PgPool, worker_name: &str) -> Result<usize> {
    let monitors = sqlx::query(
        "SELECT id, project_id, name, feed_url, task_template, lead_time_minutes, \
         poll_interval_minutes, etag, last_modified FROM calendar_monitors \
         WHERE enabled AND next_poll_at <= NOW() ORDER BY next_poll_at",
    )
    .fetch_all(pg)
    .await?;

    let mut enqueued = 0;
    for monitor in monitors {
        let id: Uuid = monitor.get("id");
        match poll_monitor(pg, worker_name, &monitor).await {
            Ok(count) => enqueued += count,
            Err(error) => {
                warn!(monitor_id = %id, error = %error, "calendar poll failed");
                sqlx::query(
                    "UPDATE calendar_monitors SET last_polled_at=NOW(), last_error=$2, \
                     next_poll_at=NOW() + poll_interval_minutes * INTERVAL '1 minute' WHERE id=$1",
                )
                .bind(id)
                .bind(error.to_string())
                .execute(pg)
                .await?;
            }
        }
    }
    Ok(enqueued)
}

async fn poll_monitor(
    pg: &PgPool,
    worker_name: &str,
    row: &sqlx::postgres::PgRow,
) -> Result<usize> {
    let id: Uuid = row.get("id");
    let feed_url: String = row.get("feed_url");
    let mut request = super::notifications::SHARED_HTTP.get(&feed_url);
    if let Some(etag) = row.get::<Option<String>, _>("etag") {
        request = request.header(IF_NONE_MATCH, etag);
    }
    if let Some(modified) = row.get::<Option<String>, _>("last_modified") {
        request = request.header(IF_MODIFIED_SINCE, modified);
    }
    let response = request
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("fetching calendar {feed_url}"))?;

    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        mark_polled(pg, id, None, None).await?;
        return Ok(0);
    }
    let response = response.error_for_status()?;
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let last_modified = response
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await?;
    let events = parse_icalendar(&body);
    let now = Utc::now();
    let lead = chrono::Duration::minutes(row.get::<i32, _>("lead_time_minutes").into());
    let template: sqlx::types::Json<Value> = row.get("task_template");
    let project_id: String = row.get("project_id");
    let monitor_name: String = row.get("name");
    let mut count = 0;

    for event in events
        .into_iter()
        .filter(|event| event.starts_at >= now && event.starts_at <= now + lead)
    {
        let mut tx = pg.begin().await?;
        let already_scheduled: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM calendar_event_actions \
             WHERE monitor_id=$1 AND event_uid=$2 AND event_start=$3)",
        )
        .bind(id)
        .bind(&event.uid)
        .bind(event.starts_at)
        .fetch_one(&mut *tx)
        .await?;
        if already_scheduled {
            tx.rollback().await?;
            continue;
        }
        let task_id = Uuid::new_v4();
        let summary = template
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Calendar: {}", event.summary));
        let payload = serde_json::json!({
            "calendar": { "monitor_id": id, "monitor": monitor_name, "uid": event.uid,
                "summary": event.summary, "starts_at": event.starts_at,
                "ends_at": event.ends_at, "location": event.location },
            "project_id": project_id, "template": template.0,
        });
        sqlx::query(
            "INSERT INTO fleet_tasks (id,task_type,summary,payload,priority,requires_capability,\
             created_by_computer_id,routing_mode) SELECT $1,COALESCE($2,'shell'),$3,$4,\
             COALESCE($5,50),COALESCE($6,'[]'::jsonb),c.id,'fleet_first' FROM computers c WHERE c.name=$7",
        )
        .bind(task_id)
        .bind(template.get("task_type").and_then(Value::as_str))
        .bind(summary)
        .bind(payload)
        .bind(template.get("priority").and_then(Value::as_i64).map(|v| v as i32))
        .bind(template.get("requires_capability").cloned())
        .bind(worker_name)
        .execute(&mut *tx)
        .await?;
        let claimed = sqlx::query(
            "INSERT INTO calendar_event_actions (monitor_id,event_uid,event_start,task_id) \
             VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(&event.uid)
        .bind(event.starts_at)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            tx.rollback().await?;
            continue;
        }
        tx.commit().await?;
        count += 1;
    }
    mark_polled(pg, id, etag, last_modified).await?;
    if count > 0 {
        info!(monitor_id = %id, count, "calendar event tasks enqueued");
    }
    Ok(count)
}

async fn mark_polled(
    pg: &PgPool,
    id: Uuid,
    etag: Option<String>,
    modified: Option<String>,
) -> Result<()> {
    sqlx::query(
        "UPDATE calendar_monitors SET last_polled_at=NOW(), last_error=NULL, \
         next_poll_at=NOW() + poll_interval_minutes * INTERVAL '1 minute', \
         etag=COALESCE($2,etag), last_modified=COALESCE($3,last_modified) WHERE id=$1",
    )
    .bind(id)
    .bind(etag)
    .bind(modified)
    .execute(pg)
    .await?;
    Ok(())
}

fn parse_icalendar(input: &str) -> Vec<CalendarEvent> {
    let unfolded = input.replace("\r\n ", "").replace("\r\n\t", "");
    let mut events = Vec::new();
    let mut fields: Vec<(&str, &str)> = Vec::new();
    let mut inside = false;
    for line in unfolded.lines().map(|line| line.trim_end_matches('\r')) {
        match line {
            "BEGIN:VEVENT" => {
                inside = true;
                fields.clear();
            }
            "END:VEVENT" if inside => {
                let get = |name: &str| {
                    fields
                        .iter()
                        .find(|(key, _)| key.split(';').next() == Some(name))
                        .map(|(_, value)| *value)
                };
                if let (Some(uid), Some(start)) =
                    (get("UID"), get("DTSTART").and_then(parse_datetime))
                {
                    events.push(CalendarEvent {
                        uid: unescape(uid),
                        summary: get("SUMMARY")
                            .map(unescape)
                            .unwrap_or_else(|| "Untitled event".into()),
                        starts_at: start,
                        ends_at: get("DTEND").and_then(parse_datetime),
                        location: get("LOCATION").map(unescape),
                    });
                }
                inside = false;
            }
            _ if inside => {
                if let Some((key, value)) = line.split_once(':') {
                    fields.push((key, value));
                }
            }
            _ => {}
        }
    }
    events
}

fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return Some(value.with_timezone(&Utc));
    }
    if let Ok(value) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ") {
        return Some(Utc.from_utc_datetime(&value));
    }
    if let Ok(value) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S") {
        return Some(Utc.from_utc_datetime(&value));
    }
    NaiveDate::parse_from_str(value, "%Y%m%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|value| Utc.from_utc_datetime(&value))
}

fn unescape(value: &str) -> String {
    value
        .replace("\\n", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
}

/// Spawn the calendar polling loop. Only the elected leader performs work.
pub fn spawn_calendar_monitor(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if crate::leader_cache::is_current_leader() {
                        if let Err(error) = evaluate_calendars(&pg, &worker_name).await { warn!(error = %error, "calendar monitor tick failed"); }
                    } else { debug!("calendar monitor skipped on follower"); }
                }
                changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { break; }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unfolded_ical_events() {
        let events = parse_icalendar(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup-1\r\nDTSTART:20260721T130000Z\r\nDTEND:20260721T133000Z\r\nSUMMARY:Fleet\\, standup\r\nLOCATION:Zoom\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Fleet, standup");
        assert_eq!(
            events[0].starts_at,
            "2026-07-21T13:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn ignores_events_without_identity_or_start() {
        assert!(parse_icalendar("BEGIN:VEVENT\nSUMMARY:No identity\nEND:VEVENT").is_empty());
    }
}
