use std::convert::Infallible;

use crate::{
    files::FileManager,
    types::{ExecutionRequest, ExecutionResult},
    worker::Worker,
};
use async_stream::try_stream;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::response::IntoResponse;
use axum::{
    Json, Router,
    extract::State,
    response::{
        Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use config::Config;
use dotenvy::dotenv;
use futures_util::stream::Stream;
use metrics::{counter, describe_counter, describe_histogram, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use redis::aio::MultiplexedConnection;
use serde_json::json;
use tokio::sync::mpsc::{self, Sender};

mod files;
mod types;
mod worker;

#[derive(Debug, Default, serde::Deserialize, PartialEq, Eq)]
struct AppConfig {
    redis_url: String,
    base_code_path: String,
    port: u16,
}

#[derive(Clone)]
struct AppState {
    redis_connection: MultiplexedConnection,
    base_code_path: String,
    prometheus_handle: PrometheusHandle,
}

fn gen_random_id(length: u32) -> String {
    let id: String = Vec::from_iter(
        (0..length)
            .map(|_| {
                let idx = fastrand::usize(0..36);
                char::from_digit(idx as u32, 36).unwrap()
            })
            .collect::<Vec<char>>(),
    )
    .into_iter()
    .collect();

    id
}

async fn execute_code_inner(
    state: AppState,
    payload: ExecutionRequest,
    tx: Sender<Result<ExecutionResult, String>>,
) {
    let mut worker = Worker::new(
        format!("{}/{}", state.base_code_path, gen_random_id(10)),
        Box::new(FileManager::new(state.redis_connection)),
    );

    for file in payload.files {
        if let Err(e) = worker.write_file(file).await {
            tracing::error!("error writing file: {}", e);
            counter!("executions_total", "outcome" => "error").increment(1);
            worker.cleanup().await;

            let _ = tx.send(Err(format!("failed to write file: {}", e))).await;
            return;
        }
    }

    for request in payload.executions {
        let die_on_error = request.die_on_error;

        let result = worker.execute(request).await;

        if let Err(e) = &result {
            tracing::error!("error executing code: {}", e.message);
            counter!("executions_total", "outcome" => "error").increment(1);
            worker.cleanup().await;

            let _ = tx
                .send(Err(format!("failed to execute code: {}", e.message)))
                .await;
            return;
        }

        let result = result.unwrap();
        counter!("executions_total", "outcome" => "ok").increment(1);
        histogram!("execution_time_ms").record(result.time_used as f64);
        histogram!("execution_memory_kb").record(result.memory_used as f64);

        let exit_code = result.exit_code;
        let _ = tx.send(Ok(result)).await;

        if die_on_error && exit_code != 0 {
            break;
        }
    }

    worker.cleanup().await;
}

async fn execute_code(
    State(state): State<AppState>,
    Json(payload): Json<ExecutionRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = mpsc::channel::<Result<ExecutionResult, String>>(100);
    counter!("requests_total").increment(1);

    tokio::spawn(async move {
        let _ = execute_code_inner(state, payload, tx).await;
    });

    Sse::new(try_stream! {
        loop {
            match rx.recv().await {
                Some(data) => {
                    match data {
                        Ok(json) => {
                            yield Event::default().data(serde_json::to_string(&json).unwrap());
                        },
                        Err(err) => {
                            tracing::error!("error executing code: {}", err);
                            yield Event::default().data(json!({ "error": err }).to_string());
                        }
                    }
                },
                None => {
                    break;
                }
            }
        }
    })
    .keep_alive(KeepAlive::default())
}

async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    state.prometheus_handle.run_upkeep();
    let body = state.prometheus_handle.render();
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    (StatusCode::OK, headers, body)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

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
    describe_histogram!("execution_memory_kb", "Memory used in kilobytes");

    let client = redis::Client::open(app_config.redis_url).unwrap();
    let con = client.get_multiplexed_async_connection().await.unwrap();
    let app = Router::new()
        .route("/execute", post(execute_code))
        .route("/metrics", get(metrics_endpoint))
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
