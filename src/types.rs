use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
pub enum File {
    Local { name: String, content: Vec<u8> },
    Remote { name: String, id: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
pub enum FilePath {
    Local { name: String },
    Remote { id: String },
    Stdout {},
    Stderr {},
    Stdin {},
    Tmp { id: u64 },
}

#[derive(Serialize, Deserialize)]
pub struct ExecutionTransfer {
    pub from: FilePath,
    pub to: FilePath,
}

#[derive(Serialize, Deserialize)]
pub struct Execution {
    pub program: String,                  // path to executable
    pub args: Vec<String>,                // command line arguments
    pub time_limit: u64,                  // in seconds
    pub wall_time_limit: u64,             // in seconds
    pub memory_limit: u64,                // in kilobytes
    pub copy_out: Vec<ExecutionTransfer>, // list of file names to copy out
    pub copy_in: Vec<ExecutionTransfer>,  // list of files to copy in
    pub return_files: Vec<FilePath>,      // list of files to return
    pub die_on_error: bool,               // whether to stop execution on first error
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExecutionFile {
    pub name: String,
    pub content: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub time_used: u128,                  // in milliseconds
    pub memory_used: u64,                 // in kilobytes
    pub return_files: Vec<ExecutionFile>, // list of returned files
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExecutionError {
    pub message: String,
}

#[derive(Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub executions: Vec<Execution>,
    pub files: Vec<File>,
}
