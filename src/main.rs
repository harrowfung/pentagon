mod files;
mod handlers;
mod system_monitor;
mod types;
mod utils;
mod worker;

use crate::{
    handlers::{
        metrics::metrics_endpoint,
        run::{execute_code_endpoint, execute_code_ws_handler},
    },
    types::{AppConfig, AppState},
};

use axum::{
    Router,
    routing::{any, get, post},
};
use config::Config;
use dotenvy::dotenv;
use metrics::{describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    dotenv().ok();
    let settings = Config::builder()
        .add_source(config::File::with_name("Settings"))
        .add_source(config::Environment::with_prefix("APP"))
        .build()
        .unwrap();

    let app_config: AppConfig = settings.try_deserialize().unwrap();

    // Install global Prometheus recorder and keep the handle for rendering metrics.
    let builder = PrometheusBuilder::new();
    let handle = builder.install_recorder().unwrap();

    // Optional: describe metrics for documentation.
    describe_counter!("requests_total", "Total number of /execute requests");
    describe_counter!("executions_total", "Total number of executed programs");
    describe_histogram!("execution_time_ms", "Execution time in milliseconds");
    describe_histogram!(
        "execution_wall_time_ms",
        "Wall execution time in milliseconds"
    );
    describe_histogram!(
        "execution_total_duration_ms",
        "Total execution duration including setup in milliseconds"
    );
    describe_histogram!("execution_memory_kb", "Memory used in kilobytes");
    describe_gauge!("active_workers", "Number of active workers");
    describe_gauge!("active_executions", "Number of active executions running");
    describe_gauge!(
        "websocket_connections_active",
        "Number of active websocket connections"
    );
    describe_counter!(
        "websocket_messages_received_total",
        "Total number of websocket messages received"
    );
    describe_counter!(
        "websocket_messages_sent_total",
        "Total number of websocket messages sent"
    );
    describe_counter!("files_created_total", "Total number of files created");
    describe_gauge!("system_memory_used_bytes", "Used system memory in bytes");
    describe_gauge!("system_memory_total_bytes", "Total system memory in bytes");
    describe_gauge!("system_cpu_usage_percent", "System CPU usage in percent");
    describe_gauge!("system_disk_free_bytes", "Free disk space in bytes");
    describe_gauge!("system_disk_total_bytes", "Total disk space in bytes");

    system_monitor::start_system_monitor().await;

    let client = redis::Client::open(app_config.redis_url).unwrap();
    let con = client.get_multiplexed_async_connection().await.unwrap();
    let app = Router::new()
        .route("/execute", post(execute_code_endpoint))
        .route("/execute", any(execute_code_ws_handler))
        .route("/metrics", get(metrics_endpoint))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        )
        .with_state(AppState {
            redis_connection: con,
            base_code_path: app_config.base_code_path.clone(),
            prometheus_handle: handle.clone(),
        });

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", app_config.port))
        .await
        .unwrap();

    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
