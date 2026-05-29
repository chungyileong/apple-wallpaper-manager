use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    thread,
};

use reqwest::blocking::Client;
use thiserror::Error;

use crate::manifest::WallpaperAsset;

#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub assets: Vec<WallpaperAsset>,
    pub output_dir: PathBuf,
}

#[derive(Debug)]
pub enum DownloadEvent {
    Started {
        index: usize,
        total: usize,
        title: String,
        file_name: String,
        total_bytes: Option<u64>,
    },
    Progress {
        bytes_downloaded: u64,
        total_bytes: Option<u64>,
    },
    Finished {
        index: usize,
        path: PathBuf,
    },
    Skipped {
        path: PathBuf,
    },
    Error {
        message: String,
    },
    Complete,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("failed to create download directory: {0}")]
    CreateDir(#[from] std::io::Error),
    #[error("failed to create HTTP client: {0}")]
    Client(#[from] reqwest::Error),
}

pub fn start_downloads(plan: DownloadPlan, tx: Sender<DownloadEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = run_downloads(&plan, &tx) {
            let _ = tx.send(DownloadEvent::Error {
                message: err.to_string(),
            });
        }
        let _ = tx.send(DownloadEvent::Complete);
    })
}

fn run_downloads(plan: &DownloadPlan, tx: &Sender<DownloadEvent>) -> Result<(), DownloadError> {
    fs::create_dir_all(&plan.output_dir)?;
    let client = Client::builder()
        .user_agent("awm/0.1")
        .danger_accept_invalid_certs(true)
        .build()?;

    for (index, asset) in plan.assets.iter().enumerate() {
        let total = plan.assets.len();
        let target_path = plan.output_dir.join(&asset.file_name);
        let temp_path = target_path.with_extension(format!(
            "{}.part",
            target_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("download")
        ));

        if target_path.is_file() {
            let _ = tx.send(DownloadEvent::Skipped {
                path: target_path.clone(),
            });
            continue;
        }

        let expected_length = remote_length(&client, &asset.url);
        if is_up_to_date(&target_path, expected_length) {
            let _ = tx.send(DownloadEvent::Skipped {
                path: target_path.clone(),
            });
            continue;
        }

        let response = client.get(&asset.url).send();
        let mut response = match response {
            Ok(resp) => resp,
            Err(err) => {
                let _ = tx.send(DownloadEvent::Error {
                    message: format!("failed to request {} ({}): {}", asset.title, asset.url, err),
                });
                continue;
            }
        };

        if !response.status().is_success() {
            let _ = tx.send(DownloadEvent::Error {
                message: format!(
                    "server returned {} for {} ({})",
                    response.status(),
                    asset.title,
                    asset.url
                ),
            });
            continue;
        }

        let total_bytes = response.content_length().or(expected_length);
        let _ = tx.send(DownloadEvent::Started {
            index,
            total,
            title: asset.title.clone(),
            file_name: asset.file_name.clone(),
            total_bytes,
        });

        let result = download_stream(&mut response, &temp_path, total_bytes, tx);
        match result {
            Ok(()) => {
                fs::rename(&temp_path, &target_path)?;
                let _ = tx.send(DownloadEvent::Finished {
                    index,
                    path: target_path,
                });
            }
            Err(err) => {
                let _ = fs::remove_file(&temp_path);
                let _ = tx.send(DownloadEvent::Error {
                    message: format!(
                        "failed to download {} ({}): {}",
                        asset.title, asset.url, err
                    ),
                });
            }
        }
    }

    Ok(())
}

fn remote_length(client: &Client, url: &str) -> Option<u64> {
    let response = client.head(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.content_length()
}

fn is_up_to_date(path: &Path, expected_length: Option<u64>) -> bool {
    let Some(expected_length) = expected_length else {
        return false;
    };
    if expected_length == 0 || !path.exists() {
        return false;
    }
    fs::metadata(path)
        .map(|metadata| metadata.len() == expected_length)
        .unwrap_or(false)
}

fn download_stream<R: Read>(
    reader: &mut R,
    temp_path: &Path,
    total_bytes: Option<u64>,
    tx: &Sender<DownloadEvent>,
) -> Result<(), std::io::Error> {
    let mut file = File::create(temp_path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut downloaded = 0u64;

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;
        let _ = tx.send(DownloadEvent::Progress {
            bytes_downloaded: downloaded,
            total_bytes,
        });
    }

    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Cursor};

    #[test]
    fn streams_bytes_to_disk() {
        let temp_dir =
            std::env::temp_dir().join(format!("awm-download-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("create temp dir");

        let (tx, _rx) = std::sync::mpsc::channel();
        let input = b"wallpaper-bytes".to_vec();
        let mut cursor = Cursor::new(input.clone());
        let temp_path = temp_dir.join("sample.mov.part");
        download_stream(&mut cursor, &temp_path, Some(input.len() as u64), &tx)
            .expect("stream bytes");
        let downloaded = fs::read(&temp_path).expect("downloaded file");
        assert_eq!(downloaded, input);
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
