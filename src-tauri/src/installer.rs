use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::available_space;
use tauri::{AppHandle, ipc::Channel};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::error::{AppError, AppResult};
use crate::github::GitHubClient;
use crate::models::{
    AuthMethod, DEPOTS, FRESH_INSTALL_BYTES, GAME_EXECUTABLE, OperationEvent, OperationKind,
    OperationRequest, OperationResult, REPAIR_BYTES, ReleaseInfo, STEAM_APP_ID, UPDATE_BYTES,
};
use crate::storage;

pub async fn run(
    app: &AppHandle,
    request: OperationRequest,
    on_event: Channel<OperationEvent>,
    cancel: CancellationToken,
    terminal_input: Arc<AsyncMutex<Option<ChildStdin>>>,
) -> AppResult<OperationResult> {
    validate_username(request.kind, &request.steam_username)?;
    let existing_game = Path::new(request.install_directory.trim())
        .join(GAME_EXECUTABLE)
        .is_file();
    let required = match request.kind {
        OperationKind::Install if existing_game => REPAIR_BYTES,
        OperationKind::Install => FRESH_INSTALL_BYTES,
        OperationKind::Repair => REPAIR_BYTES,
        OperationKind::Update => UPDATE_BYTES,
    };
    progress(
        &on_event,
        "preflight",
        if existing_game {
            "Existing Destiny 2 installation found; preparing file verification…"
        } else {
            "Checking the installation folder…"
        },
        1,
    );
    let root = prepare_install_root(&request.install_directory, required)?;
    ensure_game_closed()?;

    if !matches!(request.kind, OperationKind::Install) {
        verify_game_files(&root)?;
    }

    let github = GitHubClient::new()?;
    if matches!(request.kind, OperationKind::Install | OperationKind::Repair) {
        progress(&on_event, "tool", "Preparing DepotDownloader…", 3);
        let downloader = ensure_depot_downloader(app, &github, &on_event, &cancel).await?;
        cancel_check(&cancel)?;
        download_depots(
            &downloader,
            &root,
            request.steam_username.trim(),
            request.auth_method,
            matches!(request.kind, OperationKind::Repair),
            &on_event,
            &cancel,
            terminal_input,
        )
        .await?;
        verify_game_files(&root)?;
        if matches!(request.kind, OperationKind::Repair) {
            progress(
                &on_event,
                "repair",
                "Clearing Sunrise configuration and cached data…",
                88,
            );
            delete_sunrise_data(&root)?;
        }
    }

    cancel_check(&cancel)?;
    progress(
        &on_event,
        "release",
        "Checking the latest Sunrise release…",
        89,
    );
    let release = github
        .latest_release("stanuwu", "Sunrise", "steam_api64.dll")
        .await?;

    if matches!(request.kind, OperationKind::Update)
        && installation_is_current(&root, &release).await?
    {
        progress(
            &on_event,
            "complete",
            &format!("Sunrise {} is already current.", release.tag),
            100,
        );
        return Ok(OperationResult {
            changed: false,
            release_tag: release.tag.clone(),
            message: format!("Sunrise {} is already current.", release.tag),
        });
    }

    let cache_root = storage::app_cache_dir(app)?;
    tokio::fs::create_dir_all(&cache_root)
        .await
        .map_err(|error| AppError::io("Could not create the download cache", error))?;
    let staging = tempfile::Builder::new()
        .prefix("sunrise-payload-")
        .tempdir_in(&cache_root)
        .map_err(|error| AppError::io("Could not create a temporary download folder", error))?;
    let payload = staging.path().join("steam_api64.dll");
    let payload_hash = github
        .download(&release, &payload, &on_event, "release", 90, 96)
        .await?;
    cancel_check(&cancel)?;

    progress(&on_event, "install", "Installing Sunrise…", 98);
    install_payload(
        &root,
        &payload,
        &payload_hash,
        matches!(request.kind, OperationKind::Install | OperationKind::Repair),
    )
    .await?;
    let state = storage::new_state(
        release.tag.clone(),
        release.asset_name.clone(),
        release.digest.clone(),
        payload_hash,
    );
    storage::save_state(&root, &state).await?;

    let verb = match request.kind {
        OperationKind::Install => "Installed",
        OperationKind::Repair => "Repaired",
        OperationKind::Update => "Updated",
    };
    let message = format!("{verb} Sunrise {} successfully.", release.tag);
    progress(&on_event, "complete", &message, 100);
    Ok(OperationResult {
        changed: true,
        release_tag: release.tag,
        message,
    })
}

