use anyhow::{Context, Result};
use app_manager_broker::{Broker, BrokerConfig};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = config_path();
    let bytes = tokio::fs::read(&config_path)
        .await
        .with_context(|| format!("read broker config {}", config_path.display()))?;
    let config: BrokerConfig = serde_json::from_slice(&bytes).context("parse broker config")?;
    let broker = Broker::from_config(config)?;

    let listen =
        env::var("APP_MANAGER_BROKER_LISTEN").unwrap_or_else(|_| "127.0.0.1:8787".to_owned());
    let address: SocketAddr = listen.parse().context("parse APP_MANAGER_BROKER_LISTEN")?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("bind broker listener")?;
    broker.listen(listener).await
}

fn config_path() -> PathBuf {
    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--config" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        }
    }
    env::var_os("APP_MANAGER_BROKER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./broker-config.json"))
}
