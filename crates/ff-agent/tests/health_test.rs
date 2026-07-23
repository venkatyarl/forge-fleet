use crate::tests::TestFramework;
use crate::tests::TestHarness;
use crate::tests::TestResult;

pub struct HealthTest {
    test_harness: TestHarness,
}

impl HealthTest {
    pub fn new() -> Self {
        HealthTest {
            test_harness: TestHarness::new(),
        }
    }

    pub fn run(&mut self) {
        // Setup test harness
        self.test_harness.set_base_url("/health");

        // Test health check endpoint
        self.test_harness
            .request_get("/")
            .assert_status_code(200)
            .assert_json_eq("{ \"status\": \"ok\" }", "");

        // Teardown
        self.test_harness.reset();
    }
}
