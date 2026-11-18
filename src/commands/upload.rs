use crate::client;
use crate::config;
use anyhow::{bail, Context, Result};
use cloudreve_sdk_api::error::{ApiError, ErrorCode};
use cloudreve_sdk_uploader::{
    Tracker, UploadEvent, UploadParams, UploadSource, Uploader, UploaderConfig,
};
use futures::StreamExt;
use indicatif::{HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressStyle};
use std::iter;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use walkdir::WalkDir;

pub async fn run(local: &PathBuf, remote: &str, recursive: bool, overwrite: bool) -> Result<()> {
    let config = config::load()?;
    let server_config = config.default.resolve();

    println!("Connecting to {}...", server_config.server);

    let (client, login_response) = client::connect(
        &server_config.server,
        &server_config.email,
        &server_config.password,
    )
    .await
    .context("Login failed")?;

    println!("Logged in as: {}", login_response.user.nickname);

    let uploader = Uploader::new(Arc::new(client), UploaderConfig::default());

    let normalized_remote = normalize_remote_path(remote);

    let upload_items = create_upload_stream(local, recursive, &normalized_remote);

    let multi_progress = Arc::new(MultiProgress::new());

    let style = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} {msg}"
    )?
    .progress_chars("#>-");

    let mut success_count = 0;
    let mut skipped_count = 0;
    let mut fail_count = 0;

    for item_result in upload_items {
        let (local_path, remote_uri) = item_result?;

        let pb = multi_progress.add(ProgressBar::new(0));
        pb.set_style(style.clone());

        let label = local_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();

        match upload_file(&uploader, &local_path, &remote_uri, overwrite, &label, &pb).await {
            Ok(UploadOutcome::Uploaded) => {
                success_count += 1;
                println!("Uploaded: {}", local_path.display());
            }
            Ok(UploadOutcome::Skipped) => {
                skipped_count += 1;
                println!(
                    "Skipped: {} (remote file exists, use --overwrite to replace)",
                    local_path.display()
                );
            }
            Err(e) => {
                fail_count += 1;
                eprintln!("Failed: {} - {}", local_path.display(), e);
            }
        }
    }

    println!(
        "\nUpload complete: {} succeeded, {} skipped, {} failed",
        success_count, skipped_count, fail_count
    );

    if fail_count > 0 {
        bail!("{} file(s) failed to upload", fail_count);
    }

    Ok(())
}

fn normalize_remote_path(remote_path: &str) -> String {
    if remote_path.starts_with("cloudreve://") {
        remote_path.to_string()
    } else if remote_path.starts_with('/') {
        format!("cloudreve://my{}", remote_path)
    } else {
        format!("cloudreve://my/{}", remote_path.trim_start_matches('/'))
    }
}

fn create_upload_stream<'a>(
    local: &'a PathBuf,
    recursive: bool,
    base_remote_path: &'a str,
) -> Box<dyn Iterator<Item = Result<(PathBuf, String)>> + 'a> {
    if local.is_dir() {
        if !recursive {
            Box::new(iter::once(Err(anyhow::anyhow!("{} is a directory, use -r for recursive upload", local.display()))))
        } else {
            Box::new(
                WalkDir::new(local)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|entry| entry.path().is_file())
                    .map(move |entry| {
                        let path = entry.path();
                        let relative = path.strip_prefix(local)
                            .context("Failed to compute relative path")?;
                        let remote_uri = format!("{}/{}", base_remote_path.trim_end_matches('/'), relative.to_string_lossy());
                        Ok((path.to_path_buf(), remote_uri))
                    }),
            )
        }
    } else if local.is_file() {
        let file_name = local.file_name()
            .context("File has no name")
            .map(|name| name.to_string_lossy().to_string());
        match file_name {
            Ok(name) => {
                let remote_uri = format!("{}/{}", base_remote_path.trim_end_matches('/'), name);
                Box::new(iter::once(Ok((local.clone(), remote_uri))))
            }
            Err(e) => Box::new(iter::once(Err(e)))
        }
    } else {
        Box::new(iter::once(Err(anyhow::anyhow!("File not found: {}", local.display()))))
    }
}

enum UploadOutcome {
    Uploaded,
    Skipped,
}

async fn upload_file(
    uploader: &Uploader,
    local_path: &PathBuf,
    remote_uri: &str,
    overwrite: bool,
    label: &str,
    pb: &ProgressBar,
) -> Result<UploadOutcome> {
    let last_modified = tokio::fs::metadata(local_path)
        .await?
        .modified()
        .ok()
        .and_then(|t| {
            use std::time::UNIX_EPOCH;
            t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
        });

    let params = UploadParams {
        source: UploadSource::from_path(local_path)?,
        remote_uri: remote_uri.to_string(),
        mime_type: guess_mime_type(local_path),
        last_modified,
        overwrite,
        previous_version: String::new(),
        policy_id: String::new(),
        metadata: None,
    };

    let stream = match uploader.upload(params).await {
        Ok(stream) => stream,
        Err(error) if is_object_existed(&error) => return Ok(UploadOutcome::Skipped),
        Err(error) => return Err(error),
    };

    // Progress is a fold over the events: nothing is uploaded until they are polled.
    let mut tracker = Tracker::new(&stream.session, &[]);
    pb.set_length(tracker.current().total_size);
    pb.set_message(label.to_string());

    let mut events = stream.events;
    while let Some(event) = events.next().await {
        match event {
            UploadEvent::Completed => {
                pb.set_position(tracker.current().uploaded);
                return Ok(UploadOutcome::Uploaded);
            }
            UploadEvent::Failed(error) => return Err(error.into()),
            event @ UploadEvent::ChunkDone(_) => {
                if let Some(update) = tracker.observe(&event) {
                    pb.set_position(update.uploaded);
                    pb.set_message(format!(
                        "{} | {}/s | ETA {} | chunks {}/{}",
                        label,
                        HumanBytes(update.speed_bytes_per_sec),
                        match update.eta_seconds {
                            Some(seconds) => HumanDuration(Duration::from_secs(seconds)).to_string(),
                            None => "--".to_string(),
                        },
                        update.completed_chunks,
                        update.total_chunks,
                    ));
                }
            }
        }
    }

    bail!("upload stream ended without a terminal event")
}

/// The upload session is rejected when the remote file already exists and the
/// caller did not ask for a new version.
fn is_object_existed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ApiError>(),
            Some(ApiError::ApiError { code, .. }) if *code == ErrorCode::ObjectExisted as i32
        )
    })
}

fn guess_mime_type(path: &PathBuf) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "png" => Some("image/png".to_string()),
        "gif" => Some("image/gif".to_string()),
        "pdf" => Some("application/pdf".to_string()),
        "txt" => Some("text/plain".to_string()),
        "json" => Some("application/json".to_string()),
        "zip" => Some("application/zip".to_string()),
        "mp4" => Some("video/mp4".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(code: i32) -> anyhow::Error {
        anyhow::Error::new(ApiError::ApiError {
            code,
            message: "remote file exists".to_string(),
            error_detail: None,
            correlation_id: None,
            aggregated_errors: None,
        })
        .context("failed to create upload session")
    }

    #[test]
    fn object_existed_is_detected_through_context() {
        assert!(is_object_existed(&api_error(ErrorCode::ObjectExisted as i32)));
    }

    #[test]
    fn other_api_errors_are_reported_as_failures() {
        assert!(!is_object_existed(&api_error(
            ErrorCode::ParentNotExist as i32
        )));
    }
}