fn progress(on_event: &Channel<OperationEvent>, stage: &str, message: &str, percent: u8) {
    let _ = on_event.send(OperationEvent::Progress {
        stage: stage.into(),
        message: message.into(),
        percent: f32::from(percent),
    });
}

fn cancel_check(cancel: &CancellationToken) -> AppResult<()> {
    if cancel.is_cancelled() {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_username(kind: OperationKind, username: &str) -> AppResult<()> {
    if matches!(kind, OperationKind::Update) {
        return Ok(());
    }
    if username.trim().is_empty() {
        return Err(AppError::message(
            "Enter the Steam account name that owns Destiny 2.",
        ));
    }
    if username.chars().any(char::is_control) {
        return Err(AppError::message(
            "The Steam account name contains an invalid character.",
        ));
    }
    Ok(())
}

fn prepare_install_root(raw: &str, required_bytes: u64) -> AppResult<PathBuf> {
    if raw.trim().is_empty() {
        return Err(AppError::message("Select an installation folder."));
    }
    let requested = PathBuf::from(raw.trim());
    if !requested.is_absolute() {
        return Err(AppError::message(
            "Choose an absolute installation folder path.",
        ));
    }
    std::fs::create_dir_all(&requested)
        .map_err(|error| AppError::io("Could not create the installation folder", error))?;
    let root = requested
        .canonicalize()
        .map_err(|error| AppError::io("Could not resolve the installation folder", error))?;
    if root.parent().is_none() {
        return Err(AppError::message(
            "Choose a folder inside a drive, not the drive itself.",
        ));
    }

    tempfile::Builder::new()
        .prefix(".sunrise-write-")
        .tempfile_in(&root)
        .map_err(|error| {
            AppError::io(
                "The launcher cannot write to this installation folder",
                error,
            )
        })?;

    let free = available_space(&root)
        .map_err(|error| AppError::io("Could not check available disk space", error))?;
    if free < required_bytes {
        return Err(AppError::message(format!(
            "Not enough free space. {} is required, but only {} is available.",
            format_bytes(required_bytes),
            format_bytes(free)
        )));
    }

    Ok(root)
}

fn format_bytes(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024_f64 * 1024_f64 * 1024_f64))
}

fn ensure_game_closed() -> AppResult<()> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq destiny2.exe", "/FO", "CSV", "/NH"])
            .output();
        if output.is_ok_and(|value| {
            String::from_utf8_lossy(&value.stdout)
                .to_ascii_lowercase()
                .contains("destiny2.exe")
        }) {
            return Err(AppError::message(
                "Close Destiny 2 before installing, repairing, or updating Sunrise.",
            ));
        }
    }
    Ok(())
}

fn verify_game_files(root: &Path) -> AppResult<()> {
    if !root.join(GAME_EXECUTABLE).is_file() {
        return Err(AppError::message(
            "DepotDownloader finished, but destiny2.exe is missing.",
        ));
    }
    if !root.join("bin").join("x64").is_dir() {
        return Err(AppError::message(
            "DepotDownloader finished, but the bin/x64 folder is missing.",
        ));
    }
    Ok(())
}

async fn installation_is_current(root: &Path, release: &ReleaseInfo) -> AppResult<bool> {
    let Some(state) = storage::load_state(root).await? else {
        return Ok(false);
    };
    if !state.release_tag.eq_ignore_ascii_case(&release.tag) {
        return Ok(false);
    }
    if let (Some(installed), Some(latest)) = (
        state.release_asset_digest.as_deref(),
        release.digest.as_deref(),
    ) && !installed.eq_ignore_ascii_case(latest)
    {
        return Ok(false);
    }
    let dll = storage::mod_path(root);
    if !dll.is_file() {
        return Ok(false);
    }
    Ok(storage::hash_file(&dll)
        .await?
        .eq_ignore_ascii_case(&state.installed_dll_sha256))
}

