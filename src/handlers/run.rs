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
use metrics::{counter, histogram};
use serde_json::json;
use std::convert::Infallible;
use tokio::sync::mpsc::{self, Sender};

use crate::{
    files::RedisFileManager,
    types::{AppState, Execution, ExecutionRequest, ExecutionResult},
    utils::gen_random_id,
    worker::Worker,
};

async fn execute_execution(
    worker: &mut Worker,
    request: Execution,
) -> Result<ExecutionResult, String> {
    let result = worker.execute(request).await;

    if let Err(e) = &result {
        tracing::error!("error executing code: {}", e.message);
        counter!("executions_total", "outcome" => "error").increment(1);
        worker.cleanup().await;

        return Err(format!("failed to execute code: {}", e.message));
    }

    let result = result.unwrap();
    counter!("executions_total", "outcome" => "ok").increment(1);
    histogram!("execution_time_ms").record(result.time_used as f64);
    histogram!("execution_memory_kb").record(result.memory_used as f64);

    Ok(result)
}

async fn execute_code_inner(
    state: AppState,
    payload: ExecutionRequest,
    tx: Sender<Result<ExecutionResult, String>>,
) {
    let mut worker = Worker::new(
        format!("{}/{}", state.base_code_path, gen_random_id(10)),
        Box::new(RedisFileManager::new(state.redis_connection)),
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
}

pub async fn execute_code_endpoint(
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

pub async fn execute_code_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(|ws| handle_socket(ws, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut worker = Worker::new(
        format!("{}/{}", state.base_code_path, gen_random_id(10)),
        Box::new(RedisFileManager::new(state.redis_connection)),
    );

    while let Some(msg) = socket.recv().await {
        let msg = if let Ok(msg) = msg {
            let result = serde_json::from_str::<Execution>(msg.to_text().unwrap());
            let result = execute_execution(&mut worker, result.unwrap()).await;

            match result {
                Ok(res) => Message::Text(Utf8Bytes::from(serde_json::to_string(&res).unwrap())),
                Err(err) => {
                    tracing::error!("error executing code: {}", err);
                    Message::Text(Utf8Bytes::from(json!({ "error": err }).to_string()))
                }
            }
        } else {
            // client disconnected
            break;
        };

        if socket.send(msg).await.is_err() {
            break;
        }
    }

    worker.cleanup().await;
}
