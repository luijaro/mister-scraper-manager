use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

const CHUNK_SIZE: usize = 262_144;

/// Supported compressed game/bundle archive containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    SevenZ,
    Rar,
}

impl ArchiveFormat {
    /// Short uppercase badge label used in UI and CLI logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::SevenZ => "7Z",
            Self::Rar => "RAR",
        }
    }
}

/// Metadata for a single sanitized entry inside an archive.
#[derive(Debug, Clone)]
pub struct ArchiveEntryInfo {
    pub path: String,
    pub size_bytes: u64,
    pub is_dir: bool,
}

/// Summary of an archive header inspection without decompressing payloads.
#[derive(Debug, Clone)]
pub struct ArchiveInspection {
    pub format: ArchiveFormat,
    pub entries: Vec<ArchiveEntryInfo>,
    pub backend_used: String,
}

/// Optional external archive CLIs detected on the host machine as fallbacks.
#[derive(Debug, Clone, Default)]
pub struct ExternalArchiveTools {
    pub seven_zip: Option<PathBuf>,
    pub unrar: Option<PathBuf>,
    pub bsdtar: Option<PathBuf>,
}

/// Returns the archive format for `.zip`, `.7z`, or `.rar` paths.
pub fn detect_archive_format(path: &Path) -> Option<ArchiveFormat> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "zip" => Some(ArchiveFormat::Zip),
        "7z" => Some(ArchiveFormat::SevenZ),
        "rar" => Some(ArchiveFormat::Rar),
        _ => None,
    }
}

/// Returns `true` for non-first volumes of a multi-part RAR archive
/// (e.g., `game.part02.rar`, `game.part2.rar`, `game.r00`) so only the first
/// volume enters the queue while extraction still reads every volume.
pub fn is_secondary_rar_volume(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    if let Some((stem, ext)) = lower.rsplit_once('.') {
        if ext == "rar"
            && let Some((_, part_digits)) = stem.rsplit_once(".part")
            && !part_digits.is_empty()
            && part_digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(num) = part_digits.parse::<u32>()
        {
            return num > 1;
        }
        if ext.len() >= 2
            && ext.starts_with('r')
            && ext[1..].bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Normalizes an archive entry path to forward slashes and blocks path-traversal
/// sequences (`..`, drive prefixes, leading slashes, NUL bytes).
pub fn sanitize_archive_rel_path(raw: &str) -> Option<String> {
    if raw.contains('\0') {
        return None;
    }
    let replaced = raw.replace('\\', "/");
    let trimmed = replaced.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let mut clean_parts = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None;
        }
        if clean_parts.is_empty() && seg.contains(':') {
            return None;
        }
        clean_parts.push(seg);
    }

    if clean_parts.is_empty() {
        None
    } else {
        Some(clean_parts.join("/"))
    }
}

/// Detects optional external `7z`, `UnRAR`, and system `tar` (`bsdtar`) binaries.
pub fn detect_external_archive_tools() -> ExternalArchiveTools {
    let seven_zip = find_executable(
        &["7z", "7za", "7zz"],
        &[
            r"C:\Program Files\7-Zip\7z.exe",
            r"C:\Program Files (x86)\7-Zip\7z.exe",
        ],
    );
    let unrar = find_executable(
        &["unrar", "UnRAR"],
        &[
            r"C:\Program Files\WinRAR\UnRAR.exe",
            r"C:\Program Files (x86)\WinRAR\UnRAR.exe",
        ],
    );
    let bsdtar = find_executable(&["tar"], &[r"C:\Windows\System32\tar.exe"]);
    ExternalArchiveTools {
        seven_zip,
        unrar,
        bsdtar,
    }
}

