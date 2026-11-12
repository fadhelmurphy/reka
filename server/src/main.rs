use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let data_dir = PathBuf::from(std::env::var("REKA_DATA").unwrap_or_else(|_| "data".into()));
    let addr = std::env::var("REKA_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!("reka listening on http://{addr}");
    reka_server::serve(listener, &data_dir).await.unwrap();
}
