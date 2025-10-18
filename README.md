# Pentagon Execute API

A minimal HTTP service to safely execute processes inside a constrained Linux container and exchange data via stdin/stdout, local files, temporary buffers, and Redis-backed "remote" files.

This service exposes a single endpoint `POST /execute` that:
- Materializes a per-request working directory mounted at `/box` inside an isolated container
- Writes any provided files into that working directory
- Runs one or more executions sequentially with CPU time, wall-clock time, and memory limits
- Supports moving data between executions via:
  - stdin/stdout/stderr
  - local files (inside `/box`)
  - temporary in-memory buffers (identified by numeric IDs)
  - Redis-backed remote blobs (identified by string IDs)
- Returns per-execution results with exit code, resource usage, and requested outputs

The types and wire format are JSON-compatible with TypeScript types defined below.


## Contents

- What you get
- Requirements
- Quick start
- Configuration
- API contract
  - TypeScript types
  - Semantics
  - Supported file path combinations
- Examples
  - Simple echo with stdin/stdout
  - Two-step pipeline via tmp
  - Using remote files via Redis
  - cURL
  - Node.js/TypeScript client
- Notes and limitations
- Development tips


## What you get

- A Rust/Axum server with `POST /execute`
- Linux-seccomp isolated execution using `hakoniwa`
- Redis integration for remote file storage
- Example payload in `sample.json`
- Configurable base working directory, port, and Redis URL


## Requirements

- OS: Linux (namespaces + seccomp are Linux-only). On macOS/Windows, use WSL2 or a Linux VM.
- Redis server accessible at the configured URL (default: `redis://localhost:6379`)
- Rust toolchain (stable)
- Python/bash or other binaries in the host filesystem if you want to execute them (use absolute paths)


## Quick start

1) Clone and enter the project directory, then build and run:
   ~~~bash
   cargo run
   ~~~

   The server binds to 127.0.0.1 on the configured port (default: 3000).

2) Ensure Redis is running:
   ~~~bash
   # On Debian/Ubuntu
   sudo apt-get install redis-server
   sudo service redis-server start

   # Or with Docker
   docker run --rm -p 6379:6379 redis:7
   ~~~

3) Optionally, edit `Settings.toml` (see configuration below).

4) Send a request (see examples).


## Configuration

Settings are loaded from `Settings.toml` and can be overridden via environment variables with the `APP_` prefix.

- `Settings.toml`:
  ~~~toml
  redis_url = "redis://localhost:6379"
  port = 3000
  base_code_path = "/tmp/pentagon"
  ~~~

- Environment overrides:
  - `APP_REDIS_URL`
  - `APP_PORT`
  - `APP_BASE_CODE_PATH`

The server binds to `127.0.0.1:<port>` and creates a per-request job directory under `base_code_path`.


## API contract

- Endpoint: `POST /execute`
- Request body: JSON matching `ExecutionRequest`
- Response:
  - HTTP 200: JSON array of `ExecutionResult` (one per execution, in order)
  - Non-200: Plain-text error message

Note on binary content: byte arrays are represented as JSON arrays of numbers (not base64).


### TypeScript types

Copy these into your client to strongly type requests/responses.

~~~ts
// Byte content is a JSON array of numbers, not base64.
export type ByteArray = number[];

// File staging into /box before executions start
export type File =
  | { type: 'local'; name: string; content: ByteArray }
  | { type: 'remote'; name: string; id: string };

// File path targets/sources for transfers and returns
export type FilePath =
  | { type: 'local'; name: string }
  | { type: 'remote'; id: string }
  | { type: 'stdout' }
  | { type: 'stderr' }
  | { type: 'stdin' }
  | { type: 'tmp'; id: number };

export interface ExecutionTransfer {
  from: FilePath;
  to: FilePath;
}

export interface Execution {
  // Absolute path to the executable inside the containerized host root
  // e.g. "/usr/bin/python3", "/bin/sh"
  program: string;
  args: string[];

  // Limits
  time_limit: number;       // seconds of CPU time
  wall_time_limit: number;  // seconds of wall clock time
  memory_limit: number;     // kilobytes (KB)

  // Data movement
  copy_out: ExecutionTransfer[];
  copy_in: ExecutionTransfer[];

  // Files to include in the response (after the process exits)
  return_files: FilePath[];

  // Present in schema but not enforced to short-circuit across executions in current server
  die_on_error: boolean;
}

export interface ExecutionFile {
  name: string;
  content: ByteArray;
}

export interface ExecutionResult {
  exit_code: number;
  time_used: number;      // milliseconds (CPU user + system)
  memory_used: number;    // kilobytes (VmRSS)
  return_files: ExecutionFile[];
}