async fn ensure_depot_downloader(
    app: &AppHandle,
    github: &GitHubClient,
    on_event: &Channel<OperationEvent>,
    cancel: &CancellationToken,
) -> AppResult<PathBuf> {
    let asset_name = depot_asset_name()?;
    let release = github
        .latest_release("SteamRE", "DepotDownloader", asset_name)
        .await?;
    let versions = storage::app_data_dir(app)?
        .join("tools")
        .join("DepotDownloader")
        .join("versions");
    tokio::fs::create_dir_all(&versions)
        .await
        .map_err(|error| AppError::io("Could not create the tools folder", error))?;
    let version_directory = versions.join(sanitize_name(&release.tag));
    if let Some(executable) = find_depot_executable(&version_directory) {
        return Ok(executable);
    }

    cancel_check(cancel)?;
    let temporary = tempfile::Builder::new()
        .prefix(".depot-downloader-")
        .tempdir_in(&versions)
        .map_err(|error| AppError::io("Could not create a temporary tools folder", error))?;
    let archive = temporary.path().join("DepotDownloader.zip");
    github
        .download(&release, &archive, on_event, "tool", 3, 8)
        .await?;
    cancel_check(cancel)?;
    let extracted = temporary.path().join("extracted");
    let archive_copy = archive.clone();
    let extracted_copy = extracted.clone();
    tokio::task::spawn_blocking(move || extract_zip(&archive_copy, &extracted_copy))
        .await
        .map_err(|error| {
            AppError::message(format!("Archive extraction stopped unexpectedly: {error}"))
        })??;

    if version_directory.exists() {
        std::fs::remove_dir_all(&version_directory)
            .map_err(|error| AppError::io("Could not replace the cached tool", error))?;
    }
    std::fs::rename(&extracted, &version_directory)
        .map_err(|error| AppError::io("Could not cache DepotDownloader", error))?;
    let executable = find_depot_executable(&version_directory)
        .ok_or_else(|| AppError::message("The DepotDownloader archive has no executable."))?;
    make_executable(&executable)?;
    Ok(executable)
}

fn depot_asset_name() -> AppResult<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("DepotDownloader-windows-x64.zip"),
        ("windows", "aarch64") => Ok("DepotDownloader-windows-arm64.zip"),
        ("linux", "x86_64") => Ok("DepotDownloader-linux-x64.zip"),
        ("linux", "aarch64") => Ok("DepotDownloader-linux-arm64.zip"),
        ("macos", "x86_64") => Ok("DepotDownloader-macos-x64.zip"),
        ("macos", "aarch64") => Ok("DepotDownloader-macos-arm64.zip"),
        (os, arch) => Err(AppError::message(format!(
            "DepotDownloader does not publish an asset for {os}/{arch}."
        ))),
    }
}

