//! Cloudflare Quick Tunnel transport for Rust-owned Remote Access.

use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use flate2::read::GzDecoder;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{timeout, Instant},
};
use url::Url;
use uuid::Uuid;

const CLOUDFLARED_VERSION: &str = "2026.8.3";
const QUICK_TUNNEL_SUFFIX: &str = ".trycloudflare.com";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum CloudflareTunnelError {
    #[error("cloudflared path is invalid: {0}")]
    InvalidExecutable(String),
    #[error("automatic cloudflared download is unsupported on {0}-{1}")]
    UnsupportedPlatform(&'static str, &'static str),
    #[error("cloudflared download failed: {0}")]
    Download(String),
    #[error("cloudflared archive is invalid: {0}")]
    Archive(String),
    #[error("cloudflared did not publish a Quick Tunnel URL within 30 seconds")]
    ReadyTimeout,
    #[error("cloudflared exited before publishing a Quick Tunnel URL: {0}")]
    EarlyExit(String),
    #[error("cloudflared output ended before publishing a Quick Tunnel URL")]
    MissingUrl,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudflareTunnelEndpoint {
    pub origin: String,
    pub authority: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloudflareTunnelEvent {
    Down,
    Running(CloudflareTunnelEndpoint),
}

pub struct CloudflareQuickTunnel {
    endpoint: CloudflareTunnelEndpoint,
    events: mpsc::UnboundedReceiver<CloudflareTunnelEvent>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl CloudflareQuickTunnel {
    pub async fn start(
        executable: PathBuf,
        local_origin: String,
    ) -> Result<Self, CloudflareTunnelError> {
        let running = RunningTunnel::spawn(&executable, &local_origin).await?;
        let endpoint = running.endpoint.clone();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (stop, stop_rx) = oneshot::channel();
        let task = tokio::spawn(supervise(
            executable,
            local_origin,
            running,
            events_tx,
            stop_rx,
        ));
        Ok(Self {
            endpoint,
            events,
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub fn endpoint(&self) -> &CloudflareTunnelEndpoint {
        &self.endpoint
    }

    pub async fn next_event(&mut self) -> Option<CloudflareTunnelEvent> {
        let event = self.events.recv().await?;
        if let CloudflareTunnelEvent::Running(endpoint) = &event {
            self.endpoint = endpoint.clone();
        }
        Some(event)
    }

    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for CloudflareQuickTunnel {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub async fn resolve_cloudflared(data_dir: &Path) -> Result<PathBuf, CloudflareTunnelError> {
    if let Some(configured) = env::var_os("TESSIVUM_CLOUDFLARED") {
        let path = PathBuf::from(configured);
        if !path.is_absolute() {
            return Err(CloudflareTunnelError::InvalidExecutable(
                "TESSIVUM_CLOUDFLARED must be an absolute path".into(),
            ));
        }
        validate_executable(&path).await?;
        return Ok(path);
    }
    if let Some(path) = path_executable() {
        return Ok(path);
    }
    download_cloudflared(data_dir).await
}

fn path_executable() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|root| !root.as_os_str().is_empty())
        .map(|root| root.join(name))
        .find(|path| executable_file(path))
}

async fn validate_executable(path: &Path) -> Result<(), CloudflareTunnelError> {
    let metadata = fs::metadata(path).await.map_err(|error| {
        CloudflareTunnelError::InvalidExecutable(format!("{}: {error}", path.display()))
    })?;
    if !metadata.is_file() || !executable_file(path) {
        return Err(CloudflareTunnelError::InvalidExecutable(format!(
            "{} is not an executable file",
            path.display()
        )));
    }
    Ok(())
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Clone, Copy)]
struct ReleaseAsset {
    name: &'static str,
    sha256: &'static str,
    binary_sha256: Option<&'static str>,
    archive: bool,
}

fn release_asset() -> Result<ReleaseAsset, CloudflareTunnelError> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok(ReleaseAsset {
            name: "cloudflared-linux-amd64",
            sha256: "f29324fe934d1e100617484c78deef803c4dc2cd351d645bbde42e96b4fccc5e",
            binary_sha256: Some("f29324fe934d1e100617484c78deef803c4dc2cd351d645bbde42e96b4fccc5e"),
            archive: false,
        }),
        ("linux", "aarch64") => Ok(ReleaseAsset {
            name: "cloudflared-linux-arm64",
            sha256: "4bcfd35521a7cbc545ebfd5d57334a71ee180e2a64874981f374c81472118391",
            binary_sha256: Some("4bcfd35521a7cbc545ebfd5d57334a71ee180e2a64874981f374c81472118391"),
            archive: false,
        }),
        ("macos", "x86_64") => Ok(ReleaseAsset {
            name: "cloudflared-darwin-amd64.tgz",
            sha256: "61e1316266a00fd70ce40da011d612badc805367fb65293dd1925f938f704c99",
            binary_sha256: Some("936aa4ed783b0e191fac48e7140c34605b25d8d5c0495c3599c90e350ae6e4c4"),
            archive: true,
        }),
        ("macos", "aarch64") => Ok(ReleaseAsset {
            name: "cloudflared-darwin-arm64.tgz",
            sha256: "40c9144d86df8937c5b43293a1f7d2d2107029aa74725023dd46b1b27154352f",
            binary_sha256: Some("50a04624531e7a98ddb65f1223905e32f84e7488ed3ee8dadcd3260aa8932603"),
            archive: true,
        }),
        _ => Err(CloudflareTunnelError::UnsupportedPlatform(
            env::consts::OS,
            env::consts::ARCH,
        )),
    }
}

async fn download_cloudflared(data_dir: &Path) -> Result<PathBuf, CloudflareTunnelError> {
    let asset = release_asset()?;
    let directory = data_dir.join("bin");
    fs::create_dir_all(&directory).await?;
    set_directory_permissions(&directory).await?;
    let destination = directory.join(format!("cloudflared-{CLOUDFLARED_VERSION}"));
    if executable_file(&destination) && cached_binary_matches(&destination, asset).await? {
        return Ok(destination);
    }
    eprintln!("Downloading verified cloudflared {CLOUDFLARED_VERSION}...");

    let download = directory.join(format!(".cloudflared-{}.download", Uuid::new_v4()));
    let extracted = directory.join(format!(".cloudflared-{}.binary", Uuid::new_v4()));
    let result = async {
        let url = format!(
            "https://github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_VERSION}/{}",
            asset.name
        );
        download_verified(&url, asset.sha256, &download).await?;
        if asset.archive {
            extract_archive(download.clone(), extracted.clone()).await?;
        } else {
            fs::rename(&download, &extracted).await?;
        }
        set_executable_permissions(&extracted).await?;
        if let Some(expected) = asset.binary_sha256 {
            let actual = sha256_file(&extracted).await?;
            if actual != expected {
                return Err(CloudflareTunnelError::Archive(format!(
                    "extracted binary checksum mismatch: expected {expected}, received {actual}"
                )));
            }
        }
        fs::rename(&extracted, &destination).await?;
        Ok(destination.clone())
    }
    .await;
    let _ = fs::remove_file(&download).await;
    let _ = fs::remove_file(&extracted).await;
    result
}

async fn cached_binary_matches(
    path: &Path,
    asset: ReleaseAsset,
) -> Result<bool, CloudflareTunnelError> {
    if let Some(expected) = asset.binary_sha256 {
        return Ok(sha256_file(path).await? == expected);
    }
    let output = Command::new(path).arg("--version").output().await?;
    Ok(output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains(CLOUDFLARED_VERSION))
}

async fn download_verified(
    url: &str,
    expected_sha256: &str,
    destination: &Path,
) -> Result<(), CloudflareTunnelError> {
    let response = reqwest::Client::builder()
        .user_agent(concat!("tessivum/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(5 * 60))
        .build()
        .map_err(|error| CloudflareTunnelError::Download(error.to_string()))?
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| CloudflareTunnelError::Download(error.to_string()))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES)
    {
        return Err(CloudflareTunnelError::Download(
            "release asset exceeds 64 MiB".into(),
        ));
    }
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .await?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| CloudflareTunnelError::Download(error.to_string()))?;
        size = size.saturating_add(chunk.len() as u64);
        if size > MAX_DOWNLOAD_BYTES {
            return Err(CloudflareTunnelError::Download(
                "release asset exceeds 64 MiB".into(),
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        return Err(CloudflareTunnelError::Download(format!(
            "checksum mismatch: expected {expected_sha256}, received {actual}"
        )));
    }
    Ok(())
}

async fn extract_archive(
    archive: PathBuf,
    destination: PathBuf,
) -> Result<(), CloudflareTunnelError> {
    tokio::task::spawn_blocking(move || {
        let archive = std::fs::File::open(archive)?;
        let decoder = GzDecoder::new(archive);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()?.file_name() != Some(OsStr::new("cloudflared"))
                || !entry.header().entry_type().is_file()
            {
                continue;
            }
            let mut output = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)?;
            io::copy(&mut entry, &mut output)?;
            output.flush()?;
            output.sync_all()?;
            return Ok::<_, io::Error>(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archive does not contain the cloudflared executable",
        ))
    })
    .await
    .map_err(|error| CloudflareTunnelError::Archive(error.to_string()))?
    .map_err(|error| CloudflareTunnelError::Archive(error.to_string()))
}

async fn sha256_file(path: &Path) -> Result<String, CloudflareTunnelError> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn set_directory_permissions(_path: &Path) -> Result<(), CloudflareTunnelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

async fn set_executable_permissions(_path: &Path) -> Result<(), CloudflareTunnelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

struct RunningTunnel {
    child: Child,
    endpoint: CloudflareTunnelEndpoint,
    started_at: Instant,
    readers: Vec<JoinHandle<()>>,
}

impl RunningTunnel {
    async fn spawn(executable: &Path, local_origin: &str) -> Result<Self, CloudflareTunnelError> {
        let mut command = Command::new(executable);
        command
            .arg("tunnel")
            .arg("--url")
            .arg(local_origin)
            .arg("--no-autoupdate")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CloudflareTunnelError::Io(io::Error::other("cloudflared stdout is unavailable"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            CloudflareTunnelError::Io(io::Error::other("cloudflared stderr is unavailable"))
        })?;
        let (lines_tx, mut lines_rx) = mpsc::unbounded_channel();
        let mut readers = vec![
            tokio::spawn(read_lines(stdout, lines_tx.clone())),
            tokio::spawn(read_lines(stderr, lines_tx)),
        ];
        let ready = timeout(READY_TIMEOUT, async {
            loop {
                tokio::select! {
                    status = child.wait() => {
                        return Err(CloudflareTunnelError::EarlyExit(status?.to_string()));
                    }
                    line = lines_rx.recv() => {
                        let Some(line) = line else {
                            return Err(CloudflareTunnelError::MissingUrl);
                        };
                        if let Some(endpoint) = quick_tunnel_endpoint(&line) {
                            return Ok(endpoint);
                        }
                    }
                }
            }
        })
        .await;
        let endpoint = match ready {
            Ok(Ok(endpoint)) => endpoint,
            Ok(Err(error)) => {
                terminate_child(&mut child).await;
                for reader in readers.drain(..) {
                    reader.abort();
                }
                return Err(error);
            }
            Err(_) => {
                terminate_child(&mut child).await;
                for reader in readers.drain(..) {
                    reader.abort();
                }
                return Err(CloudflareTunnelError::ReadyTimeout);
            }
        };
        readers.push(tokio::spawn(async move {
            while lines_rx.recv().await.is_some() {}
        }));
        Ok(Self {
            child,
            endpoint,
            started_at: Instant::now(),
            readers,
        })
    }

    async fn stop(&mut self) {
        terminate_child(&mut self.child).await;
        self.abort_readers();
    }

    fn abort_readers(&mut self) {
        for reader in self.readers.drain(..) {
            reader.abort();
        }
    }
}

async fn read_lines<R>(reader: R, output: mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if output.send(line).is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

async fn supervise(
    executable: PathBuf,
    local_origin: String,
    mut running: RunningTunnel,
    events: mpsc::UnboundedSender<CloudflareTunnelEvent>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut restart_delay = Duration::from_secs(1);
    loop {
        let started_at = running.started_at;
        let status = tokio::select! {
            _ = &mut stop => {
                running.stop().await;
                return;
            }
            status = running.child.wait() => status,
        };
        let uptime = started_at.elapsed();
        running.abort_readers();
        if events.send(CloudflareTunnelEvent::Down).is_err() {
            return;
        }
        match status {
            Ok(status) => eprintln!("Cloudflare Quick Tunnel exited ({status}); restarting"),
            Err(error) => eprintln!("Cloudflare Quick Tunnel wait failed ({error}); restarting"),
        }
        if uptime >= HEALTHY_UPTIME {
            restart_delay = Duration::from_secs(1);
        }
        loop {
            tokio::select! {
                _ = &mut stop => return,
                _ = tokio::time::sleep(restart_delay) => {}
            }
            let spawned = tokio::select! {
                _ = &mut stop => return,
                spawned = RunningTunnel::spawn(&executable, &local_origin) => spawned,
            };
            match spawned {
                Ok(next) => {
                    if events
                        .send(CloudflareTunnelEvent::Running(next.endpoint.clone()))
                        .is_err()
                    {
                        let mut next = next;
                        next.stop().await;
                        return;
                    }
                    running = next;
                    if uptime < HEALTHY_UPTIME {
                        restart_delay = (restart_delay * 2).min(MAX_RESTART_DELAY);
                    }
                    break;
                }
                Err(error) => {
                    eprintln!("Cloudflare Quick Tunnel restart failed: {error}");
                    restart_delay = (restart_delay * 2).min(MAX_RESTART_DELAY);
                }
            }
        }
    }
}

fn quick_tunnel_endpoint(line: &str) -> Option<CloudflareTunnelEndpoint> {
    for (start, _) in line.match_indices("https://") {
        let candidate = line[start..]
            .split(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '|' | '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\''
                    )
            })
            .next()?
            .trim_end_matches(['.', ',', ';']);
        let Ok(parsed) = Url::parse(candidate) else {
            continue;
        };
        let Some(authority) = parsed.host_str() else {
            continue;
        };
        let Some(label) = authority.strip_suffix(QUICK_TUNNEL_SUFFIX) else {
            continue;
        };
        if parsed.scheme() != "https"
            || parsed.port().is_some()
            || parsed.username() != ""
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || label.is_empty()
            || label.contains('.')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            continue;
        }
        return Some(CloudflareTunnelEndpoint {
            origin: format!("https://{authority}"),
            authority: authority.to_owned(),
        });
    }
    None
}

