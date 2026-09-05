//! `kist serve` 的 HTTP 端：`/metrics`（OpenMetrics 文字，給 Prometheus 抓）與 `/healthz`。
//! 之後的 Web UI 會長在同一個 router 上。只綁使用者指定的位址（CLI 預設 127.0.0.1），
//! 沒有認證——不要直接暴露到網路上。

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::metrics::Metrics;

pub async fn serve_http(
    metrics: Arc<Metrics>,
    listener: TcpListener,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async { "kist: see /metrics and /healthz\n" }),
        )
        .route("/metrics", axum::routing::get(metrics_handler))
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .with_state(metrics);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await
}

/// 等 watch channel 變 true。`wait_for` 會先看當前值（訊號已發過就立即返回）；
/// sender 消失（`Err`）也視為關機。
async fn shutdown_signal(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|shutdown| *shutdown).await;
}

/// prometheus-client 輸出的是 OpenMetrics 文字格式（結尾有 `# EOF`）；
/// 舊的 text/plain 0.0.4 抓取器也讀得懂（`#` 開頭當註解）。
async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        metrics.render(),
    )
}