fn find_depot_executable(root: &Path) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_type().is_file()
                && (entry
                    .file_name()
                    .eq_ignore_ascii_case(OsStr::new("DepotDownloader"))
                    || entry
                        .file_name()
                        .eq_ignore_ascii_case(OsStr::new("DepotDownloader.exe")))
        })
        .map(|entry| entry.into_path())
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn extract_zip(archive_path: &Path, destination: &Path) -> AppResult<()> {
    let file = std::fs::File::open(archive_path)
        .map_err(|error| AppError::io("Could not open the downloaded archive", error))?;
    let mut archive = zip::ZipArchive::new(file)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| AppError::message("The archive contains an unsafe path."))?;
        let output = destination.join(enclosed);
        if entry.is_dir() {
            std::fs::create_dir_all(&output)
                .map_err(|error| AppError::io("Could not create an extracted folder", error))?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::io("Could not create an extracted folder", error))?;
        }
        let mut file = std::fs::File::create(&output)
            .map_err(|error| AppError::io("Could not create an extracted file", error))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| AppError::io("Could not extract DepotDownloader", error))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| AppError::io("Could not inspect DepotDownloader", error))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| AppError::io("Could not make DepotDownloader executable", error))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> AppResult<()> {
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download_depots(
    executable: &Path,
    install_root: &Path,
    steam_username: &str,
    auth_method: AuthMethod,
    validate: bool,
    on_event: &Channel<OperationEvent>,
    cancel: &CancellationToken,
    terminal_input: Arc<AsyncMutex<Option<ChildStdin>>>,
) -> AppResult<()> {
    let auth_message = match auth_method {
        AuthMethod::Qr => {
            "Steam authentication is handled directly by DepotDownloader. Scan the QR code when it appears; your password is never stored by this launcher."
        }
        AuthMethod::TwoFactor => {
            "Steam authentication is handled directly by DepotDownloader. Enter your password and Steam Guard code when prompted; neither is stored by this launcher."
        }
    };
    let _ = on_event.send(OperationEvent::Notice {
        message: auth_message.into(),
    });
    for (index, (depot, manifest)) in DEPOTS.into_iter().enumerate() {
        cancel_check(cancel)?;
        let (start_percent, end_percent) = if index == 0 { (12, 49) } else { (50, 85) };
        progress(on_event, "depots", "Preparing game files…", start_percent);
        run_depot(
            executable,
            install_root,
            steam_username,
            depot,
            manifest,
            validate,
            auth_method,
            index == 0,
            start_percent,
            end_percent,
            on_event,
            cancel,
            terminal_input.clone(),
        )
        .await?;
    }
    progress(on_event, "depots", "Steam game files are ready.", 86);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_depot(
    executable: &Path,
    install_root: &Path,
    steam_username: &str,
    depot: u32,
    manifest: u64,
    validate: bool,
    auth_method: AuthMethod,
    first_depot: bool,
    start_percent: u8,
    end_percent: u8,
    on_event: &Channel<OperationEvent>,
    cancel: &CancellationToken,
    terminal_input: Arc<AsyncMutex<Option<ChildStdin>>>,
) -> AppResult<()> {
    let mut command = Command::new(executable);
    command
        .current_dir(executable.parent().unwrap_or_else(|| Path::new(".")))
        .args(["-app", &STEAM_APP_ID.to_string()])
        .args(["-depot", &depot.to_string()])
        .args(["-manifest", &manifest.to_string()])
        .arg("-dir")
        .arg(install_root)
        .args(authentication_args(
            steam_username,
            auth_method,
            first_depot,
        ))
        .args(["-os", "windows"])
        .args(["-osarch", "64"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if validate {
        command.arg("-validate");
    }
    #[cfg(windows)]
    {
        command.creation_flags(0x0800_0000);
    }

    let mut child = command
        .spawn()
        .map_err(|error| AppError::io("DepotDownloader could not be started", error))?;
    *terminal_input.lock().await = child.stdin.take();
    let stdout_task = child.stdout.take().map(|stdout| {
        stream_output(
            stdout,
            "stdout",
            on_event.clone(),
            Some(DepotProgress::new(start_percent, end_percent)),
        )
    });
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| stream_output(stderr, "stderr", on_event.clone(), None));

    let outcome = tokio::select! {
        status = child.wait() => status
            .map_err(|error| AppError::io("Could not wait for DepotDownloader", error)),
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(AppError::Cancelled)
        }
    };
    *terminal_input.lock().await = None;
    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    let status = outcome?;
    if !status.success() {
        return Err(AppError::message(format!(
            "DepotDownloader stopped with exit code {}. The captured output is available in Settings for diagnostics.",
            status
                .code()
                .map_or_else(|| "unknown".into(), |code| code.to_string())
        )));
    }
    Ok(())
}

fn authentication_args(username: &str, auth_method: AuthMethod, first_depot: bool) -> Vec<String> {
    if first_depot && auth_method == AuthMethod::Qr {
        vec!["-qr".into(), "-remember-password".into()]
    } else {
        let mut args = vec![
            "-username".into(),
            username.into(),
            "-remember-password".into(),
        ];
        if first_depot && auth_method == AuthMethod::TwoFactor {
            args.push("-no-mobile".into());
        }
        args
    }
}

#[derive(Default)]
struct TerminalDecoder {
    pending: Vec<u8>,
}

impl TerminalDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    output.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        output.push_str(
                            std::str::from_utf8(&self.pending[..valid])
                                .expect("the decoder reported a valid UTF-8 prefix"),
                        );
                        self.pending.drain(..valid);
                        continue;
                    }
                    let Some(invalid_length) = error.error_len() else {
                        break;
                    };
                    let invalid: Vec<u8> = self.pending.drain(..invalid_length).collect();
                    for byte in invalid {
                        output.push(decode_oem_character(byte));
                    }
                }
            }
        }
        output
    }

    fn finish(&mut self) -> String {
        self.pending.drain(..).map(decode_oem_character).collect()
    }
}

fn decode_oem_character(byte: u8) -> char {
    match byte {
        0xB0 => '░',
        0xB1 => '▒',
        0xB2 => '▓',
        0xDB => '█',
        0xDC => '▄',
        0xDF => '▀',
        0x00..=0x7F => char::from(byte),
        _ => '�',
    }
}

#[derive(Default)]
struct QrCapture {
    rows: Vec<String>,
    expected_rows: Option<usize>,
}

