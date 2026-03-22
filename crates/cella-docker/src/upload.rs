//! Upload files into a running container via tar archive.

use std::path::Path;

use bollard::query_parameters::UploadToContainerOptions;
use tracing::debug;

use crate::CellaDockerError;
use crate::client::DockerClient;

/// A file to upload into a container.
pub struct FileToUpload {
    /// Absolute path inside the container.
    pub path: String,
    /// File content.
    pub content: Vec<u8>,
    /// File permissions (octal, e.g., 0o600).
    pub mode: u32,
}

impl DockerClient {
    /// Upload files into a running container via tar archive.
    ///
    /// Creates a tar archive in memory and uploads it via bollard's
    /// `upload_to_container`. Directories are created automatically.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::Io` on tar creation errors,
    /// `CellaDockerError::DockerApi` on upload errors.
    pub async fn upload_files(
        &self,
        container_id: &str,
        files: &[FileToUpload],
    ) -> Result<(), CellaDockerError> {
        if files.is_empty() {
            return Ok(());
        }

        debug!(
            "Uploading {} file(s) to container {container_id}",
            files.len()
        );

        let tar_bytes = create_tar_archive(files)?;

        self.inner()
            .upload_to_container(
                container_id,
                Some(UploadToContainerOptions {
                    path: "/".to_string(),
                    no_overwrite_dir_non_dir: Some("false".to_string()),
                    ..Default::default()
                }),
                bollard::body_full(tar_bytes.into()),
            )
            .await?;

        debug!("Upload complete");
        Ok(())
    }
}

/// Create an in-memory tar archive containing the given files.
fn create_tar_archive(files: &[FileToUpload]) -> Result<Vec<u8>, CellaDockerError> {
    let mut tar_buf = Vec::new();

    {
        let mut ar = tar::Builder::new(&mut tar_buf);

        // Collect unique parent directories
        let mut dirs: Vec<String> = files
            .iter()
            .filter_map(|f| {
                Path::new(&f.path)
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
            })
            .collect();
        dirs.sort();
        dirs.dedup();

        // Add directory entries (append_data handles GNU long name extensions
        // for paths exceeding standard tar header field sizes)
        for dir in &dirs {
            let dir_path = dir.strip_prefix('/').unwrap_or(dir);
            if dir_path.is_empty() {
                continue;
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_cksum();
            ar.append_data(&mut header, format!("{dir_path}/"), &[][..])?;
        }

        // Add file entries
        for file in files {
            let path = file.path.strip_prefix('/').unwrap_or(&file.path);
            let mut header = tar::Header::new_gnu();
            header.set_size(file.content.len() as u64);
            header.set_mode(file.mode);
            header.set_cksum();
            ar.append_data(&mut header, path, &file.content[..])?;
        }

        ar.finish()?;
    }

    Ok(tar_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_tar_with_files() {
        let files = vec![
            FileToUpload {
                path: "/home/user/.ssh/known_hosts".to_string(),
                content: b"github.com ssh-rsa AAAA...\n".to_vec(),
                mode: 0o600,
            },
            FileToUpload {
                path: "/home/user/.ssh/config".to_string(),
                content: b"Host *\n  AddKeysToAgent yes\n".to_vec(),
                mode: 0o600,
            },
        ];

        let tar_bytes = create_tar_archive(&files).unwrap();
        assert!(!tar_bytes.is_empty());

        // Verify tar contents
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let entries: Vec<_> = archive.entries().unwrap().filter_map(Result::ok).collect();

        // Should have directory entry + 2 file entries
        assert!(entries.len() >= 2);
    }

    #[test]
    fn create_tar_empty() {
        let tar_bytes = create_tar_archive(&[]).unwrap();
        // Empty tar is still valid (just the end-of-archive markers)
        assert!(!tar_bytes.is_empty());
    }

    #[test]
    fn create_tar_with_long_path() {
        // Path exceeding standard tar header name field (100 bytes)
        let long_dir = "a/".repeat(60);
        let long_path = format!("/home/user/.claude/{long_dir}file.json");
        let files = vec![FileToUpload {
            path: long_path,
            content: b"{}".to_vec(),
            mode: 0o644,
        }];

        let tar_bytes = create_tar_archive(&files).unwrap();
        assert!(!tar_bytes.is_empty());

        // Verify the file can be read back from the archive
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let found = archive.entries().unwrap().filter_map(Result::ok).any(|e| {
            e.path()
                .ok()
                .is_some_and(|p| p.to_string_lossy().contains("file.json"))
        });
        assert!(found, "Long path file should be present in archive");
    }

    #[test]
    fn create_tar_preserves_permissions() {
        let files = vec![FileToUpload {
            path: "/usr/local/bin/helper".to_string(),
            content: b"#!/bin/sh\necho hello\n".to_vec(),
            mode: 0o755,
        }];

        let tar_bytes = create_tar_archive(&files).unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);

        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path.contains("helper") {
                assert_eq!(entry.header().mode().unwrap(), 0o755);
            }
        }
    }
}
