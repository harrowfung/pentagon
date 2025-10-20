# Pentagon

Pentagon is a small, sandboxed code execution service built with Rust and Axum. It runs user-supplied programs in a constrained Linux container, enforces CPU/memory/wall-clock limits, supports multi-stage executions, and can move data between the sandbox and Redis for remote file storage. It exposes:

- POST /execute — a Server-Sent Events (SSE) stream of execution results
- GET /metrics — Prometheus metrics in text exposition format

This README covers requirements, building, configuration, API usage, and examples.

---

## Contents

- Requirements
- Build
- Configuration
- Run
- API
  - Execution model
  - File types and transfers
  - Example requests
  - Reading SSE responses
- Metrics
- Security and platform notes
- Troubleshooting

---

## Requirements

Pentagon relies on Linux sandboxing features and Redis.

- OS: Linux (containers, namespaces, and seccomp are required)
  - Not supported on macOS or native Windows. If you are on Windows, use WSL2 (Ubuntu or another Linux distro) to run.
- Kernel features:
  - User namespaces, network namespaces, seccomp (and optionally Landlock, depending on kernel/support)
- Redis: a reachable Redis server (tested with Redis 6+)
- Rust: a toolchain that supports Rust 2024 edition
  - Install via https://rustup.rs
  - Keep toolchain up to date: `rustup update`
- Optional:
  - Python 3 (if you want to run the Python example)
  - `redis-cli` for quick testing of remote files

---

## Build

1) Ensure Redis is running and reachable (see Configuration).
2) Build in release mode:

```sh
cargo build --release
```

This produces the binary at `target/release/pentagon`.

---

## Configuration

Pentagon loads configuration in this order:

1) `Settings.toml` file (at the project root)
2) Environment variables prefixed with `APP_`
3) `.env` file (via dotenv) for development convenience

Defaults live in `Settings.toml`. Example:

```toml
# Settings.toml
redis_url = "redis://localhost:6379"
port = 3000
base_code_path = "/tmp/pentagon"
```

Environment overrides (any of these can be set in your shell or a `.env` file):

- `APP_REDIS_URL` — Redis connection string (e.g., `redis://localhost:6379`)
- `APP_PORT` — HTTP listen port (e.g., `3000`)
- `APP_BASE_CODE_PATH` — Host directory where Pentagon will place per-execution working directories (e.g., `/tmp/pentagon`)

Notes:
- `base_code_path` must point to a directory the service can create and clean up per-execution subdirectories in.
- Redis must be reachable at startup; otherwise the service will fail to initialize.

---

## Run

With config in place:

```sh
# using config from Settings.toml
./target/release/pentagon
# or with env overrides:
APP_PORT=3000 APP_REDIS_URL=redis://127.0.0.1:6379 ./target/release/pentagon
```

The service listens on `127.0.0.1:{port}` (loopback only). You will see a log line like:
```
listening on 127.0.0.1:3000
```

---

## API

### Overview

- POST `/execute`:
  - Request body: JSON `ExecutionRequest`
  - Response: `text/event-stream` (SSE). Each event contains a JSON payload:
    - On success: an `ExecutionResult`
    - On error: `{ "error": "..." }`
- GET `/metrics`:
  - Prometheus text format with execution/request counters and histograms

### Execution model

A single request can perform one or more `executions` (stages). Pentagon:

1) Creates a unique working directory inside `base_code_path`
2) Writes initial `files` into the working directory
3) For each `execution` in order:
   - Copies inputs into the sandbox (from local file, tmp buffer, stdin, or Redis)
   - Spawns the program with CPU/memory/wall-time limits
   - Optionally copies outputs (stdout/stderr/local file) to tmp, Redis, or a local host path
   - Optionally collects `return_files` as bytes in the SSE event payload
   - If `die_on_error` is true and the program exits non-zero, subsequent stages are skipped
4) Cleans up the working directory

### Data types

Top-level request:

```json
{
  "executions": [ /* array of Execution objects */ ],
  "files": [ /* array of File objects to preplace into /box */ ]
}
```

