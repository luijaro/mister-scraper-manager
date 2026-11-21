use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use russh_sftp::client::{Config as SftpClientConfig, SftpSession};
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{SftpAuthMode, SftpConfig};
use crate::models::GameEntry;
use crate::scanner::{
    RawFileEntry, filter_directory_games, is_ignored_dir, is_image_extension, split_stem_ext,
};
use crate::systems::detect_game_system;

struct SshClientHandler {
    server_key_info: Arc<Mutex<Option<String>>>,
}

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        if let Ok(mut guard) = self.server_key_info.lock() {
            *guard = Some(format!("{server_public_key:?}"));
        }
        Ok(true)
    }
}

struct SftpConn {
    session: client::Handle<SshClientHandler>,
    sftp: SftpSession,
}

/// Active SSH + SFTP client session to a remote MiSTer FPGA device,
/// with SSH keepalives and automatic reconnection if a session drops.
pub struct SftpClient {
    cfg: SftpConfig,
    conn: AsyncMutex<SftpConn>,
    pub server_key_info: Option<String>,
}

impl SftpClient {
    /// Connects and authenticates via SSH and opens the SFTP subsystem.
    pub async fn connect(cfg: &SftpConfig) -> Result<Self> {
        let (conn, server_key_info) = Self::open_connection(cfg).await?;
        Ok(Self {
            cfg: cfg.clone(),
            conn: AsyncMutex::new(conn),
            server_key_info,
        })
    }

    async fn open_connection(cfg: &SftpConfig) -> Result<(SftpConn, Option<String>)> {
        let host = cfg.host.trim();
        if host.is_empty() {
            bail!("La dirección IP o host de la MiSTer está vacía.");
        }
        let username = cfg.username.trim();
        if username.is_empty() {
            bail!("El usuario SSH no puede estar vacío.");
        }

        let ssh_cfg = client::Config {
            nodelay: true,
            inactivity_timeout: None,
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 6,
            ..Default::default()
        };

        let key_slot = Arc::new(Mutex::new(None));
        let handler = SshClientHandler {
            server_key_info: Arc::clone(&key_slot),
        };

        let mut session = tokio::time::timeout(
            Duration::from_secs(12),
            client::connect(Arc::new(ssh_cfg), (host, cfg.port), handler),
        )
        .await
        .with_context(|| format!("Tiempo agotado conectando a {host}:{}", cfg.port))?
        .with_context(|| format!("No se pudo conectar por SSH a {host}:{}", cfg.port))?;

        match cfg.auth_mode {
            SftpAuthMode::Password => {
                let auth = session
                    .authenticate_password(username, &cfg.password)
                    .await
                    .context("Error al autenticar con contraseña SSH")?;
                if !auth.success() {
                    bail!("Autenticación SSH fallida (usuario o contraseña incorrectos).");
                }
            }
            SftpAuthMode::PrivateKey => {
                let key_path = cfg.private_key_path.trim();
                if key_path.is_empty() {
                    bail!("Selecciona un archivo de clave privada SSH.");
                }
                let passphrase = if cfg.passphrase.is_empty() {
                    None
                } else {
                    Some(cfg.passphrase.as_str())
                };
                let key_pair = load_secret_key(key_path, passphrase).with_context(|| {
                    format!("No se pudo cargar la clave privada SSH: {key_path}")
                })?;
                let hash_alg = session.best_supported_rsa_hash().await?.flatten();
                let auth = session
                    .authenticate_publickey(
                        username,
                        PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg),
                    )
                    .await
                    .context("Error al autenticar con clave pública SSH")?;
                if !auth.success() {
                    bail!("Autenticación SSH con clave privada rechazada por la MiSTer.");
                }
            }
        }

        let channel = session
            .channel_open_session()
            .await
            .context("No se pudo abrir canal SSH")?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context("No se pudo iniciar el subsistema SFTP en la MiSTer")?;

        let sftp_cfg = SftpClientConfig {
            request_timeout_secs: 60,
            ..Default::default()
        };
        let sftp = SftpSession::new_with_config(channel.into_stream(), sftp_cfg)
            .await
            .context("No se pudo inicializar la sesión SFTP")?;