impl QrCapture {
    fn push_line(&mut self, line: &str) -> Option<Vec<String>> {
        let width = line.chars().count();
        if width == 0 || !line.chars().all(is_qr_character) {
            self.rows.clear();
            self.expected_rows = None;
            return None;
        }
        let expected = *self.expected_rows.get_or_insert(width.div_ceil(2));
        self.rows.push(line.into());
        if self.rows.len() < expected {
            return None;
        }
        self.expected_rows = None;
        Some(std::mem::take(&mut self.rows))
    }
}

fn is_qr_character(character: char) -> bool {
    matches!(character, ' ' | '░' | '▒' | '▓' | '█' | '▄' | '▀')
}

struct TerminalEventParser {
    line_buffer: String,
    qr_capture: Option<QrCapture>,
    auth_prompts: AuthPromptScanner,
    auth_completion: AuthCompletionScanner,
    depot_progress: Option<DepotProgress>,
}

impl TerminalEventParser {
    fn new(depot_progress: Option<DepotProgress>) -> Self {
        Self {
            line_buffer: String::new(),
            qr_capture: None,
            auth_prompts: AuthPromptScanner::default(),
            auth_completion: AuthCompletionScanner::default(),
            depot_progress,
        }
    }

    fn push(&mut self, text: &str, on_event: &Channel<OperationEvent>) {
        for (kind, message) in self.auth_prompts.push(text) {
            let _ = on_event.send(OperationEvent::AuthPrompt {
                kind: kind.into(),
                message: message.into(),
            });
        }
        if self.auth_completion.push(text) {
            let _ = on_event.send(OperationEvent::AuthComplete);
        }
        self.line_buffer.push_str(text);
        while let Some(newline) = self.line_buffer.find('\n') {
            let mut remainder = self.line_buffer.split_off(newline + 1);
            std::mem::swap(&mut remainder, &mut self.line_buffer);
            let line = remainder.trim_end_matches(['\r', '\n']);
            self.observe_line(line, on_event);
        }
    }

    fn finish(&mut self, on_event: &Channel<OperationEvent>) {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.observe_line(&line, on_event);
        }
    }

    fn observe_line(&mut self, line: &str, on_event: &Channel<OperationEvent>) {
        if let Some(progress) = self.depot_progress.as_mut()
            && let Some(update) = progress.observe_line(line)
        {
            let _ = on_event.send(OperationEvent::Progress {
                stage: "depots".into(),
                message: update.message,
                percent: update.percent,
            });
        }
        if line.contains("Use the Steam Mobile App to sign in with this QR code:") {
            self.qr_capture = Some(QrCapture::default());
            return;
        }
        if let Some(capture) = self.qr_capture.as_mut() {
            if let Some(rows) = capture.push_line(line) {
                let _ = on_event.send(OperationEvent::QrCode { rows });
                self.qr_capture = None;
            } else if capture.expected_rows.is_none() && capture.rows.is_empty() {
                self.qr_capture = None;
            }
        }
    }
}

struct DepotProgress {
    start_percent: u8,
    end_percent: u8,
    depot_percent: f32,
}

impl DepotProgress {
    fn new(start_percent: u8, end_percent: u8) -> Self {
        Self {
            start_percent,
            end_percent,
            depot_percent: 0.0,
        }
    }

    fn observe_line(&mut self, line: &str) -> Option<DepotProgressUpdate> {
        if let Some(file_name) = parse_validating_file(line) {
            return Some(DepotProgressUpdate {
                percent: self.overall_percent(),
                message: format!("Validating {file_name}"),
            });
        }

        let (depot_percent, file_name) = parse_depot_file(line)?;
        self.depot_percent = self.depot_percent.max(depot_percent);
        Some(DepotProgressUpdate {
            percent: self.overall_percent(),
            message: format!("Downloading {file_name}"),
        })
    }

    fn overall_percent(&self) -> f32 {
        let span = self.end_percent.saturating_sub(self.start_percent) as f32;
        (f32::from(self.start_percent) + span * self.depot_percent / 100.0)
            .min(f32::from(self.end_percent))
    }
}

struct DepotProgressUpdate {
    percent: f32,
    message: String,
}

fn parse_validating_file(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix("Validating ")
        .and_then(terminal_file_name)
}

fn parse_depot_file(line: &str) -> Option<(f32, &str)> {
    let trimmed = line.trim_start();
    let (number, remainder) = trimmed.split_once('%')?;
    if number.is_empty()
        || number
            .chars()
            .any(|character| !character.is_ascii_digit() && character != '.')
        || !remainder.starts_with(char::is_whitespace)
    {
        return None;
    }
    let percent = number.parse::<f32>().ok()?;
    let file_name = terminal_file_name(remainder)?;
    percent
        .is_finite()
        .then_some((percent.clamp(0.0, 100.0), file_name))
}

