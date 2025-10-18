use crate::{
    files::FileManager,
    types::{ExecutionRequest, ExecutionResult},
    worker::Worker,
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use config::Config;
use dotenvy::dotenv;
use redis::aio::MultiplexedConnection;

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
    // file_manager: Box<dyn FileManagerTrait + Send + Sync>,
    redis_connection: MultiplexedConnection,
    base_code_path: String,
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

async fn execute_code(
    State(state): State<AppState>,
    Json(payload): Json<ExecutionRequest>,
) -> Result<Json<Vec<ExecutionResult>>, (StatusCode, String)> {
    let mut worker = Worker::new(
        format!("{}/{}", state.base_code_path, gen_random_id(10)),
        Box::new(FileManager::new(state.redis_connection)),
    );
    for file in payload.files {
        if let Err(e) = worker.write_file(file).await {
            tracing::error!("error writing file: {}", e);
            worker.cleanup().await;

            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to write file".to_string(),
            ));
        }
    }

    let mut results = Vec::new();

    for request in payload.executions {
        let die_on_error = request.die_on_error;

        let result = worker.execute(request).await;

        if let Err(e) = &result {
            tracing::error!("error executing code: {}", e.message);
            worker.cleanup().await;

            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to execute code: {}", e.message),
            ));
        }

        let result = result.unwrap();

        let exit_code = result.exit_code.clone();
        results.push(result);

        if die_on_error && exit_code != 0 {
            break;
        }
    }

    worker.cleanup().await;

    Ok(Json(results))
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

    let client = redis::Client::open(app_config.redis_url).unwrap();
    let con = client.get_multiplexed_async_connection().await.unwrap();
    let app = Router::new()
        .route("/execute", post(execute_code))
        .with_state(AppState {
            // file_manager,
            redis_connection: con,
            base_code_path: app_config.base_code_path.clone(),
        });

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", app_config.port))
        .await
        .unwrap();

    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();

    // let mut worker = Worker::new("/tmp/code-runner".to_string(), file_manager);

    // for file in sample_requests.files {
    //     worker.write_file(file).unwrap();
    // }
}