export interface ExecutionRequest {
  executions: Execution[];
  files: File[]; // Pre-staged into /box
}
~~~

Helpers for encoding/decoding text:

~~~ts
export const toBytes = (s: string) => Array.from(new TextEncoder().encode(s));
export const fromBytes = (b: number[]) => new TextDecoder().decode(new Uint8Array(b));
~~~


### Semantics

- Working directory inside the container is `/box`. Any `local` file path is relative to `/box`.
- `files` are written before the first execution:
  - `local`: stored under `/box/<name>`
  - `remote`: fetched from Redis key `id` and written under `/box/<name>`
- Executions run sequentially in the same `/box` workspace, so outputs from one can be inputs to the next (via `tmp`, `local` files, or `remote`).
- `copy_in` runs before starting a process:
  - You can load bytes from `local`, `remote`, or `tmp` and route them into `stdin`, `tmp`, or a `local` destination file.
- `copy_out` runs after the process exits:
  - You can capture from `stdout`, `stderr`, or a `local` file and route them into `tmp` or `remote`.
- `return_files` runs last and bundles selected items into the HTTP response.
- Limits:
  - `time_limit` sets CPU time (seconds).
  - `wall_time_limit` sets wall-clock timeout (seconds).
  - `memory_limit` is in kilobytes.
- Binaries must exist in the host rootfs paths; use absolute paths (e.g. `/usr/bin/python3`). The container’s `PATH` is set to `/bin` only.
- Networking is disabled; several dangerous syscalls are blocked.
- On execution failure, the server currently returns a non-200 response with a plain-text error message. Results are not returned for that execution.


### Supported file path combinations

- copy_in:
  - from:
    - `local{name}`, `remote{id}`, `tmp{id}`
  - to:
    - `local{name}`, `tmp{id}`, `stdin`
- copy_out:
  - from:
    - `stdout`, `stderr`, `local{name}`
  - to:
    - `tmp{id}`, `remote{id}`
- return_files:
  - any of: `local{name}`, `remote{id}`, `stdout`, `stderr`, `tmp{id}`


## Examples

### 1) Simple echo with stdin/stdout

This runs `/bin/sh -c 'cat'`, feeds stdin, and returns stdout.

~~~json
{
  "files": [],
  "executions": [
    {
      "program": "/bin/sh",
      "args": ["-c", "cat"],
      "time_limit": 1,
      "wall_time_limit": 2,
      "memory_limit": 65536,
      "copy_in": [
        { "from": { "type": "local", "name": "input.txt" }, "to": { "type": "stdin" } }
      ],
      "copy_out": [],
      "return_files": [{ "type": "stdout" }],
      "die_on_error": true
    }
  ]
}
~~~

Note: You must pre-stage `input.txt` via `files`:
~~~json
{
  "files": [
    { "type": "local", "name": "input.txt", "content": [72,101,108,108,111,10] }
  ],
  "executions": [ /* ... as above ... */ ]
}
~~~


### 2) Two-step pipeline via tmp

This mirrors `sample.json` with smaller, clear limits:
- Step 1 runs `python3` that raises an input number to the power of itself, and stores stdout into `tmp:1`.
- Step 2 reads `tmp:1` as stdin and writes the final stdout to Redis key `output`.

~~~json
{
  "files": [
    { "type": "remote", "name": "candle.h", "id": "X002-candle.h" }
  ],
  "executions": [
    {
      "program": "/usr/bin/python3",
      "args": ["-c", "a = int(input())\nprint(a ** a)"],
      "time_limit": 1,
      "wall_time_limit": 2,
      "memory_limit": 262144,
      "copy_in": [
        { "from": { "type": "remote", "id": "input" }, "to": { "type": "stdin" } }
      ],
      "copy_out": [
        { "from": { "type": "stdout" }, "to": { "type": "tmp", "id": 1 } }
      ],
      "return_files": [{ "type": "stderr" }],
      "die_on_error": true
    },
    {
      "program": "/usr/bin/python3",
      "args": ["-c", "print('answer:', input())"],
      "time_limit": 1,
      "wall_time_limit": 2,
      "memory_limit": 262144,
      "copy_in": [
        { "from": { "type": "tmp", "id": 1 }, "to": { "type": "stdin" } }
      ],
      "copy_out": [
        { "from": { "type": "stdout" }, "to": { "type": "remote", "id": "output" } }
      ],
      "return_files": [],
      "die_on_error": true
    }
  ]
}
~~~


### 3) Using remote files via Redis

