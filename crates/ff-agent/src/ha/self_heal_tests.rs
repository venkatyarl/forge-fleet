//! Additional self-heal tests that require a live Postgres instance.
//!
//! These live in a sibling module so the main `self_heal.rs` unit tests stay
//! focused, while multi-step scenarios (e.g. re-arming the same signature
//! repeatedly) can share the same helper pool.

#[cfg(test)]
mod tests {
    use super::super::self_heal::test_helpers::*;
    use super::super::self_heal::*;
    use chrono::Utc;
    use sqlx::Row;
    use std::env;

    #[tokio::test]
    async fn rearm_task_rearms_multiple_times_for_same_signature() {
        if env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping rearm_self_heal_task DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin, pool, db_name) = create_temp_db().await;

        let signature = "sig-rearm-multiple";
        let completed = Utc::now() - chrono::Duration::hours(1);
        let id = insert_self_heal_task(&pool, signature, "completed", Some(completed)).await;

        // First report re-arms the terminal task.
        let rearmed = rearm_self_heal_task(&pool, signature, "T2", 1, None)
            .await
            .expect("first rearm");
        assert!(rearmed, "first rearm should update the row");

        let first_rearm = sqlx::query(
            "SELECT status,
                    (payload->>'report_count')::int AS report_count,
                    (payload->>'status')::text AS payload_status
               FROM fleet_tasks
              WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch after first rearm");
        assert_eq!(first_rearm.get::<String, _>("status"), "pending");
        assert_eq!(first_rearm.get::<String, _>("payload_status"), "detected");
        assert_eq!(first_rearm.get::<i32, _>("report_count"), 1);

        // Simulate the writer running to completion again.
        let completed_again = Utc::now() - chrono::Duration::hours(1);
        sqlx::query(
            "UPDATE fleet_tasks
                SET status = 'completed',
                    completed_at = $2,
                    payload = payload || jsonb_build_object('status', 'verified')
              WHERE id = $1",
        )
        .bind(id)
        .bind(completed_again)
        .execute(&pool)
        .await
        .expect("complete task again");

        // A second report of the same signature should re-arm again.
        let rearmed_again = rearm_self_heal_task(&pool, signature, "T2", 1, None)
            .await
            .expect("second rearm");
        assert!(rearmed_again, "second rearm should update the row");

        let second_rearm = sqlx::query(
            "SELECT status,
                    (payload->>'report_count')::int AS report_count,
                    (payload->>'status')::text AS payload_status
               FROM fleet_tasks
              WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch after second rearm");
        assert_eq!(second_rearm.get::<String, _>("status"), "pending");
        assert_eq!(second_rearm.get::<String, _>("payload_status"), "detected");
        assert_eq!(
            second_rearm.get::<i32, _>("report_count"),
            2,
            "report_count should accumulate across re-arms"
        );

        drop_temp_db(admin, pool, &db_name).await;
    }
}
