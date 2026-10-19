use anyhow::{Context, Result};
use claude_sync::{app::App, config::Config, web};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env()?;
    let bind = cfg.bind;
    tracing::info!(
        %bind,
        node = %cfg.node_name,
        claude = %cfg.claude_bin,
        tmux_socket = %cfg.tmux_socket,
        "старт"
    );
    let app = App::new(cfg);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("не удалось занять {bind}"))?;
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("остановка");
}
