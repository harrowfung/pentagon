use crate::files::{FileManagerTrait, RedisFileManager};
use crate::utils::autofix;
use std::os::unix::fs::PermissionsExt;

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};

use hakoniwa::landlock::*;
use hakoniwa::seccomp::{Action, Filter};
use hakoniwa::{Container, Namespace, Rlimit, Runctl, Stdio};

use metrics::{counter, histogram};
use std::time::Instant;

use crate::types::{Execution, ExecutionError, ExecutionFile, ExecutionResult, File, FilePath};

pub struct Worker {
    container: Container,
    path: String,
    temp_files: HashMap<u64, Vec<u8>>,
    file_manager: Box<RedisFileManager>,
}

const BANNED_SYSCALLS: &[&str] = &[
    "mount", "umount", "poweroff", "reboot", "socket", "bind", "connect", "listen", "sendto",
    "recvfrom",
];

impl Worker {
    #[tracing::instrument(skip(file_manager))]
    pub fn new(code_path: String, file_manager: Box<RedisFileManager>) -> Self {
        tracing::debug!("creating new worker");
        fs::create_dir_all(&code_path).expect("Failed to create code directory");
        let mut container = Container::new();

        container
            .unshare(Namespace::Cgroup)
            .unshare(Namespace::Ipc)
            .unshare(Namespace::Uts)
            .unshare(Namespace::Network);

        let mut ruleset = Ruleset::default();

        ruleset.restrict(Resource::FS, CompatMode::Enforce);
        ruleset.add_fs_rule("/bin", FsAccess::R | FsAccess::X);
        ruleset.add_fs_rule("/lib", FsAccess::R | FsAccess::X);
        ruleset.add_fs_rule("/usr", FsAccess::R | FsAccess::X);
        ruleset.add_fs_rule("/box", FsAccess::R | FsAccess::W | FsAccess::X);

        container.landlock_ruleset(ruleset);

        let mut filter = Filter::new(Action::Allow);

        BANNED_SYSCALLS.iter().for_each(|syscall| {
            filter.add_rule(Action::Errno(libc::SIGSYS), syscall);
        });
        container.seccomp_filter(filter);

        container.rootfs("/").expect("unable to mount root fs");
        container.bindmount_rw(&code_path, "/box");

        container.runctl(Runctl::GetProcPidStatus);
        container.runctl(Runctl::GetProcPidSmapsRollup);

        Self {
            container,
            path: code_path.to_string(),
            temp_files: HashMap::new(),
            file_manager,
        }
    }

    fn store_temp_file(&mut self, id: u64, data: Vec<u8>) {
        self.temp_files.insert(id, data);
    }

    #[tracing::instrument(skip(self, file))]
    pub async fn write_file(&mut self, file: File) -> Result<(), String> {
        match file {
            File::Local { name, content } => {
                let full_path = format!("{}/{}", self.path, name);
                let mut file = fs::File::create(&full_path).map_err(|e| e.to_string())?;
                file.write_all(&content).map_err(|e| e.to_string())?;
            }

            File::Remote { id, name } => {
                let data = self
                    .file_manager
                    .get_file(FilePath::Remote { id }, None)
                    .await?;

                let full_path = format!("{}/{}", self.path, name);
                let mut file = fs::File::create(&full_path).map_err(|e| e.to_string())?;
                file.write_all(&data).map_err(|e| e.to_string())?;
            }
        }

        counter!("files_created_total").increment(1);
        Ok(())
    }