async fn terminate_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The child is placed in its own process group at spawn.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    if timeout(STOP_TIMEOUT, child.wait()).await.is_ok() {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_tunnel_url_requires_exact_cloudflare_https_origin() {
        let endpoint = quick_tunnel_endpoint(
            "INF | https://quiet-river-42.trycloudflare.com | quick tunnel ready",
        )
        .unwrap();
        assert_eq!(endpoint.origin, "https://quiet-river-42.trycloudflare.com");
        assert_eq!(endpoint.authority, "quiet-river-42.trycloudflare.com");
        assert!(quick_tunnel_endpoint("http://quiet-river.trycloudflare.com").is_none());
        assert!(quick_tunnel_endpoint("https://trycloudflare.com.evil.test").is_none());
        assert!(quick_tunnel_endpoint("https://nested.name.trycloudflare.com").is_none());
        assert!(quick_tunnel_endpoint("https://quiet-river.trycloudflare.com/path").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_removes_dead_authority_before_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let root = env::temp_dir().join(format!("tessivum-tunnel-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let count = root.join("count");
        let script = root.join("cloudflared");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nn=$(cat '{}' 2>/dev/null || echo 0)\nn=$((n + 1))\necho \"$n\" > '{}'\necho \"https://unit-$n.trycloudflare.com\" >&2\nif [ \"$n\" = 1 ]; then sleep 0.1; exit 1; fi\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
                count.display(),
                count.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut tunnel = CloudflareQuickTunnel::start(script, "http://127.0.0.1:3000".into())
            .await
            .unwrap();
        assert_eq!(tunnel.endpoint().authority, "unit-1.trycloudflare.com");
        assert_eq!(
            timeout(Duration::from_secs(5), tunnel.next_event())
                .await
                .unwrap(),
            Some(CloudflareTunnelEvent::Down),
        );
        assert_eq!(
            timeout(Duration::from_secs(5), tunnel.next_event())
                .await
                .unwrap(),
            Some(CloudflareTunnelEvent::Running(CloudflareTunnelEndpoint {
                origin: "https://unit-2.trycloudflare.com".into(),
                authority: "unit-2.trycloudflare.com".into(),
            })),
        );
        tunnel.shutdown().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