Populate input:
~~~bash
redis-cli -u redis://localhost:6379 SET input "5\n"
~~~

After the second execution above, read the result:
~~~bash
redis-cli -u redis://localhost:6379 GET output
# -> "answer: 3125\n"
~~~


### 4) cURL

Send a request (example reusing the two-step pipeline):

~~~bash
curl -sS -X POST http://127.0.0.1:3000/execute \
  -H "Content-Type: application/json" \
  --data '{
    "files": [],
    "executions": [
      {
        "program": "/bin/sh",
        "args": ["-c", "cat"],
        "time_limit": 1,
        "wall_time_limit": 2,
        "memory_limit": 65536,
        "copy_in": [
          { "from": { "type": "remote", "id": "input" }, "to": { "type": "stdin" } }
        ],
        "copy_out": [
          { "from": { "type": "stdout" }, "to": { "type": "tmp", "id": 1 } }
        ],
        "return_files": [],
        "die_on_error": true
      },
      {
        "program": "/bin/sh",
        "args": ["-c", "echo answer: $(cat)"],
        "time_limit": 1,
        "wall_time_limit": 2,
        "memory_limit": 65536,
        "copy_in": [
          { "from": { "type": "tmp", "id": 1 }, "to": { "type": "stdin" } }
        ],
        "copy_out": [
          { "from": { "type": "stdout" }, "to": { "type": "remote", "id": "output" } }
        ],
        "return_files": [],
        "die_on_error": true
      }
    ]
  }'
~~~


### 5) Node.js/TypeScript client

A minimal client using native `fetch` (Node 18+):

~~~ts
import type {
  ByteArray, File, FilePath, ExecutionTransfer,
  Execution, ExecutionRequest, ExecutionResult
} from './types'; // or inline the types from above

const toBytes = (s: string) => Array.from(new TextEncoder().encode(s));
const fromBytes = (b: number[]) => new TextDecoder().decode(new Uint8Array(b));

async function execute(req: ExecutionRequest): Promise<ExecutionResult[]> {
  const res = await fetch('http://127.0.0.1:3000/execute', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req)
  });

  if (!res.ok) {
    const text = await res.text();
    throw new Error(`Execute API error ${res.status}: ${text}`);
  }

  return res.json() as Promise<ExecutionResult[]>;
}

(async () => {
  const request: ExecutionRequest = {
    files: [
      { type: 'local', name: 'input.txt', content: toBytes('hello\n') }
    ],
    executions: [
      {
        program: '/bin/sh',
        args: ['-c', 'cat'],
        time_limit: 1,
        wall_time_limit: 2,
        memory_limit: 65536,
        copy_in: [
          { from: { type: 'local', name: 'input.txt' }, to: { type: 'stdin' } }
        ],
        copy_out: [],
        return_files: [{ type: 'stdout' }],
        die_on_error: true
      }
    ]
  };

  const results = await execute(request);
  for (const r of results) {
    console.log('exit:', r.exit_code, 'time(ms):', r.time_used, 'mem(KB):', r.memory_used);
    for (const f of r.return_files) {
      console.log(f.name, '=>', JSON.stringify(fromBytes(f.content)));
    }
  }
})();
~~~


## Notes and limitations

- OS/Container:
  - Uses Linux namespaces and seccomp. Not supported on non-Linux hosts without WSL/VM.
  - Networking is disabled and several syscalls are blocked (e.g., socket/connect/mount).
- Paths and binaries:
  - Use absolute `program` paths (e.g. `/usr/bin/python3`). The container `PATH` is `/bin` only.
  - `local` paths are relative to `/box`.
- Resource limits:
  - `time_limit` (CPU seconds) and `wall_time_limit` (wall seconds) are enforced.
  - `memory_limit` is in KB.
- Error behavior:
  - On failure to write files or execution issues, the server returns a non-200 response with a plain-text message.
  - The `die_on_error` flag exists in the schema but is not currently used to short-circuit the batch; treat each execution independently in your client and handle HTTP errors.
- Binary content:
  - Byte arrays are JSON arrays of numbers. Use `TextEncoder/Decoder` to convert text when convenient.
- Security:
  - This is an educational/minimal runner. Review threat models and harden further before using in production.


## Development tips

- Logging:
  - The server logs its listening address on startup.
- Sample payload:
  - See `sample.json` for a multi-step example using remote and tmp storage.
- Testing iteratively:
  - Start Redis, run the server, and use cURL or the Node client to iterate on payloads.
- Cleanup:
  - Each request uses a random subdirectory under `base_code_path`. Clean up as needed if you generate many requests.


Happy hacking!