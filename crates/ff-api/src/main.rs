use tracing_subscriber::EnvFilter;

use ff_api::config::ApiConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "ff_api=info,info".into()),
        )
        .init();

    let config = ApiConfig::from_env();
    ff_api::run(config).await
}