fn terminal_file_name(path: &str) -> Option<&str> {
    path.trim()
        .rsplit(['\\', '/'])
        .find(|component| !component.is_empty())
}

const AUTH_PROMPT_DEFINITIONS: [(&str, &str, &str); 4] = [
    (
        "enter account password",
        "password",
        "Enter your Steam password",
    ),
    (
        "enter your 2-factor auth code",
        "twoFactor",
        "Enter the Steam Guard code from your authenticator",
    ),
    (
        "enter your 2 factor auth code",
        "twoFactor",
        "Enter the Steam Guard code from your authenticator",
    ),
    (
        "authentication code sent to your email",
        "emailCode",
        "Enter the Steam Guard code sent to your email",
    ),
];

#[derive(Default)]
struct AuthPromptScanner {
    buffer: String,
}

impl AuthPromptScanner {
    fn push(&mut self, text: &str) -> Vec<(&'static str, &'static str)> {
        self.buffer.push_str(text);
        let lower = self.buffer.to_ascii_lowercase();
        let mut scan_from = 0;
        let mut prompts = Vec::new();

        loop {
            let next = AUTH_PROMPT_DEFINITIONS
                .iter()
                .filter_map(|definition| {
                    lower[scan_from..]
                        .find(definition.0)
                        .map(|relative| (scan_from + relative, definition))
                })
                .min_by_key(|(start, _)| *start);
            let Some((start, definition)) = next else {
                break;
            };
            prompts.push((definition.1, definition.2));
            scan_from = start + definition.0.len();
        }

        if scan_from > 0 {
            self.buffer.drain(..scan_from);
        }
        self.retain_partial_prompt();
        prompts
    }

    fn retain_partial_prompt(&mut self) {
        let tail_length = AUTH_PROMPT_DEFINITIONS
            .iter()
            .map(|definition| definition.0.len())
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        if self.buffer.len() <= tail_length {
            return;
        }
        let mut drain_to = self.buffer.len() - tail_length;
        while !self.buffer.is_char_boundary(drain_to) {
            drain_to += 1;
        }
        self.buffer.drain(..drain_to);
    }
}

const AUTH_COMPLETE_MARKERS: [&str; 9] = [
    "using steam3 suggested cellid",
    "got session token",
    "licenses for account",
    "got appinfo for",
    "using app branch:",
    "got depot key for",
    "processing depot ",
    "downloading depot ",
    "got manifest request code",
];

#[derive(Default)]
struct AuthCompletionScanner {
    buffer: String,
    emitted: bool,
}

impl AuthCompletionScanner {
    fn push(&mut self, text: &str) -> bool {
        if self.emitted {
            return false;
        }
        self.buffer.push_str(text);
        let lower = self.buffer.to_ascii_lowercase();
        if AUTH_COMPLETE_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
        {
            self.emitted = true;
            self.buffer.clear();
            return true;
        }

        let tail_length = AUTH_COMPLETE_MARKERS
            .iter()
            .map(|marker| marker.len())
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        if self.buffer.len() > tail_length {
            let mut drain_to = self.buffer.len() - tail_length;
            while !self.buffer.is_char_boundary(drain_to) {
                drain_to += 1;
            }
            self.buffer.drain(..drain_to);
        }
        false
    }
}

fn stream_output<R>(
    mut reader: R,
    stream: &'static str,
    on_event: Channel<OperationEvent>,
    depot_progress: Option<DepotProgress>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 4096];
        let mut decoder = TerminalDecoder::default();
        let mut parser = TerminalEventParser::new(depot_progress);
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let text = decoder.push(&buffer[..count]);
                    if text.is_empty() {
                        continue;
                    }
                    parser.push(&text, &on_event);
                    let _ = on_event.send(OperationEvent::Terminal {
                        stream: stream.into(),
                        text,
                    });
                }
            }
        }
        let trailing = decoder.finish();
        if !trailing.is_empty() {
            parser.push(&trailing, &on_event);
            let _ = on_event.send(OperationEvent::Terminal {
                stream: stream.into(),
                text: trailing,
            });
        }
        parser.finish(&on_event);
    })
}

