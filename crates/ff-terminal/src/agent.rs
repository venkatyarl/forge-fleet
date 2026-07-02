use ff_agent::cloud_error::classify;

pub fn classify_cloud_error(err: &ff_agent::cloud_error::CloudError) -> String {
    let info = classify(err);
    if info.message.is_empty() {
        format!("{} ({})", info.error_type, info.status)
    } else {
        format!("{} ({}) — {}", info.error_type, info.status, info.message)
    }
}