    #[tracing::instrument(skip(self, execution), fields(program = %execution.program))]
    pub async fn execute(
        &mut self,
        execution: Execution,
    ) -> Result<ExecutionResult, ExecutionError> {
        // initalization
        let mut stdin: Option<Vec<u8>> = None;

        // copy files
        for file in execution.copy_in {
            let data = match file.from {
                FilePath::Local { name, executable } => {
                    let mut f = fs::File::open(&name).map_err(|e| e.to_string()).unwrap();
                    let mut buffer = Vec::new();
                    f.read_to_end(&mut buffer)
                        .map_err(|e| e.to_string())
                        .unwrap();

                    // if executable is true, set the executable bit
                    if executable {
                        let mut perms = fs::metadata(&name)
                            .map_err(|e| e.to_string())
                            .unwrap()
                            .permissions();
                        perms.set_mode(perms.mode() | 0o111); // set executable bits
                        fs::set_permissions(&name, perms)
                            .map_err(|e| e.to_string())
                            .unwrap();
                    }
                    buffer
                }

                FilePath::Data { content } => content,
                FilePath::Remote { id } => self
                    .file_manager
                    .get_file(FilePath::Remote { id }, None)
                    .await
                    .unwrap(),
                FilePath::Tmp { id } => {
                    if !self.temp_files.contains_key(&id) {
                        Vec::new()
                    } else {
                        self.temp_files.get(&id).unwrap().clone()
                    }
                },

                _ => {
                    return Err(ExecutionError {
                        message: "Unsupported file path for copy_in".to_string(),
                    });
                }
            };

            match file.to {
                FilePath::Local { name, executable } => {
                    let full_path = format!("{}/{}", self.path, name);
                    tracing::debug!("copying to {}", full_path);
                    let mut f = fs::File::create(&full_path)
                        .map_err(|e| e.to_string())
                        .unwrap();

                    // if executable is true, set the executable bit
                    if executable {
                        let mut perms = fs::metadata(&full_path)
                            .map_err(|e| e.to_string())
                            .unwrap()
                            .permissions();
                        perms.set_mode(perms.mode() | 0o111); // set executable bits
                        fs::set_permissions(&full_path, perms)
                            .map_err(|e| e.to_string())
                            .unwrap();
                    }
                    f.write_all(&data).map_err(|e| e.to_string()).unwrap();
                    counter!("files_created_total").increment(1);
                }
                FilePath::Tmp { id } => {
                    self.store_temp_file(id, data);
                }

                FilePath::Stdin {} => {
                    stdin = Some(data);
                }
                _ => {
                    return Err(ExecutionError {
                        message: "Unsupported file path for copy_in".to_string(),
                    });
                }
            }
        }

        // prepare execution
        self.container
            .setrlimit(Rlimit::Cpu, execution.time_limit, execution.time_limit);

        self.container.setrlimit(
            Rlimit::As,
            execution.memory_limit as u64,
            execution.memory_limit as u64,
        );

        self.container.setrlimit(
            Rlimit::Stack,
            execution.memory_limit as u64,
            execution.memory_limit as u64,
        );

        let mut cmd = self.container.command(&execution.program);
        cmd.current_dir("/box")
            .args(execution.args)
            .env("PATH", "/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        cmd.wait_timeout(execution.wall_time_limit);

        // run

        let wall_start = Instant::now();
        let mut proc = match cmd.spawn() {
            Ok(p) => p,
            Err(e) => {
                return Err(ExecutionError {
                    message: format!("Failed to spawn process: {}", e),
                });
            }
        };

        if let Some(stdin) = stdin {
            if let Some(mut proc_stdin) = proc.stdin.take() {
                std::thread::spawn(move || {
                    if let Err(_) = proc_stdin.write_all(&stdin) {
                        // return RunOutput::error("Failed to write to stdin".to_string(), None, None);
                        tracing::warn!("failed to write to stdin, process could be dead");
                    }
                    drop(proc_stdin);
                });
            } else {
                return Err(ExecutionError {
                    message: "Failed to open stdin of process".to_string(),
                });
            }
        }

        let output = match proc.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                return Err(ExecutionError {
                    message: format!("Failed to wait for process output: {}", e),
                });
            }
        };

        let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
        histogram!("execution_wall_time_ms").record(wall_ms);

        let output_status = output.status.clone();

        let resource = match output.status.rusage {
            Some(r) => Some(r),
            None => {
                tracing::warn!("failed to get resource usage: {}", output_status.reason);
                // return Err(ExecutionError {
                //     message: "failed to get resource usage".to_string(),
                // });
                None
            }
        };

        let proc_resource = match output.status.proc_pid_status {
            Some(r) => Some(r),
            None => {
                tracing::warn!(
                    "Failed to get process resource usage: {}",
                    output_status.reason
                );
                None
            }
        };

        let stdout = if execution.autofix.unwrap_or(true) {
            autofix(output.stdout.clone())
        } else {
            output.stdout.clone()
        };

        if output.status.exit_code.unwrap_or(0) == 0 {
            // only copy out files when process is successful
            for file in execution.copy_out {
                let data = match file.from {
                    FilePath::Stdout { max_size } => {
                        match max_size {
                            Some(size) => {
                                if stdout.len() > size as usize {
                                    stdout[..size as usize].to_vec()
                                } else {
                                    stdout.clone()
                                }
                            }
                            None => stdout.clone()
                        }
                    },
                    FilePath::Stderr { max_size } => {
                        match max_size {
                            Some(size) => {
                                if output.stderr.len() > size as usize {
                                    output.stderr[..size as usize].to_vec()
                                } else {
                                    output.stderr.clone()
                                }
                            }
                            None => output.stderr.clone()
                        }
                    },
                    FilePath::Local { name, executable } => {
                        let full_path = format!("{}/{}", self.path, name);
                        let f = fs::File::open(&full_path);
                        let mut buffer = Vec::new();
                        match f {
                            Ok(mut file) => {
                                file.read_to_end(&mut buffer)
                                    .map_err(|e| e.to_string())
                                    .unwrap();

                                // if executable is true, set the executable bit
                                if executable {
                                    let mut perms = fs::metadata(&full_path)
                                        .map_err(|e| e.to_string())
                                        .unwrap()
                                        .permissions();
                                    perms.set_mode(perms.mode() | 0o111); // set executable bits
                                    fs::set_permissions(&full_path, perms)
                                        .map_err(|e| e.to_string())
                                        .unwrap();
                                }
                                buffer
                            }
                            Err(e) => {
                                if executable {
                                    return Err(ExecutionError {
                                        message: format!(
                                            "failed to open file {} for copy_out: {}",
                                            full_path, e
                                        ),
                                    });
                                }
                                Vec::new()
                            }
                        }
                    }
                    _ => {
                        return Err(ExecutionError {
                            message: "Unsupported file path for copy_out".to_string(),
                        });
                    }
                };

                match file.to {
                    FilePath::Tmp { id } => {
                        self.store_temp_file(id, data);
                    }
                    FilePath::Remote { id } => {
                        self.file_manager
                            .save_file(FilePath::Remote { id }, None, data)
                            .await
                            .unwrap();
                    }

                    FilePath::Local { name, executable } => {
                        let mut f = fs::File::create(&name).map_err(|e| e.to_string()).unwrap();
                        f.write_all(&data).map_err(|e| e.to_string()).unwrap();
                        counter!("files_created_total").increment(1);

                        // if executable is true, set the executable bit
                        if executable {
                            let mut perms = fs::metadata(&name)
                                .map_err(|e| e.to_string())
                                .unwrap()
                                .permissions();
                            perms.set_mode(perms.mode() | 0o111); // set executable bits
                            fs::set_permissions(&name, perms)
                                .map_err(|e| e.to_string())
                                .unwrap();
                        }
                    }

                    _ => {
                        return Err(ExecutionError {
                            message: "Unsupported file path for copy_out".to_string(),
                        });
                    }
                }
            }
        }