        let server_key_info = key_slot.lock().ok().and_then(|g| g.clone());
        Ok((SftpConn { session, sftp }, server_key_info))
    }

    async fn ensure_alive(&self, conn: &mut SftpConn) -> Result<()> {
        if conn.session.is_closed() {
            let (fresh, _) = Self::open_connection(&self.cfg).await?;
            *conn = fresh;
        }
        Ok(())
    }

    async fn reconnect(&self, conn: &mut SftpConn) -> Result<()> {
        let _ = conn.sftp.close().await;
        let _ = conn
            .session
            .disconnect(russh::Disconnect::ByApplication, "", "es")
            .await;
        let (fresh, _) = Self::open_connection(&self.cfg).await?;
        *conn = fresh;
        Ok(())
    }

    /// Gracefully closes the SFTP and SSH session.
    pub async fn close(&self) {
        let conn = self.conn.lock().await;
        let _ = conn.sftp.close().await;
        let _ = conn
            .session
            .disconnect(russh::Disconnect::ByApplication, "", "es")
            .await;
    }
}

fn is_session_closed_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    // Normal remote filesystem responses do not represent a dropped session.
    if msg.contains("no such file")
        || msg.contains("permission denied")
        || msg.contains("file already exists")
        || msg.contains("is a directory")
    {
        return false;
    }
    [
        "closed",
        "disconnect",
        "broken pipe",
        "connection reset",
        "connection aborted",
        "unexpected eof",
        "eof",
        "timed out",
        "timeout",
        "channel",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// Unified storage backend supporting local/SMB paths and remote SFTP paths.
pub enum StorageClient {
    Local,
    Sftp(Box<SftpClient>),
}

impl StorageClient {
    /// Connects via SFTP/SSH and returns a boxed `StorageClient::Sftp`.
    pub async fn connect_sftp(cfg: &SftpConfig) -> Result<Self> {
        let client = SftpClient::connect(cfg).await?;
        Ok(Self::Sftp(Box::new(client)))
    }

    /// Returns the SSH server public key summary observed during handshake, if connected via SFTP.
    pub fn server_key_info(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Sftp(client) => client.server_key_info.as_deref(),
        }
    }

    /// Returns `true` if connected via SFTP.
    pub fn is_sftp(&self) -> bool {
        matches!(self, Self::Sftp(_))
    }

    /// Lists non-hidden subdirectory names inside `path` (sorted alphabetically).
    pub async fn list_subdirs(&self, path: &str) -> Result<Vec<String>> {
        match self {
            Self::Local => {
                let p = PathBuf::from(path);
                tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
                    let mut dirs = Vec::new();
                    for entry in fs::read_dir(&p).with_context(|| {
                        format!("No se pudo leer la carpeta local: {}", p.display())
                    })? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if !name.starts_with('.') {
                                dirs.push(name);
                            }
                        }
                    }
                    dirs.sort_by_key(|a| a.to_lowercase());
                    Ok(dirs)
                })
                .await?
            }
            Self::Sftp(client) => {
                let norm = posix_normalize(path);
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match sftp_list_subdirs(&conn.sftp, &norm).await {
                    Ok(dirs) => Ok(dirs),
                    Err(err) if is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        sftp_list_subdirs(&conn.sftp, &norm).await
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Recursively scans `root` (and its subfolders, ignoring `media/`) for valid games.
    pub async fn scan_games<F>(
        &self,
        root: &str,
        cancel: &AtomicBool,
        on_progress: F,
    ) -> Result<Vec<GameEntry>>
    where
        F: Fn(&str) + Send + Sync,
    {
        match self {
            Self::Local => scan_local_games(Path::new(root), cancel, &on_progress),
            Self::Sftp(client) => {
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match scan_sftp_games(&conn.sftp, root, cancel, &on_progress).await {
                    Ok(games) => Ok(games),
                    Err(err) if !cancel.load(Ordering::Relaxed) && is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        scan_sftp_games(&conn.sftp, root, cancel, &on_progress).await
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Lists existing cover images inside `media_dir` as `lowercase_stem -> actual_file_name`.
    /// Returns an empty map if `media_dir` does not exist yet.
    pub async fn list_existing_media(&self, media_dir: &str) -> Result<HashMap<String, String>> {
        match self {
            Self::Local => {
                let p = PathBuf::from(media_dir);
                tokio::task::spawn_blocking(move || -> Result<HashMap<String, String>> {
                    if !p.exists() {
                        return Ok(HashMap::new());
                    }
                    let mut existing = HashMap::new();
                    for entry in fs::read_dir(&p).with_context(|| {
                        format!("No se pudo leer carpeta media: {}", p.display())
                    })? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            continue;
                        }
                        let file_name = entry.file_name().to_string_lossy().to_string();
                        if file_name.starts_with('.') {
                            continue;
                        }
                        let (stem, ext) = split_stem_ext(&file_name);
                        if is_image_extension(&ext) {
                            existing.insert(stem.to_lowercase(), file_name);
                        }
                    }
                    Ok(existing)
                })
                .await?
            }
            Self::Sftp(client) => {
                let norm = posix_normalize(media_dir);
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match sftp_list_existing_media(&conn.sftp, &norm).await {
                    Ok(existing) => Ok(existing),
                    Err(err) if is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        sftp_list_existing_media(&conn.sftp, &norm).await
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Creates `dir` and all missing parent directories if needed.
    pub async fn ensure_dir(&self, dir: &str) -> Result<()> {
        match self {
            Self::Local => {
                let p = PathBuf::from(dir);
                tokio::task::spawn_blocking(move || {
                    fs::create_dir_all(&p).with_context(|| {
                        format!("No se pudo crear carpeta media: {}", p.display())
                    })
                })
                .await?
            }
            Self::Sftp(client) => {
                let norm = posix_normalize(dir);
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match sftp_ensure_dir(&conn.sftp, &norm).await {
                    Ok(()) => Ok(()),
                    Err(err) if is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        sftp_ensure_dir(&conn.sftp, &norm).await
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Returns `true` if `path` exists on the target storage.
    pub async fn path_exists(&self, path: &str) -> Result<bool> {
        match self {
            Self::Local => {
                let p = PathBuf::from(path);
                tokio::task::spawn_blocking(move || Ok(p.exists())).await?
            }
            Self::Sftp(client) => {
                let norm = posix_normalize(path);
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match conn
                    .sftp
                    .try_exists(&norm)
                    .await
                    .with_context(|| format!("Error comprobando ruta remota: {norm}"))
                {
                    Ok(exists) => Ok(exists),
                    Err(err) if is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        conn.sftp
                            .try_exists(&norm)
                            .await
                            .with_context(|| format!("Error comprobando ruta remota: {norm}"))
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Streams a local file to `dst` in 256 KiB chunks, reporting `(bytes_sent, total_bytes)`
    /// without buffering entire large disc images into memory.
    pub async fn copy_local_file_streamed<F>(
        &self,
        src: &Path,
        dst: &str,
        cancel: &AtomicBool,
        on_bytes: F,
    ) -> Result<u64>
    where
        F: Fn(u64, u64) + Send + Sync,
    {
        const CHUNK_SIZE: usize = 262_144;

        match self {
            Self::Local => {
                let src_buf = src.to_path_buf();
                let dst_buf = PathBuf::from(dst);
                let cancel_flag = cancel.load(Ordering::Relaxed);
                if cancel_flag {
                    bail!("Operación cancelada por el usuario.");
                }
                // Wrap callback in Arc so spawn_blocking can invoke it synchronously.
                // Note: since we await immediately, borrow lifetime is safe via scoped or Arc.
                // Use std::thread::scope? Better: channel or Arc? F is not 'static!
                // Wait: let's check! `F` is only `Fn(u64, u64) + Send + Sync` (not `'static`).
                // On Local, `fs` reads/writes in chunks can run inside `tokio::task::block_in_place`
                // OR simply read with `tokio::fs::File` and write with `tokio::fs::File`!
                // WAIT! `tokio::fs::File` works asynchronously on `self` without needing `'static`!
                let mut reader = tokio::fs::File::open(&src_buf)
                    .await
                    .with_context(|| format!("No se pudo abrir archivo origen: {}", src_buf.display()))?;
                let total_bytes = reader.metadata().await?.len();
                let mut writer = tokio::fs::File::create(&dst_buf)
                    .await
                    .with_context(|| format!("No se pudo crear archivo destino: {}", dst_buf.display()))?;

                let mut buf = vec![0u8; CHUNK_SIZE];
                let mut sent = 0u64;
                on_bytes(0, total_bytes);
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        drop(writer);
                        let _ = tokio::fs::remove_file(&dst_buf).await;
                        bail!("Operación cancelada por el usuario.");
                    }
                    let n = reader.read(&mut buf).await.with_context(|| {
                        format!("Error leyendo archivo origen: {}", src_buf.display())
                    })?;
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&buf[..n]).await.with_context(|| {
                        format!("Error escribiendo archivo destino: {}", dst_buf.display())
                    })?;
                    sent += n as u64;
                    on_bytes(sent, total_bytes);
                }
                writer.flush().await.with_context(|| {
                    format!("Error vaciando buffer en destino: {}", dst_buf.display())
                })?;
                Ok(sent)
            }
            Self::Sftp(client) => {
                let dst_norm = posix_normalize(dst);
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match sftp_copy_file_streamed(&conn.sftp, src, &dst_norm, cancel, &on_bytes).await {
                    Ok(sent) => Ok(sent),
                    Err(err) if !cancel.load(Ordering::Relaxed) && is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        sftp_copy_file_streamed(&conn.sftp, src, &dst_norm, cancel, &on_bytes).await
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Writes `bytes` to `path`, creating or truncating the destination file.
    pub async fn write_file(&self, path: &str, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Local => {
                let p = PathBuf::from(path);
                let data = bytes.to_vec();
                tokio::task::spawn_blocking(move || {
                    fs::write(&p, &data).with_context(|| {
                        format!("No se pudo escribir portada en: {}", p.display())
                    })
                })
                .await?
            }
            Self::Sftp(client) => {
                let norm = posix_normalize(path);
                let mut conn = client.conn.lock().await;
                client.ensure_alive(&mut conn).await?;
                match sftp_write_file(&conn.sftp, &norm, bytes).await {
                    Ok(()) => Ok(()),
                    Err(err) if is_session_closed_error(&err) => {
                        client.reconnect(&mut conn).await?;
                        sftp_write_file(&conn.sftp, &norm, bytes).await
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Closes underlying network connections cleanly (no-op for Local).
    pub async fn close(&self) {
        if let Self::Sftp(client) = self {
            client.close().await;
        }
    }
}

async fn sftp_list_subdirs(sftp: &SftpSession, norm: &str) -> Result<Vec<String>> {
    let entries = sftp
        .read_dir(norm)
        .await
        .with_context(|| format!("No se pudo leer carpeta remota: {norm}"))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        if name == "." || name == ".." || name.starts_with('.') {
            continue;
        }
        if entry.file_type().is_dir() {
            dirs.push(name);
        }
    }
    dirs.sort_by_key(|a| a.to_lowercase());
    Ok(dirs)
}

async fn sftp_list_existing_media(
    sftp: &SftpSession,
    norm: &str,
) -> Result<HashMap<String, String>> {
    let exists = sftp
        .try_exists(norm)
        .await
        .with_context(|| format!("Error al comprobar carpeta media remota: {norm}"))?;
    if !exists {
        return Ok(HashMap::new());
    }
    let entries = sftp
        .read_dir(norm)
        .await
        .with_context(|| format!("No se pudo leer carpeta media remota: {norm}"))?;
    let mut existing = HashMap::new();
    for entry in entries {
        let file_name = entry.file_name();
        if file_name == "." || file_name == ".." || file_name.starts_with('.') {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        let (stem, ext) = split_stem_ext(&file_name);
        if is_image_extension(&ext) {
            existing.insert(stem.to_lowercase(), file_name);
        }
    }
    Ok(existing)
}

async fn sftp_ensure_dir(sftp: &SftpSession, norm: &str) -> Result<()> {
    if sftp
        .try_exists(norm)
        .await
        .with_context(|| format!("Error comprobando carpeta remota: {norm}"))?
    {
        return Ok(());
    }
    let mut current = if norm.starts_with('/') {
        String::from("/")
    } else {
        String::new()
    };
    for part in norm.split('/').filter(|s| !s.is_empty()) {
        current = if current.is_empty() {
            part.to_string()
        } else if current == "/" {
            format!("/{part}")
        } else {
            format!("{current}/{part}")
        };
        if !sftp
            .try_exists(&current)
            .await
            .with_context(|| format!("Error comprobando carpeta remota: {current}"))?
            && let Err(err) = sftp.create_dir(&current).await
        {
            // Ignore race/already-exists if directory now exists
            let now_exists = sftp
                .try_exists(&current)
                .await
                .with_context(|| format!("Error verificando carpeta remota: {current}"))?;
            if !now_exists {
                return Err(err)
                    .with_context(|| format!("No se pudo crear carpeta remota: {current}"));
            }
        }
    }
    Ok(())
}

async fn sftp_copy_file_streamed<F>(
    sftp: &SftpSession,
    src: &Path,
    dst_norm: &str,
    cancel: &AtomicBool,
    on_bytes: &F,
) -> Result<u64>
where
    F: Fn(u64, u64) + Send + Sync,
{
    const CHUNK_SIZE: usize = 262_144;
    let mut reader = tokio::fs::File::open(src)
        .await
        .with_context(|| format!("No se pudo abrir archivo origen: {}", src.display()))?;
    let total_bytes = reader.metadata().await?.len();
    let mut writer = sftp
        .open_with_flags(
            dst_norm,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .with_context(|| format!("No se pudo abrir archivo remoto para escribir: {dst_norm}"))?;

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut sent = 0u64;
    on_bytes(0, total_bytes);
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = sftp.remove_file(dst_norm).await;
            bail!("Operación cancelada por el usuario.");
        }
        let n = reader
            .read(&mut buf)
            .await
            .with_context(|| format!("Error leyendo archivo origen: {}", src.display()))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .await
            .with_context(|| format!("Error subiendo datos a: {dst_norm}"))?;
        sent += n as u64;
        on_bytes(sent, total_bytes);
    }
    writer
        .flush()
        .await
        .with_context(|| format!("Error vaciando buffer en: {dst_norm}"))?;
    writer
        .shutdown()
        .await
        .with_context(|| format!("Error cerrando archivo remoto: {dst_norm}"))?;
    Ok(sent)
}

async fn sftp_write_file(sftp: &SftpSession, path: &str, bytes: &[u8]) -> Result<()> {
    let mut file = sftp
        .open_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .with_context(|| format!("No se pudo abrir archivo remoto para escribir: {path}"))?;
    file.write_all(bytes)
        .await
        .with_context(|| format!("Error subiendo datos a: {path}"))?;
    file.flush()
        .await
        .with_context(|| format!("Error vaciando buffer en: {path}"))?;
    file.shutdown()
        .await
        .with_context(|| format!("Error cerrando archivo remoto: {path}"))?;
    Ok(())
}

fn scan_local_games<F>(
    root: &Path,
    cancel: &AtomicBool,
    on_progress: &F,
) -> Result<Vec<GameEntry>>
where
    F: Fn(&str),
{
    if !root.exists() {
        bail!("La carpeta de juegos no existe: {}", root.display());
    }
    if !root.is_dir() {
        bail!(
            "La ruta de juegos no es una carpeta válida: {}",
            root.display()
        );
    }

    let root_str = root.to_string_lossy().to_string();
    let mut queue = VecDeque::new();
    queue.push_back((root.to_path_buf(), String::new()));

    let mut games = Vec::new();
    while let Some((dir_path, rel_dir)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Operación cancelada por el usuario.");
        }
        let progress_label = if rel_dir.is_empty() {
            root_str.clone()
        } else {
            rel_dir.clone()
        };
        on_progress(&progress_label);

        let mut raw_files = Vec::new();
        let mut subdirs = Vec::new();
        let read_dir = fs::read_dir(&dir_path)
            .with_context(|| format!("No se pudo leer carpeta de juegos: {}", dir_path.display()))?;

        for entry in read_dir {
            let entry = entry?;
            let file_type = entry.file_type()?;
            // Skip symlinks to prevent directory loops
            if file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                if !is_ignored_dir(&name) {
                    let child_rel = if rel_dir.is_empty() {
                        name.clone()
                    } else {
                        format!("{rel_dir}/{name}")
                    };
                    subdirs.push((entry.path(), child_rel));
                }
            } else if file_type.is_file() {
                raw_files.push(RawFileEntry {
                    full_path: entry.path().to_string_lossy().to_string(),
                    parent_dir: dir_path.to_string_lossy().to_string(),
                    relative_dir: rel_dir.clone(),
                    file_name: name,
                });
            }
        }

        subdirs.sort_by_key(|a| a.1.to_lowercase());
        for sub in subdirs {
            queue.push_back(sub);
        }

        for (raw, stem, ext) in filter_directory_games(raw_files) {
            let system = detect_game_system(&root_str, &raw.relative_dir, &stem, &ext);
            games.push(GameEntry {
                full_path: raw.full_path,
                parent_dir: raw.parent_dir,
                relative_dir: raw.relative_dir,
                file_name: raw.file_name,
                stem,
                extension: ext,
                system,
            });
        }
    }

    sort_games(&mut games);
    Ok(games)
}

async fn scan_sftp_games<F>(
    sftp: &SftpSession,
    root: &str,
    cancel: &AtomicBool,
    on_progress: &F,
) -> Result<Vec<GameEntry>>
where
    F: Fn(&str),
{
    let root_norm = posix_normalize(root);
    let meta = sftp
        .metadata(&root_norm)
        .await
        .with_context(|| format!("La carpeta remota no existe o no es accesible: {root_norm}"))?;
    if !meta.is_dir() {
        bail!("La ruta remota no es una carpeta: {root_norm}");
    }

    let mut queue = VecDeque::new();
    queue.push_back((root_norm.clone(), String::new()));

    let mut games = Vec::new();
    while let Some((dir_path, rel_dir)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Operación cancelada por el usuario.");
        }
        let progress_label = if rel_dir.is_empty() {
            root_norm.clone()
        } else {
            rel_dir.clone()
        };
        on_progress(&progress_label);

        let entries = sftp
            .read_dir(&dir_path)
            .await
            .with_context(|| format!("No se pudo leer carpeta remota: {dir_path}"))?;

        let mut raw_files = Vec::new();
        let mut subdirs = Vec::new();

        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let ft = entry.file_type();
            if ft.is_symlink() {
                continue;
            }
            let child_full = posix_join(&dir_path, &name);
            if ft.is_dir() {
                if !is_ignored_dir(&name) {
                    let child_rel = if rel_dir.is_empty() {
                        name
                    } else {
                        format!("{rel_dir}/{name}")
                    };
                    subdirs.push((child_full, child_rel));
                }
            } else {
                raw_files.push(RawFileEntry {
                    full_path: child_full,
                    parent_dir: dir_path.clone(),
                    relative_dir: rel_dir.clone(),
                    file_name: name,
                });
            }
        }

        subdirs.sort_by_key(|a| a.1.to_lowercase());
        for sub in subdirs {
            queue.push_back(sub);
        }

        for (raw, stem, ext) in filter_directory_games(raw_files) {
            let system = detect_game_system(&root_norm, &raw.relative_dir, &stem, &ext);
            games.push(GameEntry {
                full_path: raw.full_path,
                parent_dir: raw.parent_dir,
                relative_dir: raw.relative_dir,
                file_name: raw.file_name,
                stem,
                extension: ext,
                system,
            });
        }
    }

    sort_games(&mut games);
    Ok(games)
}

fn sort_games(games: &mut [GameEntry]) {
    games.sort_by(|a, b| {
        a.relative_dir
            .to_lowercase()
            .cmp(&b.relative_dir.to_lowercase())
            .then_with(|| a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()))
    });
}

/// Joins a base directory and child component using OS path rules (Local) or POSIX rules (SFTP).
pub fn join_storage_path(is_sftp: bool, base: &str, child: &str) -> String {
    if is_sftp {
        posix_join(base, child)
    } else {
        Path::new(base).join(child).to_string_lossy().to_string()
    }
}

/// Normalizes a POSIX path by resolving `.` and `..` segments and collapsing slashes.
pub fn posix_normalize(path: &str) -> String {
    let replaced = path.replace('\\', "/");
    let is_abs = replaced.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in replaced.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if !stack.is_empty() && *stack.last().unwrap() != ".." {
                    stack.pop();
                } else if !is_abs {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    if is_abs {
        if stack.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", stack.join("/"))
        }
    } else if stack.is_empty() {
        ".".to_string()
    } else {
        stack.join("/")
    }
}

/// Joins two POSIX path components with `/`.
pub fn posix_join(base: &str, child: &str) -> String {
    let base_norm = posix_normalize(base);
    let child_trimmed = child.trim_matches('/');
    if child_trimmed.is_empty() {
        return base_norm;
    }
    if base_norm == "/" {
        format!("/{child_trimmed}")
    } else if base_norm == "." {
        child_trimmed.to_string()
    } else {
        format!("{base_norm}/{child_trimmed}")
    }
}

/// Returns the parent directory of a POSIX path.
pub fn posix_parent(path: &str) -> String {
    let norm = posix_normalize(path);
    if norm == "/" || norm == "." {
        return norm;
    }
    match norm.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => ".".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_helpers_work_across_edge_cases() {
        assert_eq!(posix_normalize("/media/fat/games/PSX/"), "/media/fat/games/PSX");
        assert_eq!(posix_normalize("/media/fat/../usb0//games/"), "/media/usb0/games");
        assert_eq!(posix_normalize("/"), "/");
        assert_eq!(posix_join("/media/fat/games/PSX", "media"), "/media/fat/games/PSX/media");
        assert_eq!(posix_join("/", "media"), "/media");
        assert_eq!(posix_parent("/media/fat/games/PSX"), "/media/fat/games");
        assert_eq!(posix_parent("/media"), "/");
        assert_eq!(posix_parent("/"), "/");
    }

    #[tokio::test]
    async fn streamed_copy_transfers_multichunk_file_and_checks_existence() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.bin");
        let dst = dir.path().join("out.bin");
        let payload: Vec<u8> = (0..350_000usize).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &payload).unwrap();

        let storage = StorageClient::Local;
        let dst_str = dst.to_string_lossy().to_string();
        assert!(!storage.path_exists(&dst_str).await.unwrap());

        let cancel = AtomicBool::new(false);
        let progress_calls = Arc::new(Mutex::new(Vec::new()));
        let pc_ref = Arc::clone(&progress_calls);

        let copied = storage
            .copy_local_file_streamed(&src, &dst_str, &cancel, move |sent, total| {
                pc_ref.lock().unwrap().push((sent, total));
            })
            .await
            .unwrap();

        assert_eq!(copied, payload.len() as u64);
        assert!(storage.path_exists(&dst_str).await.unwrap());
        assert_eq!(fs::read(&dst).unwrap(), payload);

        let calls = progress_calls.lock().unwrap();
        assert!(calls.len() >= 3);
        assert_eq!(calls.last().copied(), Some((payload.len() as u64, payload.len() as u64)));
    }

    #[test]
    fn detects_dropped_sftp_session_errors_without_false_positives() {
        assert!(is_session_closed_error(&anyhow::anyhow!("Channel closed")));
        assert!(is_session_closed_error(&anyhow::anyhow!(
            "Error subiendo datos a /media/fat/games/PSX/game.chd: SSH session disconnected"
        )));
        assert!(is_session_closed_error(&anyhow::anyhow!("Request timed out")));
        assert!(is_session_closed_error(&anyhow::anyhow!("Unexpected EOF")));
        assert!(!is_session_closed_error(&anyhow::anyhow!(
            "No se pudo leer carpeta remota: No such file"
        )));
        assert!(!is_session_closed_error(&anyhow::anyhow!(
            "Error al abrir archivo: Permission denied"
        )));
    }
}
