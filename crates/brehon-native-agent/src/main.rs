use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "brehon_native_agent=info,warn".to_string()),
        )
        .with_writer(std::io::stderr)
        .init();

    brehon_native_agent::run(brehon_native_agent::Cli::parse()).await
}