`File` (initial files written to the sandbox working directory `/box`):

- Local file content:
  ```json
  { "type": "local", "name": "file.txt", "content": [1, 2, 3] }
  ```
  - `content` is raw bytes as an array of integers (0–255)

- Remote file (fetched from Redis and saved as `name` inside `/box`):
  ```json
  { "type": "remote", "name": "input.txt", "id": "my-redis-key" }
  ```

`FilePath` (locations used in copy_in/copy_out/return_files):

- Local path inside sandbox working dir (relative to `/box`):
  ```json
  { "type": "local", "name": "relative/path/in/box.txt", "executable": false }
  ```
- Remote Redis object:
  ```json
  { "type": "remote", "id": "my-redis-key" }
  ```
- Standard streams:
  ```json
  { "type": "stdin" }   // only valid as a "to" target in copy_in
  { "type": "stdout" }  // only valid as a "from" source in copy_out or return_files
  { "type": "stderr" }  // only valid as a "from" source in copy_out or return_files
  ```
- Tmp buffer (in-memory between stages, identified by a numeric id):
  ```json
  { "type": "tmp", "id": 1 }
  ```

`ExecutionTransfer`:
```json
{
  "from": { /* FilePath */ },
  "to":   { /* FilePath */ }
}
```

`Execution`:
```json
{
  "program": "/usr/bin/python3",
  "args": ["-c", "print('hello')"],
  "time_limit": 1,            // seconds (CPU time)
  "wall_time_limit": 2,       // seconds (wall clock timeout)
  "memory_limit": 268435456,  // kilobytes
  "copy_in": [ /* ExecutionTransfer[] */ ],
  "copy_out": [ /* ExecutionTransfer[] */ ],
  "return_files": [ /* FilePath[] */ ],
  "die_on_error": true
}
```

`ExecutionResult` (emitted per stage as an SSE event on success):
```json
{
  "exit_code": 0,
  "time_used": 5,     // milliseconds (user + system CPU time)
  "memory_used": 1234, // kilobytes (VmRSS)
  "return_files": [
    { "name": "stdout", "content": [ /* bytes */ ] },
    { "name": "stderr", "content": [ /* bytes */ ] }
  ]
}
```

On error, Pentagon emits an event with:
```json
{ "error": "failed to execute code: ..." }
```

### Example: two-stage Python pipeline

This example:
1) Stage 1 reads a number from stdin, prints a^a to stdout, and stores stdout in tmp(1)
2) Stage 2 reads tmp(1) on stdin and prints `answer: <value>`, storing stdout in Redis key `output`

First, put input into Redis key `input`:

```sh
redis-cli set input 3
```

Then POST to `/execute`:

```sh
curl -N -H "Content-Type: application/json" \
  --data @pentagon/sample.json \
  http://127.0.0.1:3000/execute
```

The `pentagon/sample.json` provided in the repo looks like:

```json
{
  "executions": [
    {
      "program": "/usr/bin/python3",
      "args": ["-c", "a = int(input())\nprint(a ** a)"],
      "time_limit": 1,
      "wall_time_limit": 2,
      "memory_limit": 268435456,
      "copy_out": [
        { "from": { "type": "stdout" }, "to": { "type": "tmp", "id": 1 } }
      ],
      "copy_in": [
        { "from": { "type": "remote", "id": "input" }, "to": { "type": "stdin" } }
      ],
      "return_files": [
        { "type": "stderr" }
      ],
      "die_on_error": true
    },
    {
      "program": "/usr/bin/python3",
      "args": ["-c", "print(\"answer:\", input())"],
      "time_limit": 1,
      "wall_time_limit": 2,
      "memory_limit": 268435456,
      "copy_out": [
        { "from": { "type": "stdout" }, "to": { "type": "remote", "id": "output" } }
      ],
      "copy_in": [
        { "from": { "type": "tmp", "id": 1 }, "to": { "type": "stdin" } }
      ],
      "return_files": [],
      "die_on_error": true
    }
  ],
  "files": [
    { "type": "remote", "name": "candle.h", "id": "X002-candle.h" }
  ]
}
```

