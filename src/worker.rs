use crate::files::FileManagerTrait;

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};

use hakoniwa::seccomp::{Action, Arch, Filter};
use hakoniwa::{Container, Namespace, Rlimit, Runctl, Stdio};

use crate::files::FileManager;
use crate::types::{Execution, ExecutionError, ExecutionFile, ExecutionResult, File, FilePath};

pub struct Worker {
    container: Container,
    path: String,
    temp_files: HashMap<u64, Vec<u8>>,
    file_manager: Box<FileManager>,
}

const BANNED_SYSCALLS: &[&str] = &[
    "mount", "umount", "poweroff", "reboot", "socket", "bind", "connect", "listen", "sendto",
    "recvfrom",
];

impl Worker {
    pub fn new(code_path: String, file_manager: Box<FileManager>) -> Self {
        fs::create_dir_all(&code_path).expect("Failed to create code directory");
        let mut container = Container::new();

        let mut filter = Filter::new(Action::Allow);

        #[cfg(target_arch = "x86_64")]
        {
            filter.add_arch(Arch::X8664);
            filter.add_arch(Arch::X86);
            filter.add_arch(Arch::X32);
        }

        container
            .unshare(Namespace::Cgroup)
            .unshare(Namespace::Ipc)
            .unshare(Namespace::Uts)
            .unshare(Namespace::Network);

        BANNED_SYSCALLS.iter().for_each(|syscall| {
            filter.add_rule(Action::Errno(libc::SIGSYS), syscall);
        });

        container.rootfs("/").expect("unable to mount root fs");
        container.seccomp_filter(filter);

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

        Ok(())
    }

    pub async fn execute(
        &mut self,
        execution: Execution,
    ) -> Result<ExecutionResult, ExecutionError> {
        // initalization
        let mut stdin: Option<Vec<u8>> = None;

        // copy files
        for file in execution.copy_in {
            let data = match file.from {
                FilePath::Local { name } => {
                    let mut f = fs::File::open(&name).map_err(|e| e.to_string()).unwrap();
                    let mut buffer = Vec::new();
                    f.read_to_end(&mut buffer)
                        .map_err(|e| e.to_string())
                        .unwrap();
                    buffer
                }
                FilePath::Remote { id } => self
                    .file_manager
                    .get_file(FilePath::Remote { id }, None)
                    .await
                    .unwrap(),
                FilePath::Tmp { id } => self.temp_files.get(&id).unwrap().clone(),

                _ => {
                    return Err(ExecutionError {
                        message: "Unsupported file path for copy_in".to_string(),
                    });
                }
            };

            match file.to {
                FilePath::Local { name } => {
                    let full_path = format!("{}/{}", self.path, name);
                    let mut f = fs::File::create(&full_path)
                        .map_err(|e| e.to_string())
                        .unwrap();
                    f.write_all(&data).map_err(|e| e.to_string()).unwrap();
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

        let mut cmd = self.container.command(&execution.program);
        cmd.current_dir("/box")
            .args(execution.args)
            .env("PATH", "/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        cmd.wait_timeout(execution.wall_time_limit);

        // run

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
                if let Err(_) = proc_stdin.write_all(&stdin) {
                    // return RunOutput::error("Failed to write to stdin".to_string(), None, None);
                    eprintln!("warning: failed to write to stdin, process could be dead");
                }
                drop(proc_stdin);
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

        let output_status = output.status.clone();

        let resource = match output.status.rusage {
            Some(r) => r,
            None => {
                eprintln!("failed to get resource usage: {}", output_status.reason);
                return Err(ExecutionError {
                    message: "failed to get resource usage".to_string(),
                });
            }
        };

        let proc_resource = match output.status.proc_pid_status {
            Some(r) => r,
            None => {
                eprintln!(
                    "Failed to get process resource usage: {}",
                    output_status.reason
                );
                return Err(ExecutionError {
                    message: "failed to get process resource usage".to_string(),
                });
            }
        };

        for file in execution.copy_out {
            let data = match file.from {
                FilePath::Stdout {} => output.stdout.clone(),
                FilePath::Stderr {} => output.stderr.clone(),
                FilePath::Local { name } => {
                    let full_path = format!("{}/{}", self.path, name);
                    let mut f = fs::File::open(&full_path).map_err(|e| ExecutionError {
                        message: format!("failed to open {}: {}", &full_path, e),
                    })?;
                    let mut buffer = Vec::new();
                    f.read_to_end(&mut buffer)
                        .map_err(|e| e.to_string())
                        .unwrap();
                    buffer
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

                FilePath::Local { name } => {
                    let mut f = fs::File::create(&name).map_err(|e| e.to_string()).unwrap();
                    f.write_all(&data).map_err(|e| e.to_string()).unwrap();
                }

                _ => {
                    return Err(ExecutionError {
                        message: "Unsupported file path for copy_out".to_string(),
                    });
                }
            }
        }

        let mut return_files: Vec<ExecutionFile> = Vec::new();
        for file in execution.return_files {
            match file {
                // match all possible file paths
                FilePath::Local { name } => {
                    let full_path = format!("{}/{}", self.path, name);
                    let mut f = fs::File::open(&full_path)
                        .map_err(|e| e.to_string())
                        .unwrap();
                    let mut buffer = Vec::new();
                    f.read_to_end(&mut buffer)
                        .map_err(|e| e.to_string())
                        .unwrap();

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

                FilePath::Stderr {} => {
                    return_files.push(ExecutionFile {
                        name: "stderr".to_string(),
                        content: output.stderr.clone(),
                    });
                }

                FilePath::Stdout {} => {
                    return_files.push(ExecutionFile {
                        name: "stdout".to_string(),
                        content: output.stdout.clone(),
                    });
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

        Ok(ExecutionResult {
            exit_code: output.status.code,
            time_used: resource.user_time.as_millis() + resource.system_time.as_millis(),
            memory_used: proc_resource.vmrss as u64,
            return_files,
        })
    }

    pub async fn cleanup(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
