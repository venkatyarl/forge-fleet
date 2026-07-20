#[test]
fn feeder_backpressure_limits_are_bounded() {
    assert_eq!(ff_agent::auto_backlog_feeder::REVIEW_CEILING, 40);
    assert_eq!(ff_agent::auto_backlog_feeder::FEED_TARGET, 30);
    assert!((1..=2).contains(&ff_agent::auto_backlog_feeder::MAX_FEEDS_PER_TICK));
    assert!(ff_agent::auto_backlog_feeder::within_pipeline_limits(
        true, 39, 29
    ));
    assert!(!ff_agent::auto_backlog_feeder::within_pipeline_limits(
        false, 0, 0
    ));
    assert!(!ff_agent::auto_backlog_feeder::within_pipeline_limits(
        true, 40, 0
    ));
    assert!(!ff_agent::auto_backlog_feeder::within_pipeline_limits(
        true, 0, 30
    ));
}