- The `files` entry demonstrates that you can fetch a remote object from Redis and place it in the working directory before any executions start. It’s not used by the Python snippet, but shows how remote files are materialized into `/box`.

After it finishes, you can check what Stage 2 wrote to Redis:

```sh
redis-cli get output
```

### Minimal one-stage example

Execute `echo` and return stdout:

```json
{
  "executions": [
    {
      "program": "/bin/sh",
      "args": ["-c", "echo hello"],
      "time_limit": 1,
      "wall_time_limit": 2,
      "memory_limit": 65536,
      "copy_in": [],
      "copy_out": [],
      "return_files": [{ "type": "stdout" }],
      "die_on_error": true
    }
  ],
  "files": []
}
```

Send it:

```sh
curl -N -H "Content-Type: application/json" \
  -d @/path/to/minimal.json \
  http://127.0.0.1:3000/execute
```

### Reading the SSE stream

The response uses SSE with default event type and data lines containing a JSON string:

- Success event: JSON of `ExecutionResult`
- Error event: `{"error":"..."}`

Examples of clients:
- curl: `curl -N http://127.0.0.1:3000/execute -d @req.json -H 'Content-Type: application/json'`
- Browsers/EventSource: open a stream and `JSON.parse(event.data)` for each message
- Any SSE client library in your language

---

## Metrics

GET `/metrics` exposes Prometheus metrics. Notable series include:

- `requests_total` (counter): total number of `/execute` requests
- `executions_total{outcome="ok"|"error"}` (counter): total executed programs by outcome
- `execution_time_ms` (histogram): CPU time used (user + system) in milliseconds
- `execution_memory_kb` (histogram): memory (VmRSS) in kilobytes
- `execution_wall_time_ms` (histogram): wall-clock time in milliseconds for a spawned process

Scrape example:
```
scrape_configs:
  - job_name: 'pentagon'
    static_configs:
      - targets: ['127.0.0.1:3000']
    metrics_path: /metrics
```

---


## Security and platform notes

IMPORTANT SECURITY DISCLAIMER: No sandbox is perfectly secure. Running untrusted code is inherently risky and may result in data exfiltration, resource abuse, or system compromise. Always treat code and artifacts processed by this service as potentially malicious. Operate Pentagon in an isolated environment with least privilege, limit filesystem and network access on the host, monitor usage, and keep your kernel and dependencies up to date.


- Linux-only: relies on namespaces and seccomp. The code:
  - Unshares cgroup, IPC, UTS, and network namespaces (network is disabled inside the sandbox)
  - Applies a seccomp filter that blocks dangerous syscalls
  - Constrains CPU time, address space, and wall time
  - Executes in a bind-mounted working directory (`/box`)
- Privileges: depending on your distribution, you may need to enable unprivileged user namespaces or run in an environment where they are permitted.
- Do not run the service as root unless you understand the implications.
- Always run in an isolated host (container/VM) if evaluating untrusted code.

---

## Troubleshooting

- Cannot connect to Redis:
  - Check `redis_url` in `Settings.toml` or `APP_REDIS_URL`
  - Verify `redis-cli PING` succeeds
- Permission errors about namespaces/seccomp:
  - Ensure your kernel supports user and network namespaces and seccomp
  - Some distros require enabling unprivileged user namespaces:
    - e.g., `sysctl kernel.unprivileged_userns_clone=1` (varies by distro/policy)
- Program not found:
  - Ensure the `program` path exists inside the containerized environment (rootfs is `/`, PATH is `/bin`)
- Non-zero exit code:
  - If `die_on_error` is true, later stages will not run. Inspect stderr via `return_files` or `copy_out` → `stderr`.

---

## Development

- Logging: emitted via `tracing_subscriber::fmt`. Run the binary directly to see logs on stdout/stderr.
- Env overrides: use a `.env` file for local development (e.g., `APP_PORT=3000`).
- Clean working directories are removed automatically after each request.

---

## License


Released under the MIT License. See LICENSE for details.