        let mut return_files: Vec<ExecutionFile> = Vec::new();
        for file in execution.return_files {
            match file {
                // match all possible file paths
                FilePath::Local { name, executable } => {
                    let full_path = format!("{}/{}", self.path, name);
                    let mut f = fs::File::open(&full_path)
                        .map_err(|e| e.to_string())
                        .unwrap();
                    let mut buffer = Vec::new();
                    f.read_to_end(&mut buffer)
                        .map_err(|e| e.to_string())
                        .unwrap();

                    // if executable is true, set the executable bit
                    if executable {
                        let mut perms = fs::metadata(&full_path)
                            .map_err(|e| e.to_string())
                            .unwrap()
                            .permissions();
                        perms.set_mode(perms.mode() | 0o111); // set executable bits
                        fs::set_permissions(&full_path, perms)
                            .map_err(|e| e.to_string())
                            .unwrap();
                    }

                    return_files.push(ExecutionFile {
                        name,
                        content: buffer,
                    });
                }

                FilePath::Remote { id } => {
                    let data = self
                        .file_manager
                        .get_file(FilePath::Remote { id: id.clone() }, None)
                        .await
                        .unwrap();

                    return_files.push(ExecutionFile {
                        name: format!("remote_{}", id),
                        content: data,
                    });
                }

                FilePath::Stderr {
                    max_size,
                } => {
                    match max_size {
                        Some(size) => {
                            if output.stderr.len() > size as usize {
                                return_files.push(ExecutionFile {
                                    name: "stderr".to_string(),
                                    content: output.stderr[..size as usize].to_vec(),
                                });
                            } else {
                                return_files.push(ExecutionFile {
                                    name: "stderr".to_string(),
                                    content: output.stderr.clone(),
                                });
                            }
                        }
                        None => {
                            return_files.push(ExecutionFile {
                                name: "stderr".to_string(),
                                content: output.stderr.clone(),
                            });
                        }
                    }
                }

                FilePath::Stdout { max_size } => {
                    match max_size {
                        Some(size) => {
                            if stdout.len() > size as usize {
                                return_files.push(ExecutionFile {
                                    name: "stdout".to_string(),
                                    content: stdout[..size as usize].to_vec(),
                                });
                            } else {
                                return_files.push(ExecutionFile {
                                    name: "stdout".to_string(),
                                    content: stdout.clone(),
                                });
                            }
                        }
                        None => {
                            return_files.push(ExecutionFile {
                                name: "stdout".to_string(),
                                content: stdout.clone(),
                            });
                        }
                    }
                }

                FilePath::Tmp { id } => {
                    let data = self.temp_files.remove(&id).unwrap();
                    return_files.push(ExecutionFile {
                        name: format!("tmp_{}", id),
                        content: data,
                    });
                }

                _ => {
                    return Err(ExecutionError {
                        message: "Unsupported file path for return_files".to_string(),
                    });
                }
            }
        }

        let memory_used = match proc_resource {
            Some(res) => res.vmrss as u64,
            None => 0,
        };
        let time_used = match resource {
            Some(res) => res.user_time.as_millis() + res.system_time.as_millis(),
            None => 0,
        };

        Ok(ExecutionResult {
            exit_code: output.status.code,
            time_used,
            memory_used,
            return_files,
        })
    }

    #[tracing::instrument(skip(self))]
    pub async fn cleanup(&mut self) {
        tracing::debug!("cleaning up worker");
        let _ = fs::remove_dir_all(&self.path);
    }
}