fn find_executable(names: &[&str], common_paths: &[&str]) -> Option<PathBuf> {
    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
                #[cfg(windows)]
                {
                    let exe = dir.join(format!("{name}.exe"));
                    if exe.is_file() {
                        return Some(exe);
                    }
                }
            }
        }
    }

    for p in common_paths {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

/// Prevents spawning visible console windows on Windows GUI runs.
pub fn apply_no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Reads archive headers without extracting payloads to list contained files and sizes.
/// Uses built-in Rust decoders first and falls back to external `7z` / `unrar` / `tar` if needed.
pub fn inspect_archive(path: &Path) -> Result<ArchiveInspection> {
    let format = detect_archive_format(path)
        .with_context(|| format!("Formato de archivo comprimido no soportado: {}", path.display()))?;

    let native_res = match format {
        ArchiveFormat::Zip => inspect_zip_native(path).map(|entries| ("Nativo (ZIP)", entries)),
        ArchiveFormat::SevenZ => inspect_sevenz_native(path).map(|entries| ("Nativo (7Z)", entries)),
        ArchiveFormat::Rar => inspect_rar_native(path).map(|entries| ("Nativo (RAR)", entries)),
    };

    if let Ok((backend, entries)) = native_res {
        return Ok(ArchiveInspection {
            format,
            entries,
            backend_used: backend.to_string(),
        });
    }
    let native_err = native_res.unwrap_err();

    let tools = detect_external_archive_tools();
    if let Some(ref seven_z) = tools.seven_zip
        && let Ok(entries) = inspect_with_7z_cli(seven_z, path)
    {
        return Ok(ArchiveInspection {
            format,
            entries,
            backend_used: "7-Zip CLI".to_string(),
        });
    }

    if format == ArchiveFormat::Rar
        && let Some(ref unrar) = tools.unrar
        && let Ok(entries) = inspect_with_unrar_cli(unrar, path)
    {
        return Ok(ArchiveInspection {
            format,
            entries,
            backend_used: "UnRAR CLI".to_string(),
        });
    }

    if let Some(ref tar) = tools.bsdtar
        && let Ok(entries) = inspect_with_bsdtar(tar, path)
    {
        return Ok(ArchiveInspection {
            format,
            entries,
            backend_used: "bsdtar".to_string(),
        });
    }

    Err(native_err).with_context(|| {
        format!(
            "No se pudo inspeccionar el archivo {} (ni con el lector nativo ni con herramientas externas)",
            path.display()
        )
    })
}

fn inspect_zip_native(path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let file = File::open(path)
        .with_context(|| format!("No se pudo abrir ZIP: {}", path.display()))?;
    let archive = zip::ZipArchive::new(file)
        .with_context(|| format!("ZIP inválido o dañado: {}", path.display()))?;
    let meta = archive.metadata();
    let mut entries = Vec::with_capacity(meta.len());
    for idx in 0..meta.len() {
        let entry = meta.entry(idx)?;
        let raw_name = entry.name()?;
        let Some(clean_path) = sanitize_archive_rel_path(&raw_name) else {
            continue;
        };
        entries.push(ArchiveEntryInfo {
            path: clean_path,
            size_bytes: if entry.is_dir() { 0 } else { entry.size() },
            is_dir: entry.is_dir(),
        });
    }
    entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(entries)
}

fn inspect_sevenz_native(path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let archive = sevenz_rust2::Archive::open(path)
        .with_context(|| format!("No se pudo leer 7Z: {}", path.display()))?;
    let mut entries = Vec::with_capacity(archive.files.len());
    for entry in &archive.files {
        if entry.is_anti_item {
            continue;
        }
        let Some(clean_path) = sanitize_archive_rel_path(entry.name()) else {
            continue;
        };
        entries.push(ArchiveEntryInfo {
            path: clean_path,
            size_bytes: if entry.is_directory() {
                0
            } else {
                entry.size()
            },
            is_dir: entry.is_directory(),
        });
    }
    entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(entries)
}

fn inspect_rar_native(path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let archive = unrar_ng::Archive::new(path)
        .open_for_listing()
        .map_err(|e| anyhow::anyhow!("Error abriendo RAR {}: {e}", path.display()))?;
    let mut entries = Vec::new();
    for item in archive {
        let header = item.map_err(|e| anyhow::anyhow!("Error leyendo cabecera RAR: {e}"))?;
        let raw = header.filename.to_string_lossy();
        let Some(clean_path) = sanitize_archive_rel_path(&raw) else {
            continue;
        };
        entries.push(ArchiveEntryInfo {
            path: clean_path,
            size_bytes: if header.is_directory() {
                0
            } else {
                header.unpacked_size
            },
            is_dir: header.is_directory(),
        });
    }
    entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(entries)
}

fn inspect_with_7z_cli(seven_z: &Path, archive_path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let mut cmd = Command::new(seven_z);
    cmd.args(["l", "-slt", "-ba", "-p"])
        .arg(archive_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_no_window(&mut cmd);

    let out = cmd.output()?;
    if !out.status.success() {
        bail!("7z l falló con código {:?}", out.status.code());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut cur_size: u64 = 0;
    let mut cur_is_dir = false;

    let flush = |entries: &mut Vec<ArchiveEntryInfo>,
                 cur_path: &mut Option<String>,
                 cur_size: &mut u64,
                 cur_is_dir: &mut bool| {
        if let Some(raw) = cur_path.take()
            && let Some(clean) = sanitize_archive_rel_path(&raw)
        {
            entries.push(ArchiveEntryInfo {
                path: clean,
                size_bytes: if *cur_is_dir { 0 } else { *cur_size },
                is_dir: *cur_is_dir,
            });
        }
        *cur_size = 0;
        *cur_is_dir = false;
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut entries, &mut cur_path, &mut cur_size, &mut cur_is_dir);
            continue;
        }
        if let Some(v) = trimmed.strip_prefix("Path = ") {
            cur_path = Some(v.to_string());
        } else if let Some(v) = trimmed.strip_prefix("Size = ") {
            cur_size = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = trimmed.strip_prefix("Folder = ") {
            cur_is_dir = v.trim() == "+";
        } else if let Some(v) = trimmed.strip_prefix("Attributes = ")
            && v.starts_with('D')
        {
            cur_is_dir = true;
        }
    }
    flush(&mut entries, &mut cur_path, &mut cur_size, &mut cur_is_dir);
    entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(entries)
}

fn inspect_with_unrar_cli(unrar: &Path, archive_path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let mut cmd = Command::new(unrar);
    cmd.args(["lb", "-p-"])
        .arg(archive_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_no_window(&mut cmd);

    let out = cmd.output()?;
    if !out.status.success() {
        bail!("unrar lb falló con código {:?}", out.status.code());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_dir = trimmed.ends_with('/') || trimmed.ends_with('\\');
        if let Some(clean) = sanitize_archive_rel_path(trimmed) {
            entries.push(ArchiveEntryInfo {
                path: clean,
                size_bytes: 0,
                is_dir,
            });
        }
    }
    entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(entries)
}

fn inspect_with_bsdtar(tar: &Path, archive_path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let mut cmd = Command::new(tar);
    cmd.arg("-tf")
        .arg(archive_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_no_window(&mut cmd);

    let out = cmd.output()?;
    if !out.status.success() {
        bail!("tar -tf falló con código {:?}", out.status.code());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        let is_dir = line.ends_with('/') || line.ends_with('\\');
        if let Some(clean) = sanitize_archive_rel_path(line) {
            entries.push(ArchiveEntryInfo {
                path: clean,
                size_bytes: 0,
                is_dir,
            });
        }
    }
    entries.sort_by_key(|e| e.path.to_lowercase());
    Ok(entries)
}

/// Extracts an entire archive safely into `dest_dir`, reporting cumulative extracted bytes via
/// `on_progress(done_bytes, total_bytes)`. Uses native Rust extractors first and falls back to
/// external CLI tools (`7z`, `unrar`, `tar`) when needed (e.g. legacy compression variants).
pub fn extract_archive<F>(
    archive_path: &Path,
    dest_dir: &Path,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<String>
where
    F: FnMut(u64, u64),
{
    let format = detect_archive_format(archive_path).with_context(|| {
        format!(
            "Formato de archivo comprimido no soportado: {}",
            archive_path.display()
        )
    })?;

    if cancel.load(Ordering::Relaxed) {
        bail!("Extracción cancelada por el usuario.");
    }

    fs::create_dir_all(dest_dir)
        .with_context(|| format!("No se pudo crear carpeta temporal: {}", dest_dir.display()))?;

    let native_res = match format {
        ArchiveFormat::Zip => extract_zip_native(archive_path, dest_dir, cancel, &mut on_progress)
            .map(|()| "Nativo (ZIP)".to_string()),
        ArchiveFormat::SevenZ => {
            extract_sevenz_native(archive_path, dest_dir, cancel, &mut on_progress)
                .map(|()| "Nativo (7Z)".to_string())
        }
        ArchiveFormat::Rar => extract_rar_native(archive_path, dest_dir, cancel, &mut on_progress)
            .map(|()| "Nativo (RAR)".to_string()),
    };

    if let Ok(backend) = native_res {
        return Ok(backend);
    }
    let native_err = native_res.unwrap_err();
    if cancel.load(Ordering::Relaxed) {
        return Err(native_err);
    }

    clear_directory_contents(dest_dir);

    let tools = detect_external_archive_tools();
    if let Some(ref seven_z) = tools.seven_zip {
        match run_cli_with_cancel(
            seven_z,
            &[
                "x",
                "-y",
                "-bd",
                &format!("-o{}", dest_dir.display()),
            ],
            archive_path,
            None,
            cancel,
        ) {
            Ok(()) => {
                on_progress(1, 1);
                return Ok("7-Zip CLI".to_string());
            }
            Err(_) if !cancel.load(Ordering::Relaxed) => clear_directory_contents(dest_dir),
            Err(e) => return Err(e),
        }
    }

    if format == ArchiveFormat::Rar
        && let Some(ref unrar) = tools.unrar
    {
        let mut dest_arg = dest_dir.to_string_lossy().to_string();
        if !dest_arg.ends_with(['/', '\\']) {
            dest_arg.push(std::path::MAIN_SEPARATOR);
        }
        match run_cli_with_cancel(
            unrar,
            &["x", "-y", "-o+"],
            archive_path,
            Some(&dest_arg),
            cancel,
        ) {
            Ok(()) => {
                on_progress(1, 1);
                return Ok("UnRAR CLI".to_string());
            }
            Err(_) if !cancel.load(Ordering::Relaxed) => clear_directory_contents(dest_dir),
            Err(e) => return Err(e),
        }
    }

    if let Some(ref tar) = tools.bsdtar {
        let dest_str = dest_dir.to_string_lossy().to_string();
        match run_cli_with_cancel(tar, &["-xf"], archive_path, Some(&dest_str), cancel) {
            Ok(()) => {
                on_progress(1, 1);
                return Ok("bsdtar".to_string());
            }
            Err(_) if !cancel.load(Ordering::Relaxed) => clear_directory_contents(dest_dir),
            Err(e) => return Err(e),
        }
    }

    Err(native_err).with_context(|| {
        format!(
            "No se pudo descomprimir {} (si tiene contraseña o compresión exótica, instala 7-Zip o WinRAR)",
            archive_path.display()
        )
    })
}

fn extract_zip_native<F>(
    archive_path: &Path,
    dest_dir: &Path,
    cancel: &AtomicBool,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(u64, u64),
{
    let file = File::open(archive_path)
        .with_context(|| format!("No se pudo abrir ZIP: {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut total_bytes = 0u64;
    let meta = archive.metadata();
    for idx in 0..meta.len() {
        let e = meta.entry(idx)?;
        if !e.is_dir() {
            total_bytes = total_bytes.saturating_add(e.size());
        }
    }

    let mut done_bytes = 0u64;
    on_progress(0, total_bytes.max(1));
    let mut buf = vec![0u8; CHUNK_SIZE];

    for idx in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Extracción cancelada por el usuario.");
        }
        let mut entry = archive.by_index(idx)?;
        let raw_name = entry.name()?.to_string();
        let Some(clean_rel) = sanitize_archive_rel_path(&raw_name) else {
            continue;
        };
        let target = join_clean_rel(dest_dir, &clean_rel);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)
            .with_context(|| format!("No se pudo crear archivo extraído: {}", target.display()))?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("Extracción cancelada por el usuario.");
            }
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done_bytes = done_bytes.saturating_add(n as u64);
            on_progress(done_bytes, total_bytes.max(done_bytes).max(1));
        }
    }

    Ok(())
}

fn extract_sevenz_native<F>(
    archive_path: &Path,
    dest_dir: &Path,
    cancel: &AtomicBool,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(u64, u64),
{
    let archive_meta = sevenz_rust2::Archive::open(archive_path)
        .with_context(|| format!("No se pudo abrir 7Z: {}", archive_path.display()))?;
    let total_bytes: u64 = archive_meta
        .files
        .iter()
        .filter(|e| !e.is_directory() && !e.is_anti_item)
        .map(|e| e.size())
        .sum();
    drop(archive_meta);

    let mut done_bytes = 0u64;
    on_progress(0, total_bytes.max(1));
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut cancelled = false;

    let res = sevenz_rust2::decompress_file_with_extract_fn(
        archive_path,
        dest_dir,
        |entry, reader, _suggested_dest| {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return Err(sevenz_rust2::Error::Other("Extracción cancelada".into()));
            }
            if entry.is_anti_item {
                return Ok(true);
            }
            let Some(clean_rel) = sanitize_archive_rel_path(entry.name()) else {
                return Ok(true);
            };
            let target = join_clean_rel(dest_dir, &clean_rel);
            if entry.is_directory() {
                fs::create_dir_all(&target)
                    .map_err(|e| sevenz_rust2::Error::Io(e, "mkdir".into()))?;
                return Ok(true);
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| sevenz_rust2::Error::Io(e, "mkdir parent".into()))?;
            }
            let mut out = File::create(&target)
                .map_err(|e| sevenz_rust2::Error::Io(e, target.to_string_lossy().into_owned().into()))?;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    return Err(sevenz_rust2::Error::Other("Extracción cancelada".into()));
                }
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| sevenz_rust2::Error::Io(e, "read 7z".into()))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .map_err(|e| sevenz_rust2::Error::Io(e, "write 7z".into()))?;
                done_bytes = done_bytes.saturating_add(n as u64);
                on_progress(done_bytes, total_bytes.max(done_bytes).max(1));
            }
            Ok(true)
        },
    );

    if cancelled {
        bail!("Extracción cancelada por el usuario.");
    }
    res.with_context(|| format!("Error descomprimiendo 7Z {}", archive_path.display()))?;
    Ok(())
}

