use async_stream::try_stream;
use axum::{
    Json,
    extract::{
        State,
        ws::{Message, Utf8Bytes},
    },
    response::{
        Sse,
        sse::{Event, KeepAlive},
    },
};
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::Response,
};
use futures_util::Stream;
use metrics::{counter, gauge, histogram};
use serde_json::json;
use std::convert::Infallible;
use std::time::Instant;
use tokio::sync::mpsc::{self, Sender};

use crate::{
    files::RedisFileManager,
    types::{AppState, Execution, ExecutionMessage, ExecutionRequest, ExecutionResult},
    utils::gen_random_id,
    worker::Worker,
};

struct GaugeGuard {
    name: &'static str,
}

impl GaugeGuard {
    fn new(name: &'static str) -> Self {
        gauge!(name).increment(1.0);
        Self { name }
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        gauge!(self.name).decrement(1.0);
    }
}

#[tracing::instrument(skip(worker), fields(program = %request.program))]
async fn execute_execution(
    worker: &mut Worker,
    request: Execution,
) -> Result<ExecutionResult, String> {
    let _guard = GaugeGuard::new("active_executions");
    tracing::debug!("starting execution");
    let result = worker.execute(request).await;

    if let Err(e) = &result {
        tracing::error!("error executing code: {}", e.message);
        counter!("executions_total", "outcome" => "error").increment(1);

        return Err(format!("failed to execute code: {}", e.message));
    }

    let result = result.unwrap();
    tracing::debug!(
        exit_code = result.exit_code,
        time_used = result.time_used,
        memory_used = result.memory_used,
        "execution finished"
    );
    counter!("executions_total", "outcome" => "ok").increment(1);
    histogram!("execution_time_ms").record(result.time_used as f64);
    histogram!("execution_memory_kb").record(result.memory_used as f64);

    Ok(result)
}

#[tracing::instrument(skip(state, tx), fields(files_count = payload.files.len(), executions_count = payload.executions.len()))]
async fn execute_code_inner(
    state: AppState,
    payload: ExecutionRequest,
    tx: Sender<Result<ExecutionResult, String>>,
) {
    let start = Instant::now();
    let _guard = GaugeGuard::new("active_workers");
    tracing::info!("processing execution request");
    let mut worker = Worker::new(
        format!("{}/{}", state.base_code_path, gen_random_id(10)),
        Box::new(RedisFileManager::new(state.redis_connection)),
    );

    for file in payload.files {
        if let Err(e) = worker.write_file(file).await {
            tracing::error!("error writing file: {}", e);
            counter!("executions_total", "outcome" => "error").increment(1);
            worker.cleanup().await;
            histogram!("execution_total_duration_ms").record(start.elapsed().as_millis() as f64);

            let _ = tx.send(Err(format!("failed to write file: {}", e))).await;
            return;
        }
    }

    for request in payload.executions {
        let die_on_error = request.die_on_error;

        let result = execute_execution(&mut worker, request).await;
        let exit_code = match &result {
            Ok(res) => res.exit_code,
            Err(_) => 1,
        };
        if let Ok(res) = result {
            let _ = tx.send(Ok(res)).await;
        }

        if die_on_error && exit_code != 0 {
            break;
        }
    }

    worker.cleanup().await;
    histogram!("execution_total_duration_ms").record(start.elapsed().as_millis() as f64);
}

#[tracing::instrument(skip(state))]
pub async fn execute_code_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<ExecutionRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = mpsc::channel::<Result<ExecutionResult, String>>(100);
    counter!("requests_total").increment(1);
    tracing::info!("received execution request");

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

pub async fn execute_code_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(|ws| handle_socket(ws, state))
}

#[tracing::instrument(skip(socket, state))]
async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let _guard = GaugeGuard::new("websocket_connections_active");
    let _worker_guard = GaugeGuard::new("active_workers");

    tracing::info!("websocket connection established for code execution");
    let mut worker = Worker::new(
        format!("{}/{}", state.base_code_path, gen_random_id(10)),
        Box::new(RedisFileManager::new(state.redis_connection)),
    );

    while let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            let start = Instant::now();
            counter!("websocket_messages_received_total").increment(1);
            let result = serde_json::from_str::<ExecutionMessage>(msg.to_text().unwrap());
            if result.is_err() {
                tracing::error!("invalid execution request: {}", result.err().unwrap());
                continue;
            }
            let message = result.unwrap();
            match message {
                ExecutionMessage::Single { id, execution } => {
                    tracing::debug!(id = ?id, "processing single execution");
                    let result = execute_execution(&mut worker, execution).await;

                    let msg = match result {
                        Ok(res) => {
                            Message::Text(Utf8Bytes::from(serde_json::to_string(&res).unwrap()))
                        }
                        Err(err) => {
                            tracing::error!("error executing code: {}", err);
                            Message::Text(Utf8Bytes::from(json!({ "error": err }).to_string()))
                        }
                    };

                    if socket.send(msg).await.is_err() {
                        break;
                    }
                    counter!("websocket_messages_sent_total").increment(1);
                }

                ExecutionMessage::Batch { id, executions } => {
                    tracing::debug!(id = ?id, count = executions.len(), "processing batch execution");
                    for execution in executions {
                        let die_on_error = execution.die_on_error.clone();
                        let result = execute_execution(&mut worker, execution).await;

                        match result {
                            Ok(res) => {
                                if socket
                                    .send(Message::Text(Utf8Bytes::from(
                                        serde_json::to_string(&res).unwrap(),
                                    )))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                counter!("websocket_messages_sent_total").increment(1);
                                if res.exit_code != 0 && die_on_error {
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::error!("error executing code: {}", err);
                                if socket
                                    .send(Message::Text(Utf8Bytes::from(
                                        json!({ "error": err }).to_string(),
                                    )))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                counter!("websocket_messages_sent_total").increment(1);
                            }
                        }
                    }
                }
            }
            histogram!("execution_total_duration_ms").record(start.elapsed().as_millis() as f64);
        } else {
            tracing::error!("error receiving websocket message: {}", msg.err().unwrap());

            break;
        };
    }

    worker.cleanup().await;
}
