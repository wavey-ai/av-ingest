use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "av_ingest_proxy=info,web_service=info".into()),
        )
        .init();

    av_ingest_proxy::run_from_env().await
}