fn extract_rar_native<F>(
    archive_path: &Path,
    dest_dir: &Path,
    cancel: &AtomicBool,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(u64, u64),
{
    let total_bytes: u64 = inspect_rar_native(archive_path)?
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.size_bytes)
        .sum();
    let mut done_bytes = 0u64;
    on_progress(0, total_bytes.max(1));

    let mut archive = unrar_ng::Archive::new(archive_path)
        .open_for_processing()
        .map_err(|e| anyhow::anyhow!("No se pudo abrir RAR {}: {e}", archive_path.display()))?;

    while let Some(header) = archive
        .read_header()
        .map_err(|e| anyhow::anyhow!("Error leyendo entrada RAR: {e}"))?
    {
        if cancel.load(Ordering::Relaxed) {
            bail!("Extracción cancelada por el usuario.");
        }
        let raw = header.entry().filename.to_string_lossy().to_string();
        let entry_size = header.entry().unpacked_size;
        let is_file = header.entry().is_file();

        if let Some(clean_rel) = sanitize_archive_rel_path(&raw) {
            let target = join_clean_rel(dest_dir, &clean_rel);
            if !is_file {
                fs::create_dir_all(&target)?;
                archive = header
                    .skip()
                    .map_err(|e| anyhow::anyhow!("Error saltando carpeta RAR: {e}"))?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                archive = header
                    .extract_to(&target)
                    .map_err(|e| anyhow::anyhow!("Error extrayendo {clean_rel}: {e}"))?;
                done_bytes = done_bytes.saturating_add(entry_size);
                on_progress(done_bytes, total_bytes.max(done_bytes).max(1));
            }
        } else {
            archive = header
                .skip()
                .map_err(|e| anyhow::anyhow!("Error omitiendo ruta insegura en RAR: {e}"))?;
        }
    }

    Ok(())
}