fn delete_sunrise_data(root: &Path) -> AppResult<()> {
    let x64 = root.join("bin").join("x64");
    let sunrise = x64.join("Sunrise");
    if !sunrise.starts_with(&x64) {
        return Err(AppError::message("The Sunrise data folder path is unsafe."));
    }
    if sunrise.exists() {
        std::fs::remove_dir_all(sunrise)
            .map_err(|error| AppError::io("Could not clear Sunrise data", error))?;
    }
    Ok(())
}

async fn install_payload(
    root: &Path,
    payload: &Path,
    expected_hash: &str,
    preserve_depot_dll: bool,
) -> AppResult<()> {
    let target = storage::mod_path(root);
    let target_directory = target
        .parent()
        .ok_or_else(|| AppError::message("The game DLL path is invalid."))?;
    if !target_directory.is_dir() {
        return Err(AppError::message(
            "The game bin/x64 folder is missing. Run Install or Repair first.",
        ));
    }

    let metadata_root = root.join(".sunrise");
    let rollback = metadata_root.join("rollback").join("steam_api64.dll");
    let original = metadata_root.join("original").join("steam_api64.dll");
    std::fs::create_dir_all(rollback.parent().expect("rollback has a parent"))
        .map_err(|error| AppError::io("Could not create the rollback folder", error))?;
    if target.is_file() {
        std::fs::copy(&target, &rollback)
            .map_err(|error| AppError::io("Could not create a rollback copy", error))?;
        if preserve_depot_dll {
            std::fs::create_dir_all(original.parent().expect("original has a parent"))
                .map_err(|error| AppError::io("Could not create the original DLL folder", error))?;
            std::fs::copy(&target, &original).map_err(|error| {
                AppError::io("Could not preserve the original Steam DLL", error)
            })?;
        }
    }

    let temporary = target_directory.join(format!(".steam_api64-{}.tmp", std::process::id()));
    tokio::fs::copy(payload, &temporary)
        .await
        .map_err(|error| AppError::io("Could not stage the Sunrise DLL", error))?;
    if target.exists() {
        tokio::fs::remove_file(&target).await.map_err(|error| {
            AppError::io("Could not replace steam_api64.dll; close the game", error)
        })?;
    }
    if let Err(error) = tokio::fs::rename(&temporary, &target).await {
        if rollback.is_file() {
            let _ = tokio::fs::copy(&rollback, &target).await;
        }
        return Err(AppError::io(
            "Could not finish installing the Sunrise DLL",
            error,
        ));
    }

    let actual = storage::hash_file(&target).await?;
    if !actual.eq_ignore_ascii_case(expected_hash) {
        if rollback.is_file() {
            let _ = tokio::fs::copy(&rollback, &target).await;
        }
        return Err(AppError::message(
            "The installed Sunrise DLL failed its SHA-256 check; the previous DLL was restored.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AuthCompletionScanner, AuthPromptScanner, DepotProgress, QrCapture, TerminalDecoder,
        authentication_args, depot_asset_name, parse_depot_file, parse_validating_file,
        sanitize_name,
    };
    use crate::models::AuthMethod;

    #[test]
    fn qr_and_username_authentication_are_never_combined() {
        let qr = authentication_args("sunrise_user", AuthMethod::Qr, true);
        assert!(qr.iter().any(|argument| argument == "-qr"));
        assert!(!qr.iter().any(|argument| argument == "-username"));

        let cached = authentication_args("sunrise_user", AuthMethod::Qr, false);
        assert!(cached.iter().any(|argument| argument == "-username"));
        assert!(!cached.iter().any(|argument| argument == "-qr"));
    }

    #[test]
    fn two_factor_authentication_requests_a_code_without_qr() {
        let two_factor = authentication_args("sunrise_user", AuthMethod::TwoFactor, true);
        assert!(two_factor.iter().any(|argument| argument == "-username"));
        assert!(two_factor.iter().any(|argument| argument == "-no-mobile"));
        assert!(!two_factor.iter().any(|argument| argument == "-qr"));
    }

    #[test]
    fn terminal_decoder_preserves_windows_qr_blocks() {
        let mut decoder = TerminalDecoder::default();
        assert_eq!(decoder.push(&[b' ', 0xDB, 0xDB, b'\n']), " ██\n");
    }

    #[test]
    fn terminal_decoder_reassembles_split_utf8_characters() {
        let mut decoder = TerminalDecoder::default();
        assert_eq!(decoder.push(&[0xE2, 0x96]), "");
        assert_eq!(decoder.push(&[0x88]), "█");
    }

    #[test]
    fn qr_capture_emits_a_complete_square_matrix() {
        let mut capture = QrCapture::default();
        assert!(capture.push_line("      ").is_none());
        assert!(capture.push_line("  ██  ").is_none());
        assert_eq!(
            capture.push_line("      "),
            Some(vec!["      ".into(), "  ██  ".into(), "      ".into()])
        );
    }

    #[test]
    fn auth_scanner_emits_password_then_hyphenated_steam_guard_prompt() {
        let mut scanner = AuthPromptScanner::default();
        assert!(scanner.push("Enter account pass").is_empty());
        assert_eq!(
            scanner
                .push("word for \"sunrise\": ")
                .into_iter()
                .map(|prompt| prompt.0)
                .collect::<Vec<_>>(),
            vec!["password"]
        );
        assert_eq!(
            scanner
                .push(
                    "Connecting to Steam3... Done! STEAM GUARD! Please enter your 2-factor auth code from your authenticator app: ",
                )
                .into_iter()
                .map(|prompt| prompt.0)
                .collect::<Vec<_>>(),
            vec!["twoFactor"]
        );
    }

    #[test]
    fn auth_scanner_emits_repeated_code_prompts_for_retries() {
        let mut scanner = AuthPromptScanner::default();
        let first =
            scanner.push("Please enter your 2 factor auth code from your authenticator app: ");
        let retry = scanner.push(
            "Invalid code. Please enter your 2-factor auth code from your authenticator app: ",
        );
        assert_eq!(first[0].0, "twoFactor");
        assert_eq!(retry[0].0, "twoFactor");
    }

    #[test]
    fn auth_completion_scanner_detects_cached_login_across_chunks_once() {
        let mut scanner = AuthCompletionScanner::default();
        assert!(!scanner.push("Logging 'sunrise' into Steam3... Done! Using Steam3 sug"));
        assert!(scanner.push("gested CellID: 207"));
        assert!(!scanner.push("Got session token!"));
    }

    #[test]
    fn auth_completion_scanner_detects_post_steam_guard_output() {
        let mut scanner = AuthCompletionScanner::default();
        assert!(!scanner.push(
            "STEAM GUARD! Please enter your 2-factor auth code from your authenticator app: "
        ));
        assert!(!scanner.push("Done!\r\n"));
        assert!(scanner.push("Got 2329 licenses for account!\r\n"));
    }

    #[test]
    fn depot_percent_parser_reads_depotdownloader_file_lines() {
        assert_eq!(
            parse_depot_file(" 42.17% C:\\Games\\Project Sunrise\\destiny2.exe"),
            Some((42.17, "destiny2.exe"))
        );
        assert_eq!(parse_depot_file("Downloading depot 1085661"), None);
        assert_eq!(parse_depot_file("Retrying after 50% of a second"), None);
        assert_eq!(
            parse_validating_file(
                "Validating \\\\?\\C:\\Games\\Project Sunrise\\packages\\activity.pkg"
            ),
            Some("activity.pkg")
        );
    }

    #[test]
    fn depot_progress_maps_local_percent_and_never_moves_backwards() {
        let mut progress = DepotProgress::new(12, 49);
        let validating = progress
            .observe_line("Validating C:\\Games\\first.pkg")
            .expect("validation should update the current file");
        assert_eq!(validating.percent, 12.0);
        assert_eq!(validating.message, "Validating first.pkg");

        let early = progress
            .observe_line(" 00.85% C:\\Games\\early.pkg")
            .expect("download should update progress");
        assert!((early.percent - 12.3145).abs() < f32::EPSILON);
        assert_eq!(early.message, "Downloading early.pkg");

        let later = progress
            .observe_line(" 50.00% C:\\Games\\second.pkg")
            .expect("download should update progress");
        assert_eq!(later.percent, 30.5);

        let delayed = progress
            .observe_line(" 49.00% C:\\Games\\delayed.pkg")
            .expect("file name should still update");
        assert_eq!(delayed.percent, 30.5);

        let complete = progress
            .observe_line("100.00% C:\\Games\\destiny2.exe")
            .expect("completion should update progress");
        assert_eq!(complete.percent, 49.0);
    }

    #[test]
    fn release_tags_become_safe_folder_names() {
        assert_eq!(
            sanitize_name("DepotDownloader 3.4/0"),
            "DepotDownloader_3.4_0"
        );
    }

    #[test]
    fn current_platform_has_a_depot_asset_when_supported() {
        if matches!(std::env::consts::OS, "windows" | "linux" | "macos")
            && matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
        {
            assert!(depot_asset_name().is_ok());
        }
    }
}
