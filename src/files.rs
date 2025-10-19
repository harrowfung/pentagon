use crate::types::FilePath;
use redis::{AsyncCommands, aio::MultiplexedConnection};
use std::fs;

pub struct FileManager {
    connection: MultiplexedConnection,
}

pub trait FileManagerTrait {
    async fn save_file(
        &mut self,
        file_path: FilePath,
        base_path: Option<String>,
        content: Vec<u8>,
    ) -> Result<(), String>;

    async fn get_file(
        &mut self,
        file: FilePath,
        base_path: Option<String>,
    ) -> Result<Vec<u8>, String>;
}

impl FileManagerTrait for FileManager {
    async fn save_file(
        &mut self,
        file_path: FilePath,
        base_path: Option<String>,
        content: Vec<u8>,
    ) -> Result<(), String> {
        match file_path {
            FilePath::Remote { id } => {
                let _: () = self
                    .connection
                    .set(id, content)
                    .await
                    .map_err(|e| format!("Failed to save remote file: {}", e))?;
                Ok(())
            }

            FilePath::Local { name, executable } => {
                let full_path = if let Some(base) = base_path {
                    format!("{}/{}", base, name)
                } else {
                    name
                };
                fs::write(full_path.clone(), content)
                    .map_err(|e| format!("Failed to write local file: {}", e))?;

                if executable {
                    let metadata = fs::metadata(&full_path)
                        .map_err(|e| format!("Failed to get file metadata: {}", e))?;
                    let mut permissions = metadata.permissions();

                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(0o755);
                    fs::set_permissions(&full_path, permissions)
                        .map_err(|e| format!("Failed to set executable permission: {}", e))?;
                }
                Ok(())
            }

            _ => Err("Unsupported file path type for saving".to_string()),
        }
    }

    async fn get_file(
        &mut self,
        file: FilePath,
        base_path: Option<String>,
    ) -> Result<Vec<u8>, String> {
        match file {
            FilePath::Local {
                name,
                executable: _,
            } => {
                let full_path = if let Some(base) = base_path {
                    format!("{}/{}", base, name)
                } else {
                    name
                };
                let data =
                    fs::read(full_path).map_err(|e| format!("Failed to read local file: {}", e))?;
                Ok(data)
            }

            FilePath::Remote { id } => {
                let data: Vec<u8> = self
                    .connection
                    .get(id)
                    .await
                    .map_err(|e| format!("Failed to get remote file: {}", e))?;
                Ok(data)
            }

            _ => Err("Unsupported file path type".to_string()),
        }
    }
}

impl FileManager {
    pub fn new(connection: MultiplexedConnection) -> Self {
        FileManager { connection }
    }
}
