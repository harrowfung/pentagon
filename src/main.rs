use redis::Client;

use crate::{
    files::FileManager,
    types::{Execution, ExecutionRequest, ExecutionTransfer, File, FilePath},
    worker::Worker,
};

mod files;
mod types;
mod worker;

fn main() {
    let client = Client::open("redis://localhost:6379/").expect("unable to connect to redis");
    let con = client
        .get_connection()
        .expect("unable to get redis connection");

    let file_manager = Box::new(FileManager::new(Box::new(con)));
    let mut worker = Worker::new("/tmp/code-runner".to_string(), file_manager);

    let sample_requests: ExecutionRequest = ExecutionRequest {
        executions: vec![
            Execution {
                program: "/usr/bin/python3".to_string(),
                args: vec![
                    "-c".to_string(),
                    "a = int(input())\nprint(a ** a)".to_string(),
                ],
                time_limit: 1,
                memory_limit: 256 * 1024 * 1024,
                wall_time_limit: 2,
                copy_in: vec![ExecutionTransfer {
                    from: FilePath::Remote {
                        id: "input".to_string(),
                    },
                    to: FilePath::Stdin {},
                }],
                copy_out: vec![ExecutionTransfer {
                    from: FilePath::Stdout {},
                    to: FilePath::Tmp { id: 1 },
                }],
                return_files: vec![FilePath::Stderr {}],
                die_on_error: true,
            },
            Execution {
                program: "/usr/bin/python3".to_string(),
                args: vec!["-c".to_string(), "print(\"answer:\", input())".to_string()],
                time_limit: 1,
                memory_limit: 256 * 1024 * 1024,
                wall_time_limit: 2,
                copy_in: vec![ExecutionTransfer {
                    from: FilePath::Tmp { id: 1 },
                    to: FilePath::Stdin {},
                }],
                copy_out: vec![ExecutionTransfer {
                    from: FilePath::Stdout {},
                    to: FilePath::Remote {
                        id: "output".to_string(),
                    },
                }],
                return_files: vec![],
                die_on_error: true,
            },
        ],
        files: vec![File::Remote {
            name: "candle.h".to_string(),
            id: "X002-candle.h".to_string(),
        }],
    };

    // serealize the request
    let serialized = serde_json::to_string(&sample_requests).unwrap();
    // store at sample.json
    std::fs::write("sample.json", &serialized).unwrap();

    for file in sample_requests.files {
        worker.write_file(file).unwrap();
    }

    for request in sample_requests.executions {
        let result = worker.execute(request).unwrap();
        dbg!("Execution result: {:?}", result);
    }
}