fn run_cli_with_cancel(
    exe: &Path,
    pre_args: &[&str],
    archive_path: &Path,
    post_dest_dir: Option<&str>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut cmd = Command::new(exe);
    cmd.args(pre_args)
        .arg(archive_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(dest) = post_dest_dir {
        if exe
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("tar") || n.eq_ignore_ascii_case("tar.exe"))
        {
            cmd.arg("-C").arg(dest);
        } else {
            cmd.arg(dest);
        }
    }
    apply_no_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("No se pudo ejecutar {}", exe.display()))?;

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Extracción cancelada por el usuario.");
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let mut err_buf = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = io::Read::read_to_string(&mut stderr, &mut err_buf);
            }
            bail!(
                "{} terminó con código {:?}: {}",
                exe.display(),
                status.code(),
                err_buf.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn join_clean_rel(base: &Path, clean_rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for seg in clean_rel.split('/') {
        out.push(seg);
    }
    out
}

fn clear_directory_contents(dir: &Path) {
    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn sanitize_archive_rel_path_blocks_traversal_and_normalizes_slashes() {
        assert_eq!(sanitize_archive_rel_path("../evil.bin"), None);
        assert_eq!(sanitize_archive_rel_path("sub/../../evil.bin"), None);
        assert_eq!(sanitize_archive_rel_path(r"C:\Windows\win.ini"), None);
        assert_eq!(sanitize_archive_rel_path("/"), None);
        assert_eq!(
            sanitize_archive_rel_path(r"\GameFolder\Track 01.bin"),
            Some("GameFolder/Track 01.bin".to_string())
        );
        assert_eq!(
            sanitize_archive_rel_path("./Sub//Game.cue"),
            Some("Sub/Game.cue".to_string())
        );
    }

    #[test]
    fn detects_secondary_rar_volumes() {
        assert!(!is_secondary_rar_volume("Castlevania.rar"));
        assert!(!is_secondary_rar_volume("Castlevania.part1.rar"));
        assert!(!is_secondary_rar_volume("Castlevania.part01.rar"));
        assert!(!is_secondary_rar_volume("Castlevania.part001.rar"));
        assert!(is_secondary_rar_volume("Castlevania.part2.rar"));
        assert!(is_secondary_rar_volume("Castlevania.part02.rar"));
        assert!(is_secondary_rar_volume("Castlevania.part10.rar"));
        assert!(is_secondary_rar_volume("Castlevania.r00"));
        assert!(is_secondary_rar_volume("Castlevania.r15"));
    }

    #[test]
    fn zip_inspect_and_extract_round_trip() {
        let dir = tempdir().unwrap();
        let zip_path = dir.path().join("DiscGame.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();

        writer.start_file("DiscGame/Game.cue", opts).unwrap();
        writer
            .write_all(b"FILE \"Game.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n")
            .unwrap();
        writer.start_file("DiscGame/Game.bin", opts).unwrap();
        writer.write_all(&[0xABu8; 4096]).unwrap();
        writer.finish().unwrap();

        let inspection = inspect_archive(&zip_path).unwrap();
        assert_eq!(inspection.format, ArchiveFormat::Zip);
        assert_eq!(inspection.backend_used, "Nativo (ZIP)");
        assert_eq!(inspection.entries.len(), 2);

        let extract_dir = dir.path().join("out");
        let cancel = AtomicBool::new(false);
        let backend = extract_archive(&zip_path, &extract_dir, &cancel, |_, _| {}).unwrap();
        assert_eq!(backend, "Nativo (ZIP)");
        assert!(extract_dir.join("DiscGame").join("Game.cue").is_file());
        assert_eq!(
            fs::read(extract_dir.join("DiscGame").join("Game.bin"))
                .unwrap()
                .len(),
            4096
        );
    }

    #[test]
    fn sevenz_inspect_and_extract_round_trip() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src_7z");
        let game_dir = src_dir.join("MetalGear");
        fs::create_dir_all(&game_dir).unwrap();
        fs::write(
            game_dir.join("Metal Gear Solid.cue"),
            b"FILE \"Metal Gear Solid.bin\" BINARY\n",
        )
        .unwrap();
        fs::write(game_dir.join("Metal Gear Solid.bin"), vec![0x5Au8; 2352]).unwrap();

        let archive_path = dir.path().join("MetalGear.7z");
        sevenz_rust2::compress_to_path(&src_dir, &archive_path).unwrap();

        let inspection = inspect_archive(&archive_path).unwrap();
        assert_eq!(inspection.format, ArchiveFormat::SevenZ);
        assert_eq!(inspection.backend_used, "Nativo (7Z)");
        assert!(
            inspection
                .entries
                .iter()
                .any(|e| e.path.ends_with("Metal Gear Solid.cue"))
        );

        let extract_dir = dir.path().join("out_7z");
        let cancel = AtomicBool::new(false);
        let backend = extract_archive(&archive_path, &extract_dir, &cancel, |_, _| {}).unwrap();
        assert_eq!(backend, "Nativo (7Z)");
        assert!(
            extract_dir
                .join("MetalGear")
                .join("Metal Gear Solid.cue")
                .is_file()
        );
    }

    fn ieee_crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn push_vint(buf: &mut Vec<u8>, mut val: u64) {
        loop {
            let mut byte = (val & 0x7F) as u8;
            val >>= 7;
            if val != 0 {
                byte |= 0x80;
            }
            buf.push(byte);
            if val == 0 {
                break;
            }
        }
    }

    fn push_rar5_block(out: &mut Vec<u8>, body: &[u8]) {
        let mut size_and_body = Vec::new();
        push_vint(&mut size_and_body, body.len() as u64);
        size_and_body.extend_from_slice(body);
        let crc = ieee_crc32(&size_and_body);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size_and_body);
    }

    fn build_minimal_stored_rar5(file_name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        // RAR 5.0 signature
        out.extend_from_slice(b"Rar!\x1A\x07\x01\x00");

        // 1. Main header (type = 1, flags = 0, archive_flags = 0)
        let mut main_body = Vec::new();
        push_vint(&mut main_body, 1);
        push_vint(&mut main_body, 0);
        push_vint(&mut main_body, 0);
        push_rar5_block(&mut out, &main_body);

        // 2. File header (type = 2, flags = 0x0002 = has data area)
        let mut file_body = Vec::new();
        push_vint(&mut file_body, 2);
        push_vint(&mut file_body, 0x0002);
        push_vint(&mut file_body, data.len() as u64); // DataSize
        push_vint(&mut file_body, 0x0004); // FileFlags: CRC32 present
        push_vint(&mut file_body, data.len() as u64); // UnpackedSize
        push_vint(&mut file_body, 0); // FileAttributes
        file_body.extend_from_slice(&ieee_crc32(data).to_le_bytes()); // DataCRC32
        push_vint(&mut file_body, 0); // CompressionInfo: Store
        push_vint(&mut file_body, 0); // HostOS: Windows
        push_vint(&mut file_body, file_name.len() as u64);
        file_body.extend_from_slice(file_name.as_bytes());
        push_rar5_block(&mut out, &file_body);
        out.extend_from_slice(data);

        // 3. End of archive header (type = 5, flags = 0, end_flags = 0)
        let mut end_body = Vec::new();
        push_vint(&mut end_body, 5);
        push_vint(&mut end_body, 0);
        push_vint(&mut end_body, 0);
        push_rar5_block(&mut out, &end_body);

        out
    }

    #[test]
    fn rar_inspect_and_extract_round_trip() {
        let dir = tempdir().unwrap();
        let rar_path = dir.path().join("Chrono.rar");
        let payload = b"ROM-GBA-TEST-PAYLOAD-1234";
        let bytes = build_minimal_stored_rar5("Chrono/Test.gba", payload);
        fs::write(&rar_path, bytes).unwrap();

        let inspection = inspect_archive(&rar_path).unwrap();
        assert_eq!(inspection.format, ArchiveFormat::Rar);
        assert_eq!(inspection.backend_used, "Nativo (RAR)");
        assert_eq!(inspection.entries.len(), 1);
        assert_eq!(inspection.entries[0].path, "Chrono/Test.gba");
        assert_eq!(inspection.entries[0].size_bytes, payload.len() as u64);

        let extract_dir = dir.path().join("out_rar");
        let cancel = AtomicBool::new(false);
        let backend = extract_archive(&rar_path, &extract_dir, &cancel, |_, _| {}).unwrap();
        assert_eq!(backend, "Nativo (RAR)");
        assert_eq!(
            fs::read(extract_dir.join("Chrono").join("Test.gba")).unwrap(),
            payload
        );
    }
}
