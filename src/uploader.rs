use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::archives::{self, ArchiveEntryInfo, ArchiveFormat};
use crate::chd;
use crate::config::MediaPlacement;
use crate::imaging;
use crate::matcher::CoverIndex;
use crate::models::{CoverImage, MatchCandidate, MatchKind};
use crate::scanner::{
    RawFileEntry, filter_directory_games, is_ignored_dir, is_ignored_non_game_file,
    is_supported_game_extension, split_stem_ext,
};
use crate::storage::{StorageClient, join_storage_path, posix_join, posix_parent};
use crate::systems::{
    SystemId, detect_game_system, detect_system_from_extension, detect_system_from_folder_name,
    detect_system_from_stem_tags,
};

const OPTICAL_SHEET_EXTS: &[&str] = &["cue", "ccd", "gdi", "mds"];
const DISC_AUX_TRACK_EXTS: &[&str] = &["bin", "img", "raw", "sub", "wav"];
const OPTICAL_DISC_EXTS: &[&str] = &["chd", "iso", "cso", "pbp", "cdi"];

/// One file inside an atomic multi-file game folder (e.g., `.cue` + `.bin` tracks).
#[derive(Debug, Clone)]
pub struct FolderFile {
    pub src_path: PathBuf,
    pub rel_path: String,
    pub size_bytes: u64,
}

/// Source payload queued for bulk upload to MiSTer storage.
#[derive(Debug, Clone)]
pub enum UploadPayload {
    SingleFile {
        src_path: PathBuf,
        file_name: String,
        stem: String,
        ext: String,
        size_bytes: u64,
    },
    AtomicFolder {
        src_dir: PathBuf,
        folder_name: String,
        files: Vec<FolderFile>,
        launch_stems: Vec<String>,
        convertible_sheets: Vec<String>,
        primary_stem: String,
        total_bytes: u64,
    },
    Archive {
        archive_path: PathBuf,
        archive_name: String,
        folder_name: String,
        format: ArchiveFormat,
        inspect_backend: String,
        compressed_bytes: u64,
        uncompressed_bytes: u64,
        inner_files: Vec<ArchiveEntryInfo>,
        inner_root: String,
        is_disc_set: bool,
        convertible_sheets: Vec<String>,
        launch_stems: Vec<String>,
        primary_stem: String,
        single_inner_file: Option<(String, String)>,
    },
}

impl UploadPayload {
    /// Human-readable display label (file name, folder name, or archive game label).
    pub fn display_name(&self) -> &str {
        match self {
            Self::SingleFile { file_name, .. } => file_name,
            Self::AtomicFolder { folder_name, .. } => folder_name,
            Self::Archive {
                single_inner_file: Some((rel_path, _)),
                ..
            } => rel_path.rsplit('/').next().unwrap_or(rel_path),
            Self::Archive {
                folder_name,
                archive_name,
                ..
            } => {
                if folder_name.is_empty() {
                    archive_name
                } else {
                    folder_name
                }
            }
        }
    }

    /// Primary ROM/playlist stem used for matching a cover image.
    pub fn primary_stem(&self) -> &str {
        match self {
            Self::SingleFile { stem, .. } => stem,
            Self::AtomicFolder { primary_stem, .. } | Self::Archive { primary_stem, .. } => {
                primary_stem
            }
        }
    }

    /// Launchable stems inside this payload that ConsoleMode maps to covers in `media/`.
    pub fn launch_stems(&self) -> Vec<String> {
        match self {
            Self::SingleFile { stem, .. } => vec![stem.clone()],
            Self::AtomicFolder {
                launch_stems,
                primary_stem,
                ..
            }
            | Self::Archive {
                launch_stems,
                primary_stem,
                ..
            } => {
                if launch_stems.is_empty() {
                    vec![primary_stem.clone()]
                } else {
                    launch_stems.clone()
                }
            }
        }
    }

    /// Total payload size in bytes (uncompressed size for archives).
    pub fn total_bytes(&self) -> u64 {
        match self {
            Self::SingleFile { size_bytes, .. } => *size_bytes,
            Self::AtomicFolder { total_bytes, .. } => *total_bytes,
            Self::Archive {
                uncompressed_bytes, ..
            } => *uncompressed_bytes,
        }
    }

    /// Number of logical source files inside this payload.
    pub fn file_count(&self) -> usize {
        match self {
            Self::SingleFile { .. } => 1,
            Self::AtomicFolder { files, .. } => files.len(),
            Self::Archive {
                single_inner_file: Some(_),
                ..
            } => 1,
            Self::Archive { inner_files, .. } => inner_files.len().max(1),
        }
    }

    /// Expected file count transferred to MiSTer taking CHD conversion into account.
    pub fn effective_output_file_count(&self, convert_to_chd: bool) -> usize {
        if convert_to_chd && self.can_convert_to_chd() {
            let sheets = self.convertible_sheets();
            if sheets.len() <= 1 {
                1
            } else {
                let has_m3u = match self {
                    Self::AtomicFolder { files, .. } => {
                        files.iter().any(|f| f.rel_path.to_ascii_lowercase().ends_with(".m3u"))
                    }
                    Self::Archive { inner_files, .. } => inner_files
                        .iter()
                        .any(|f| f.path.to_ascii_lowercase().ends_with(".m3u")),
                    Self::SingleFile { .. } => false,
                };
                sheets.len() + usize::from(has_m3u)
            }
        } else {
            self.file_count()
        }
    }

    /// Unique key used to deduplicate queued sources.
    pub fn source_identity(&self) -> (PathBuf, String) {
        match self {
            Self::SingleFile { src_path, .. } => (canonical_or_self(src_path), String::new()),
            Self::AtomicFolder {
                src_dir,
                folder_name,
                ..
            } => (canonical_or_self(src_dir), format!("folder:{folder_name}")),
            Self::Archive {
                archive_path,
                inner_root,
                single_inner_file,
                ..
            } => {
                let sub = single_inner_file
                    .as_ref()
                    .map(|(p, _)| format!("file:{p}"))
                    .unwrap_or_else(|| format!("root:{inner_root}"));
                (canonical_or_self(archive_path), sub)
            }
        }
    }

    /// Returns `true` if this payload has `.cue` / `.gdi` (or supported disc sheets) convertible to `.chd`.
    pub fn can_convert_to_chd(&self) -> bool {
        !self.convertible_sheets().is_empty()
    }

    /// Lists convertible sheet relative paths (e.g. `Game.cue`).
    pub fn convertible_sheets(&self) -> &[String] {
        match self {
            Self::SingleFile { .. } => &[],
            Self::AtomicFolder {
                convertible_sheets, ..
            }
            | Self::Archive {
                convertible_sheets, ..
            } => convertible_sheets,
        }
    }

    /// Returns the archive container format when originating from `.zip`/`.7z`/`.rar`.
    pub fn archive_format(&self) -> Option<ArchiveFormat> {
        match self {
            Self::Archive { format, .. } => Some(*format),
            _ => None,
        }
    }

    /// Returns `true` for standalone optical disc/playlist files (including 1-file archives or
    /// single-disc `.cue`/`.gdi` sets collapsed into a single `.chd`) that can optionally be
    /// wrapped inside `<System>/<stem>/<file>` when `wrap_disc_in_folder` is enabled.
    pub fn is_disc_file(&self) -> bool {
        match self {
            Self::SingleFile { ext, .. } => {
                OPTICAL_DISC_EXTS.contains(&ext.as_str()) || ext == "m3u"
            }
            Self::AtomicFolder { .. } => false,
            Self::Archive {
                single_inner_file: Some((_, ext)),
                ..
            } => OPTICAL_DISC_EXTS.contains(&ext.as_str()) || ext == "m3u",
            Self::Archive { .. } => false,
        }
    }
}

/// Lifecycle state for an item in the bulk upload queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadStatus {
    Ready,
    NeedsSystem,
    Uploading,
    Done,
    SkippedExists,
    Failed(String),
}

impl UploadStatus {
    /// Short Spanish label for UI badges.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ready => "Listo",
            Self::NeedsSystem => "Sin consola",
            Self::Uploading => "Subiendo...",
            Self::Done => "Subido",
            Self::SkippedExists => "Ya existe",
            Self::Failed(_) => "Error",
        }
    }
}

/// One staged game (single ROM, atomic folder, or compressed archive) with destination routing
/// and matched cover.
#[derive(Debug, Clone)]
pub struct UploadItem {
    pub payload: UploadPayload,
    pub detected_system: Option<SystemId>,
    pub target_system: Option<SystemId>,
    pub target_system_dir: String,
    pub target_game_path: String,
    pub target_media_dir: String,
    pub convert_to_chd: bool,
    pub convert_to_chd_manual: bool,
    pub selected_cover_idx: Option<usize>,
    pub cover_score: f64,
    pub cover_match_kind: Option<MatchKind>,
    pub cover_candidates: Vec<MatchCandidate>,
    pub upload_cover: bool,
    pub selected: bool,
    pub status: UploadStatus,
}

/// Options governing destination routing, CHD conversion, extraction temp dir, cover assignment,
/// and streaming upload.
#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub default_system: Option<SystemId>,
    pub overwrite_games: bool,
    pub overwrite_covers: bool,
    pub convert_covers_to_png: bool,
    pub strict_system_match: bool,
    pub cover_threshold: f64,
    pub wrap_disc_in_folder: bool,
    pub media_placement: MediaPlacement,
    pub convert_bin_cue_to_chd: bool,
    pub chdman_path: Option<PathBuf>,
    pub temp_dir: Option<PathBuf>,
    pub dry_run: bool,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            default_system: None,
            overwrite_games: false,
            overwrite_covers: false,
            convert_covers_to_png: true,
            strict_system_match: true,
            cover_threshold: 0.78,
            wrap_disc_in_folder: false,
            media_placement: MediaPlacement::RootMedia,
            convert_bin_cue_to_chd: false,
            chdman_path: None,
            temp_dir: None,
            dry_run: false,
        }
    }
}

/// Progress events emitted while executing bulk uploads, extractions, and CHD conversions.
#[derive(Debug, Clone)]
pub enum UploadEvent {
    ItemStarting {
        item_idx: usize,
        current: usize,
        total: usize,
        label: String,
    },
    StageProgress {
        stage: String,
        done: u64,
        total: u64,
    },
    FileBytes {
        file_name: String,
        sent: u64,
        total: u64,
    },
    ItemFinished {
        item_idx: usize,
        status: UploadStatus,
    },
}

/// Final summary produced after executing a bulk upload batch.
#[derive(Debug, Clone, Default)]
pub struct UploadSummary {
    pub total_items: usize,
    pub games_uploaded: usize,
    pub archives_extracted: usize,
    pub chds_created: usize,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub covers_uploaded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

/// Formats a byte count into a human-readable string (`KB`, `MB`, `GB`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Discovers uploadable games, atomic `.bin`/`.cue`/`.m3u` folders, and compressed
/// `.zip`/`.7z`/`.rar` bundles from any mix of local files and directories.
pub fn discover_upload_sources(
    paths: &[PathBuf],
) -> Result<Vec<(UploadPayload, Option<SystemId>)>> {
    let mut out = Vec::new();
    let mut seen_keys: HashSet<(PathBuf, String)> = HashSet::new();

    for path in paths {
        if !path.exists() {
            bail!("La ruta de origen no existe: {}", path.display());
        }
        let parent_hint = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| detect_system_from_folder_name(&n.to_string_lossy()));

        if path.is_file() {
            for (payload, sys) in inspect_single_file_or_archive(path, parent_hint)? {
                let key = payload.source_identity();
                if seen_keys.insert(key) {
                    out.push((payload, sys));
                }
            }
        } else if path.is_dir() {
            discover_dir(path, parent_hint, &mut seen_keys, &mut out)?;
        }
    }

    Ok(out)
}

fn canonical_or_self(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn inspect_single_file_or_archive(
    path: &Path,
    inherited_system: Option<SystemId>,
) -> Result<Vec<(UploadPayload, Option<SystemId>)>> {
    let Some(file_name_os) = path.file_name() else {
        return Ok(Vec::new());
    };
    let file_name = file_name_os.to_string_lossy().to_string();
    let (stem_slice, ext) = split_stem_ext(&file_name);
    if is_ignored_non_game_file(&file_name, &ext) || !is_supported_game_extension_or_archive(&ext) {
        return Ok(Vec::new());
    }

    if let Some(format) = archives::detect_archive_format(path) {
        if archives::is_secondary_rar_volume(&file_name) {
            return Ok(Vec::new());
        }
        return discover_archive(path, &file_name, stem_slice, format, inherited_system);
    }

    // If the user explicitly selected a loose .cue or .gdi sheet, package it with its sibling
    // disc track files (.bin/.img/.wav/etc.) so tracks aren't abandoned and CHD conversion works.
    if matches!(ext.as_str(), "cue" | "gdi")
        && let Some(bundle) = build_sheet_bundle_from_file(path, &file_name, stem_slice, inherited_system)?
    {
        return Ok(vec![bundle]);
    }

    let stem = stem_slice.to_string();
    let size_bytes = fs::metadata(path)
        .with_context(|| format!("No se pudo leer tamaño de {}", path.display()))?
        .len();

    let sys = detect_system_from_extension(&ext)
        .or(inherited_system)
        .or_else(|| detect_system_from_stem_tags(&stem));

    Ok(vec![(
        UploadPayload::SingleFile {
            src_path: path.to_path_buf(),
            file_name,
            stem,
            ext,
            size_bytes,
        },
        sys,
    )])
}

fn is_supported_game_extension_or_archive(ext: &str) -> bool {
    ext == "rar" || is_supported_game_extension(ext)
}

fn build_sheet_bundle_from_file(
    sheet_path: &Path,
    file_name: &str,
    stem: &str,
    inherited_system: Option<SystemId>,
) -> Result<Option<(UploadPayload, Option<SystemId>)>> {
    let Some(parent) = sheet_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(None);
    };

    let mut matched_paths = vec![sheet_path.to_path_buf()];
    let mut seen_names: HashSet<String> = HashSet::from([file_name.to_ascii_lowercase()]);

    // 1) Parse exact FILE references in .cue if possible
    let sheet_ext = split_stem_ext(file_name).1;
    if sheet_ext == "cue"
        && let Ok(text) = fs::read_to_string(sheet_path)
    {
        for line in text.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("FILE").or_else(|| trimmed.strip_prefix("file")) {
                let after = rest.trim_start();
                let track_ref = if let Some(inner) = after.strip_prefix('"') {
                    inner.split('"').next().unwrap_or("")
                } else {
                    after.split_whitespace().next().unwrap_or("")
                };
                let base_ref = track_ref.rsplit(['/', '\\']).next().unwrap_or(track_ref);
                let candidate = parent.join(base_ref);
                if candidate.is_file() {
                    let lower = base_ref.to_ascii_lowercase();
                    if seen_names.insert(lower) {
                        matched_paths.push(candidate);
                    }
                }
            }
        }
    }

    // 2) Also grab track files in the same directory sharing the sheet stem prefix (e.g. Game (Track 1).bin)
    if let Ok(entries) = fs::read_dir(parent) {
        let stem_lower = stem.to_ascii_lowercase();
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let (fstem, fext) = split_stem_ext(&name);
            if DISC_AUX_TRACK_EXTS.contains(&fext.as_str())
                && fstem.to_ascii_lowercase().starts_with(&stem_lower)
                && seen_names.insert(name.to_ascii_lowercase())
            {
                matched_paths.push(p);
            }
        }
    }

    if matched_paths.len() <= 1 && sheet_ext != "cue" && sheet_ext != "gdi" {
        return Ok(None);
    }

    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    for p in matched_paths {
        let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let rel_name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        total_bytes = total_bytes.saturating_add(size);
        files.push(FolderFile {
            src_path: p,
            rel_path: rel_name,
            size_bytes: size,
        });
    }
    files.sort_by_key(|f| f.rel_path.to_lowercase());

    let convertible_sheets = if chd::is_convertible_sheet_ext(&sheet_ext, false) {
        vec![file_name.to_string()]
    } else {
        Vec::new()
    };

    let sys = inherited_system.or_else(|| detect_system_from_stem_tags(stem));
    Ok(Some((
        UploadPayload::AtomicFolder {
            src_dir: parent.to_path_buf(),
            folder_name: stem.to_string(),
            files,
            launch_stems: vec![stem.to_string()],
            convertible_sheets,
            primary_stem: stem.to_string(),
            total_bytes,
        },
        sys,
    )))
}

fn discover_dir(
    dir: &Path,
    inherited_system: Option<SystemId>,
    seen_keys: &mut HashSet<(PathBuf, String)>,
    out: &mut Vec<(UploadPayload, Option<SystemId>)>,
) -> Result<()> {
    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !dir_name.is_empty() && is_ignored_dir(&dir_name) {
        return Ok(());
    }

    let mut direct_subdirs = Vec::new();
    let mut direct_files = Vec::new();

    for entry in fs::read_dir(dir)
        .with_context(|| format!("No se pudo leer carpeta origen: {}", dir.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if ft.is_dir() {
            if !is_ignored_dir(&name) {
                direct_subdirs.push(entry.path());
            }
        } else if ft.is_file() && !name.starts_with('.') && !name.starts_with("._") {
            direct_files.push(entry.path());
        }
    }

    direct_subdirs.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    direct_files.sort_by_key(|p| p.to_string_lossy().to_lowercase());

    let dir_system = detect_system_from_folder_name(&dir_name);
    if is_atomic_game_folder(&direct_subdirs, &direct_files, dir_system) {
        if let Some((payload, sys)) = build_atomic_folder(dir, &dir_name, inherited_system)? {
            let key = payload.source_identity();
            if seen_keys.insert(key) {
                out.push((payload, sys));
            }
        }
        return Ok(());
    }

    let next_inherited = dir_system.or(inherited_system);

    // Handle archives separately from loose ROMs so `.7z`/`.zip`/`.rar` inspect internally.
    let mut non_archive_entries: Vec<RawFileEntry> = Vec::new();
    for p in &direct_files {
        let Some(fname) = p.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let (_, ext) = split_stem_ext(&fname);
        if is_ignored_non_game_file(&fname, &ext) {
            continue;
        }
        if let Some(format) = archives::detect_archive_format(p) {
            if archives::is_secondary_rar_volume(&fname) {
                continue;
            }
            let (arch_stem, _) = split_stem_ext(&fname);
            for (payload, sys) in discover_archive(p, &fname, arch_stem, format, next_inherited)? {
                let key = payload.source_identity();
                if seen_keys.insert(key) {
                    out.push((payload, sys));
                }
            }
        } else {
            non_archive_entries.push(RawFileEntry {
                full_path: p.to_string_lossy().to_string(),
                parent_dir: dir.to_string_lossy().to_string(),
                relative_dir: String::new(),
                file_name: fname,
            });
        }
    }

    for (raw, stem, ext) in filter_directory_games(non_archive_entries) {
        let path = PathBuf::from(&raw.full_path);
        let size_bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let sys = detect_game_system("", "", &stem, &ext).or(next_inherited);
        let payload = UploadPayload::SingleFile {
            src_path: path,
            file_name: raw.file_name,
            stem,
            ext,
            size_bytes,
        };
        let key = payload.source_identity();
        if seen_keys.insert(key) {
            out.push((payload, sys));
        }
    }

    for sub in direct_subdirs {
        discover_dir(&sub, next_inherited, seen_keys, out)?;
    }

    Ok(())
}

/// Inspects an archive's headers internally without decompressing payloads and classifies
/// contained ROMs or optical disc bundles (`.cue`/`.bin`/`.gdi`/`.m3u`) into upload items.
fn discover_archive(
    archive_path: &Path,
    archive_name: &str,
    archive_stem: &str,
    format: ArchiveFormat,
    inherited_system: Option<SystemId>,
) -> Result<Vec<(UploadPayload, Option<SystemId>)>> {
    let compressed_bytes = fs::metadata(archive_path)
        .with_context(|| format!("No se pudo leer tamaño de {}", archive_path.display()))?
        .len();

    let inspection = match archives::inspect_archive(archive_path) {
        Ok(insp) => insp,
        Err(_) if format == ArchiveFormat::Zip => {
            // Fall back cleanly for non-standard zips (or treat as opaque zip if desired)
            return Ok(vec![(
                UploadPayload::SingleFile {
                    src_path: archive_path.to_path_buf(),
                    file_name: archive_name.to_string(),
                    stem: archive_stem.to_string(),
                    ext: "zip".to_string(),
                    size_bytes: compressed_bytes,
                },
                inherited_system.or_else(|| detect_system_from_stem_tags(archive_stem)),
            )]);
        }
        Err(err) => return Err(err),
    };

    let format = inspection.format;
    let inspect_backend = inspection.backend_used;
    let clean_entries: Vec<ArchiveEntryInfo> = inspection
        .entries
        .into_iter()
        .filter(|e| !e.is_dir && !is_ignored_archive_entry(&e.path))
        .collect();

    if clean_entries.is_empty() {
        return Ok(Vec::new());
    }

    // Preserve Arcade/MAME `.zip` ROM sets intact rather than unpacking them.
    if format == ArchiveFormat::Zip
        && (inherited_system == Some(SystemId::Arcade)
            || !archive_has_recognized_console_or_disc_content(&clean_entries))
    {
        let sys = inherited_system
            .or_else(|| detect_system_from_stem_tags(archive_stem))
            .or(Some(SystemId::Arcade));
        return Ok(vec![(
            UploadPayload::SingleFile {
                src_path: archive_path.to_path_buf(),
                file_name: archive_name.to_string(),
                stem: archive_stem.to_string(),
                ext: "zip".to_string(),
                size_bytes: compressed_bytes,
            },
            sys,
        )]);
    }

    // Check if the whole archive (or a single top-level directory inside it) forms ONE optical disc set
    // (including multi-disc folders that share one game title or contain an .m3u playlist).
    let root_hint = single_common_root_dir(&clean_entries);
    if archive_entries_form_single_disc_set(&clean_entries) {
        let item = build_archive_disc_payload(
            archive_path,
            archive_name,
            archive_stem,
            format,
            inspect_backend,
            compressed_bytes,
            clean_entries,
            root_hint,
            inherited_system,
        );
        return Ok(vec![item]);
    }

    // Group entries by their parent directory inside the archive to support archives holding
    // multiple games (e.g., pack of GBA + SNES ROMs or multiple `.cue`/`.bin` subfolders).
    let mut by_dir: BTreeMap<String, Vec<ArchiveEntryInfo>> = BTreeMap::new();
    for entry in clean_entries {
        let dir = entry
            .path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push(entry);
    }

    let chdman_avail = chd::find_chdman(None).is_some();
    let mut out = Vec::new();
    for (dir_rel, entries) in by_dir {
        let dir_sys = detect_archive_path_system(&dir_rel).or(inherited_system);
        let raw_entries: Vec<RawFileEntry> = entries
            .iter()
            .map(|e| {
                let fname = e.path.rsplit('/').next().unwrap_or(&e.path).to_string();
                RawFileEntry {
                    full_path: e.path.clone(),
                    parent_dir: dir_rel.clone(),
                    relative_dir: dir_rel.clone(),
                    file_name: fname,
                }
            })
            .collect();

        let has_sheet = entries.iter().any(|e| {
            let fname = e.path.rsplit('/').next().unwrap_or(&e.path);
            let (_, ext) = split_stem_ext(fname);
            OPTICAL_SHEET_EXTS.contains(&ext.as_str()) || ext == "m3u"
        });

        if has_sheet {
            let folder_label = if !dir_rel.is_empty() {
                dir_rel
                    .rsplit('/')
                    .next()
                    .unwrap_or(archive_stem)
                    .to_string()
            } else {
                archive_stem.to_string()
            };
            let uncompressed_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();
            let convertible_sheets = collect_convertible_sheets(&entries, chdman_avail);
            let (launch_stems, primary_stem, sheet_sys) =
                compute_disc_stems_and_system(&entries, &folder_label, dir_sys);
            let final_sys = sheet_sys.or_else(|| detect_system_from_stem_tags(archive_stem));
            out.push((
                UploadPayload::Archive {
                    archive_path: archive_path.to_path_buf(),
                    archive_name: archive_name.to_string(),
                    folder_name: folder_label,
                    format,
                    inspect_backend: inspect_backend.clone(),
                    compressed_bytes,
                    uncompressed_bytes,
                    inner_files: entries,
                    inner_root: dir_rel,
                    is_disc_set: true,
                    convertible_sheets,
                    launch_stems,
                    primary_stem,
                    single_inner_file: None,
                },
                final_sys,
            ));
            continue;
        }

        for (raw, stem, ext) in filter_directory_games(raw_entries) {
            let size_bytes = entries
                .iter()
                .find(|e| e.path == raw.full_path)
                .map(|e| e.size_bytes)
                .unwrap_or(0);
            let sys = detect_game_system(&dir_rel, "", &stem, &ext)
                .or(dir_sys)
                .or_else(|| detect_system_from_stem_tags(archive_stem));
            let single_entry = ArchiveEntryInfo {
                path: raw.full_path.clone(),
                size_bytes,
                is_dir: false,
            };
            out.push((
                UploadPayload::Archive {
                    archive_path: archive_path.to_path_buf(),
                    archive_name: archive_name.to_string(),
                    folder_name: raw.file_name.clone(),
                    format,
                    inspect_backend: inspect_backend.clone(),
                    compressed_bytes,
                    uncompressed_bytes: size_bytes,
                    inner_files: vec![single_entry],
                    inner_root: String::new(),
                    is_disc_set: false,
                    convertible_sheets: Vec::new(),
                    launch_stems: vec![stem.clone()],
                    primary_stem: stem,
                    single_inner_file: Some((raw.full_path, ext)),
                },
                sys,
            ));
        }
    }

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn build_archive_disc_payload(
    archive_path: &Path,
    archive_name: &str,
    archive_stem: &str,
    format: ArchiveFormat,
    inspect_backend: String,
    compressed_bytes: u64,
    entries: Vec<ArchiveEntryInfo>,
    inner_root: String,
    inherited_system: Option<SystemId>,
) -> (UploadPayload, Option<SystemId>) {
    let chdman_avail = chd::find_chdman(None).is_some();
    let folder_name = if !inner_root.is_empty() {
        inner_root
            .rsplit('/')
            .next()
            .unwrap_or(archive_stem)
            .to_string()
    } else {
        archive_stem.to_string()
    };
    let uncompressed_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();
    let convertible_sheets = collect_convertible_sheets(&entries, chdman_avail);
    let path_sys = detect_archive_path_system(&inner_root).or(inherited_system);
    let (launch_stems, primary_stem, disc_sys) =
        compute_disc_stems_and_system(&entries, &folder_name, path_sys);
    let final_sys = disc_sys.or_else(|| detect_system_from_stem_tags(archive_stem));

    (
        UploadPayload::Archive {
            archive_path: archive_path.to_path_buf(),
            archive_name: archive_name.to_string(),
            folder_name,
            format,
            inspect_backend,
            compressed_bytes,
            uncompressed_bytes,
            inner_files: entries,
            inner_root,
            is_disc_set: true,
            convertible_sheets,
            launch_stems,
            primary_stem,
            single_inner_file: None,
        },
        final_sys,
    )
}

fn is_ignored_archive_entry(rel_path: &str) -> bool {
    let segs: Vec<&str> = rel_path.split('/').collect();
    if segs
        .iter()
        .take(segs.len().saturating_sub(1))
        .any(|d| is_ignored_dir(d) || d.eq_ignore_ascii_case("__MACOSX"))
    {
        return true;
    }
    let Some(fname) = segs.last() else {
        return true;
    };
    let lower = fname.to_ascii_lowercase();
    if fname.starts_with('.')
        || fname.starts_with("._")
        || lower == "thumbs.db"
        || lower == "desktop.ini"
    {
        return true;
    }
    let (_, ext) = split_stem_ext(fname);
    is_ignored_non_game_file(fname, &ext)
}

fn archive_has_recognized_console_or_disc_content(entries: &[ArchiveEntryInfo]) -> bool {
    entries.iter().any(|e| {
        let fname = e.path.rsplit('/').next().unwrap_or(&e.path);
        let (_, ext) = split_stem_ext(fname);
        OPTICAL_SHEET_EXTS.contains(&ext.as_str())
            || OPTICAL_DISC_EXTS.contains(&ext.as_str())
            || ext == "m3u"
            || detect_system_from_extension(&ext).is_some()
    })
}

fn single_common_root_dir(entries: &[ArchiveEntryInfo]) -> String {
    let mut top_dirs: HashSet<&str> = HashSet::new();
    for e in entries {
        let Some((first, _)) = e.path.split_once('/') else {
            return String::new();
        };
        top_dirs.insert(first);
    }
    if top_dirs.len() == 1 {
        top_dirs.into_iter().next().unwrap_or("").to_string()
    } else {
        String::new()
    }
}

fn archive_entries_form_single_disc_set(entries: &[ArchiveEntryInfo]) -> bool {
    let mut sheet_stems = Vec::new();
    let mut disc_stems = Vec::new();
    let mut has_m3u = false;
    let mut cartridge_roms = 0usize;

    for e in entries {
        let fname = e.path.rsplit('/').next().unwrap_or(&e.path);
        let (stem, ext) = split_stem_ext(fname);
        if !is_supported_game_extension(&ext) {
            continue;
        }
        if ext == "m3u" {
            has_m3u = true;
        } else if OPTICAL_SHEET_EXTS.contains(&ext.as_str()) {
            sheet_stems.push(stem.to_string());
        } else if OPTICAL_DISC_EXTS.contains(&ext.as_str()) {
            disc_stems.push(stem.to_string());
        } else if !DISC_AUX_TRACK_EXTS.contains(&ext.as_str()) {
            cartridge_roms += 1;
        }
    }

    if cartridge_roms > 0 {
        return false;
    }
    if has_m3u {
        return true;
    }
    if !sheet_stems.is_empty() {
        let bases: HashSet<String> = sheet_stems
            .iter()
            .map(|s| strip_disc_and_tags_simple(s))
            .collect();
        return bases.len() <= 1;
    }
    if disc_stems.len() > 1 {
        let bases: HashSet<String> = disc_stems
            .iter()
            .map(|s| strip_disc_and_tags_simple(s))
            .collect();
        return bases.len() == 1;
    }
    false
}

fn detect_archive_path_system(rel_dir: &str) -> Option<SystemId> {
    for seg in rel_dir.split('/').rev() {
        if let Some(sys) = detect_system_from_folder_name(seg) {
            return Some(sys);
        }
    }
    None
}

fn collect_convertible_sheets(entries: &[ArchiveEntryInfo], chdman_avail: bool) -> Vec<String> {
    let mut out: Vec<String> = entries
        .iter()
        .filter(|e| {
            let fname = e.path.rsplit('/').next().unwrap_or(&e.path);
            let (_, ext) = split_stem_ext(fname);
            chd::is_convertible_sheet_ext(&ext, chdman_avail)
        })
        .map(|e| e.path.clone())
        .collect();
    out.sort_by_key(|p| p.to_lowercase());
    out
}

fn compute_disc_stems_and_system(
    entries: &[ArchiveEntryInfo],
    fallback_stem: &str,
    inherited_system: Option<SystemId>,
) -> (Vec<String>, String, Option<SystemId>) {
    let mut raw_entries = Vec::new();
    for e in entries {
        let fname = e.path.rsplit('/').next().unwrap_or(&e.path).to_string();
        raw_entries.push(RawFileEntry {
            full_path: e.path.clone(),
            parent_dir: String::new(),
            relative_dir: String::new(),
            file_name: fname,
        });
    }

    let filtered = filter_directory_games(raw_entries);
    let mut launch_stems = Vec::new();
    let mut seen_stems = HashSet::new();
    let mut primary_rank = u8::MAX;
    let mut primary_stem = fallback_stem.to_string();
    let mut detected_sys = inherited_system;

    for (raw, stem, ext) in &filtered {
        if seen_stems.insert(stem.to_lowercase()) {
            launch_stems.push(stem.clone());
        }
        let rank = if ext == "m3u" {
            0
        } else if OPTICAL_SHEET_EXTS.contains(&ext.as_str()) {
            1
        } else {
            2
        };
        if rank < primary_rank {
            primary_rank = rank;
            primary_stem = stem.clone();
        }
        if detected_sys.is_none() {
            detected_sys = detect_archive_path_system(&raw.full_path)
                .or_else(|| detect_system_from_extension(ext))
                .or_else(|| detect_system_from_stem_tags(stem));
        }
    }

    if detected_sys.is_none() {
        detected_sys = detect_system_from_stem_tags(fallback_stem)
            .or_else(|| detect_system_from_stem_tags(&primary_stem));
    }

    (launch_stems, primary_stem, detected_sys)
}

fn is_atomic_game_folder(
    direct_subdirs: &[PathBuf],
    direct_files: &[PathBuf],
    dir_system: Option<SystemId>,
) -> bool {
    if !direct_subdirs.is_empty() || dir_system.is_some() {
        return false;
    }

    let mut has_sheet_or_m3u = false;
    let mut disc_stems = Vec::new();
    let mut non_disc_games = 0usize;

    for path in direct_files {
        let Some(fname) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let (stem, ext) = split_stem_ext(&fname);
        if is_ignored_non_game_file(&fname, &ext) || !is_supported_game_extension(&ext) {
            continue;
        }
        if OPTICAL_SHEET_EXTS.contains(&ext.as_str()) || ext == "m3u" {
            has_sheet_or_m3u = true;
        } else if OPTICAL_DISC_EXTS.contains(&ext.as_str()) {
            disc_stems.push(stem.to_string());
        } else if ext != "bin" && ext != "img" {
            non_disc_games += 1;
        }
    }

    if has_sheet_or_m3u {
        return true;
    }
    if non_disc_games == 0 && disc_stems.len() == 1 {
        return true;
    }
    if non_disc_games == 0 && disc_stems.len() > 1 {
        let base_titles: HashSet<String> = disc_stems
            .iter()
            .map(|s| strip_disc_and_tags_simple(s))
            .collect();
        return base_titles.len() == 1;
    }

    false
}

fn strip_disc_and_tags_simple(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len());
    let mut depth = 0usize;
    for ch in stem.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch.to_ascii_lowercase()),
            _ => {}
        }
    }
    let trimmed = out.trim();
    for marker in [" - disc ", " - disk ", " - cd ", " disc ", " disk ", " cd "] {
        if let Some(pos) = trimmed.rfind(marker) {
            return trimmed[..pos].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn build_atomic_folder(
    dir: &Path,
    folder_name: &str,
    inherited_system: Option<SystemId>,
) -> Result<Option<(UploadPayload, Option<SystemId>)>> {
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let mut raw_entries = Vec::new();

    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry
            .with_context(|| format!("Error al leer carpeta de juego: {}", dir.display()))?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }
        let Some(fname) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let lower_fname = fname.to_ascii_lowercase();
        if fname.starts_with('.')
            || fname.starts_with("._")
            || lower_fname == "thumbs.db"
            || lower_fname == "desktop.ini"
        {
            continue;
        }
        let Ok(rel) = path.strip_prefix(dir) else {
            continue;
        };
        if rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(os) => Some(os.to_string_lossy()),
                _ => None,
            })
            .any(|seg| is_ignored_dir(&seg))
        {
            continue;
        }
        let rel_posix = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(os) => Some(os.to_string_lossy().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if rel_posix.is_empty() {
            continue;
        }

        let size_bytes = entry.metadata()?.len();
        total_bytes += size_bytes;
        files.push(FolderFile {
            src_path: path.to_path_buf(),
            rel_path: rel_posix,
            size_bytes,
        });

        raw_entries.push(RawFileEntry {
            full_path: path.to_string_lossy().to_string(),
            parent_dir: dir.to_string_lossy().to_string(),
            relative_dir: String::new(),
            file_name: fname,
        });
    }

    if files.is_empty() {
        return Ok(None);
    }

    files.sort_by_key(|f| f.rel_path.to_lowercase());

    let chdman_avail = chd::find_chdman(None).is_some();
    let mut convertible_sheets: Vec<String> = files
        .iter()
        .filter(|f| {
            let fname = f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path);
            let (_, ext) = split_stem_ext(fname);
            chd::is_convertible_sheet_ext(&ext, chdman_avail)
        })
        .map(|f| f.rel_path.clone())
        .collect();
    convertible_sheets.sort_by_key(|s| s.to_lowercase());

    let filtered = filter_directory_games(raw_entries);
    let mut launch_stems = Vec::new();
    let mut seen_stems = HashSet::new();
    let mut primary_rank = u8::MAX;
    let mut primary_stem = folder_name.to_string();
    let mut detected_sys = inherited_system;

    for (_, stem, ext) in &filtered {
        if seen_stems.insert(stem.to_lowercase()) {
            launch_stems.push(stem.clone());
        }
        let rank = if ext == "m3u" {
            0
        } else if OPTICAL_SHEET_EXTS.contains(&ext.as_str()) {
            1
        } else {
            2
        };
        if rank < primary_rank {
            primary_rank = rank;
            primary_stem = stem.clone();
        }
        if detected_sys.is_none() {
            detected_sys = detect_system_from_extension(ext)
                .or_else(|| detect_system_from_stem_tags(stem));
        }
    }

    if detected_sys.is_none() {
        detected_sys = detect_system_from_stem_tags(folder_name)
            .or_else(|| detect_system_from_stem_tags(&primary_stem));
    }

    Ok(Some((
        UploadPayload::AtomicFolder {
            src_dir: dir.to_path_buf(),
            folder_name: folder_name.to_string(),
            files,
            launch_stems,
            convertible_sheets,
            primary_stem,
            total_bytes,
        },
        detected_sys,
    )))
}

/// Inspects direct subdirectories of `games_base` so existing MiSTer console folders
/// (e.g. `MegaDrive` or `GAMEBOY`) are reused rather than creating duplicate folders.
pub async fn resolve_existing_system_dirs(
    storage: &StorageClient,
    games_base: &str,
) -> HashMap<SystemId, String> {
    let mut map = HashMap::new();
    let Ok(subdirs) = storage.list_subdirs(games_base).await else {
        return map;
    };
    for dir in subdirs {
        if let Some(sys) = detect_system_from_folder_name(&dir) {
            map.entry(sys).or_insert(dir);
        }
    }
    map
}

/// Resolves the destination folder name for `sys` inside MiSTer's `games/` root.
pub fn target_folder_name(sys: SystemId, existing_dirs: &HashMap<SystemId, String>) -> String {
    existing_dirs
        .get(&sys)
        .cloned()
        .unwrap_or_else(|| sys.mister_folder_name().to_string())
}

/// Builds a full upload plan with destination paths and matched covers.
pub fn build_upload_plan(
    discovered: Vec<(UploadPayload, Option<SystemId>)>,
    covers: &[CoverImage],
    games_base: &str,
    is_sftp: bool,
    existing_dirs: &HashMap<SystemId, String>,
    options: &UploadOptions,
) -> Vec<UploadItem> {
    let index = CoverIndex::new(covers);
    let mut items = Vec::with_capacity(discovered.len());

    for (payload, detected_system) in discovered {
        let target_system = detected_system.or(options.default_system);
        let convert_to_chd = options.convert_bin_cue_to_chd && payload.can_convert_to_chd();
        let mut item = UploadItem {
            payload,
            detected_system,
            target_system,
            target_system_dir: String::new(),
            target_game_path: String::new(),
            target_media_dir: String::new(),
            convert_to_chd,
            convert_to_chd_manual: false,
            selected_cover_idx: None,
            cover_score: 0.0,
            cover_match_kind: None,
            cover_candidates: Vec::new(),
            upload_cover: false,
            selected: target_system.is_some(),
            status: if target_system.is_some() {
                UploadStatus::Ready
            } else {
                UploadStatus::NeedsSystem
            },
        };
        refresh_upload_item_with_index(
            &mut item,
            &index,
            games_base,
            is_sftp,
            existing_dirs,
            options,
        );
        items.push(item);
    }

    items
}

/// Recomputes destination paths and cover matching for a single `UploadItem`
/// (for instance after changing its target console or per-item CHD toggle in the GUI).
pub fn recompute_upload_item(
    item: &mut UploadItem,
    covers: &[CoverImage],
    games_base: &str,
    is_sftp: bool,
    existing_dirs: &HashMap<SystemId, String>,
    options: &UploadOptions,
) {
    let index = CoverIndex::new(covers);
    refresh_upload_item_with_index(item, &index, games_base, is_sftp, existing_dirs, options);
}

/// Re-evaluates routing, CHD conversion paths, and cover matching across the whole queue.
pub fn rematch_upload_items(
    items: &mut [UploadItem],
    covers: &[CoverImage],
    games_base: &str,
    is_sftp: bool,
    existing_dirs: &HashMap<SystemId, String>,
    options: &UploadOptions,
) {
    let index = CoverIndex::new(covers);
    for item in items {
        if item.target_system.is_none()
            && item.detected_system.is_none()
            && let Some(def_sys) = options.default_system
        {
            item.target_system = Some(def_sys);
        }
        if !item.convert_to_chd_manual {
            item.convert_to_chd =
                options.convert_bin_cue_to_chd && item.payload.can_convert_to_chd();
        }
        refresh_upload_item_with_index(item, &index, games_base, is_sftp, existing_dirs, options);
    }
}

fn single_sheet_stem(item: &UploadItem) -> Option<String> {
    let sheets = item.payload.convertible_sheets();
    if sheets.len() == 1 {
        let fname = sheets[0].rsplit('/').next().unwrap_or(&sheets[0]);
        let (stem, _) = split_stem_ext(fname);
        Some(stem.to_string())
    } else {
        None
    }
}

fn refresh_upload_item_with_index(
    item: &mut UploadItem,
    index: &CoverIndex,
    games_base: &str,
    is_sftp: bool,
    existing_dirs: &HashMap<SystemId, String>,
    options: &UploadOptions,
) {
    if let Some(sys) = item.target_system {
        let sys_folder = target_folder_name(sys, existing_dirs);
        item.target_system_dir = join_storage_path(is_sftp, games_base, &sys_folder);

        let (game_path, media_dir) = if item.convert_to_chd && item.payload.can_convert_to_chd() {
            if let Some(sheet_stem) = single_sheet_stem(item) {
                let chd_name = format!("{sheet_stem}.chd");
                compute_single_file_paths(
                    &item.target_system_dir,
                    is_sftp,
                    options,
                    &chd_name,
                    &sheet_stem,
                    true,
                )
            } else {
                let folder_name = item.payload.display_name().to_string();
                compute_folder_paths(&item.target_system_dir, is_sftp, options, &folder_name)
            }
        } else {
            let is_disc_single = item.payload.is_disc_file();
            match &item.payload {
                UploadPayload::SingleFile {
                    file_name, stem, ..
                } => compute_single_file_paths(
                    &item.target_system_dir,
                    is_sftp,
                    options,
                    file_name,
                    stem,
                    is_disc_single,
                ),
                UploadPayload::AtomicFolder { folder_name, .. } => compute_folder_paths(
                    &item.target_system_dir,
                    is_sftp,
                    options,
                    folder_name,
                ),
                UploadPayload::Archive {
                    single_inner_file: Some((rel_path, ext)),
                    primary_stem,
                    ..
                } => {
                    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
                    let is_disc = OPTICAL_DISC_EXTS.contains(&ext.as_str()) || ext == "m3u";
                    compute_single_file_paths(
                        &item.target_system_dir,
                        is_sftp,
                        options,
                        file_name,
                        primary_stem,
                        is_disc,
                    )
                }
                UploadPayload::Archive { folder_name, .. } => compute_folder_paths(
                    &item.target_system_dir,
                    is_sftp,
                    options,
                    folder_name,
                ),
            }
        };
        item.target_game_path = game_path;
        item.target_media_dir = media_dir;

        if !matches!(item.status, UploadStatus::Done | UploadStatus::Uploading) {
            item.status = UploadStatus::Ready;
            item.selected = true;
        }
    } else {
        item.target_system_dir.clear();
        item.target_game_path.clear();
        item.target_media_dir.clear();
        if !matches!(item.status, UploadStatus::Done | UploadStatus::Uploading) {
            item.status = UploadStatus::NeedsSystem;
            item.selected = false;
        }
    }

    let candidates = index.match_game(
        item.payload.primary_stem(),
        item.target_system,
        options.strict_system_match,
    );
    item.cover_candidates = candidates;

    if item.cover_match_kind == Some(MatchKind::Manual) && item.selected_cover_idx.is_some() {
        return;
    }

    if let Some(best) = item
        .cover_candidates
        .first()
        .filter(|c| c.score >= options.cover_threshold)
    {
        item.selected_cover_idx = Some(best.cover_idx);
        item.cover_score = best.score;
        item.cover_match_kind = Some(best.kind);
        item.upload_cover = true;
    } else {
        item.selected_cover_idx = None;
        item.cover_score = item
            .cover_candidates
            .first()
            .map(|c| c.score)
            .unwrap_or(0.0);
        item.cover_match_kind = item.cover_candidates.first().map(|c| c.kind);
        item.upload_cover = false;
    }
}

fn compute_single_file_paths(
    target_system_dir: &str,
    is_sftp: bool,
    options: &UploadOptions,
    file_name: &str,
    stem: &str,
    is_disc: bool,
) -> (String, String) {
    if options.wrap_disc_in_folder && is_disc {
        let game_dir = join_storage_path(is_sftp, target_system_dir, stem);
        let game_path = join_storage_path(is_sftp, &game_dir, file_name);
        let media_dir = match options.media_placement {
            MediaPlacement::RootMedia => join_storage_path(is_sftp, target_system_dir, "media"),
            MediaPlacement::PerSubfolderMedia => join_storage_path(is_sftp, &game_dir, "media"),
        };
        (game_path, media_dir)
    } else {
        let game_path = join_storage_path(is_sftp, target_system_dir, file_name);
        let media_dir = join_storage_path(is_sftp, target_system_dir, "media");
        (game_path, media_dir)
    }
}

fn compute_folder_paths(
    target_system_dir: &str,
    is_sftp: bool,
    options: &UploadOptions,
    folder_name: &str,
) -> (String, String) {
    let game_path = join_storage_path(is_sftp, target_system_dir, folder_name);
    let media_dir = match options.media_placement {
        MediaPlacement::RootMedia => join_storage_path(is_sftp, target_system_dir, "media"),
        MediaPlacement::PerSubfolderMedia => join_storage_path(is_sftp, &game_path, "media"),
    };
    (game_path, media_dir)
}

/// Returns the destination cover path(s) inside `media/` for an `UploadItem`.
pub fn cover_target_paths(
    item: &UploadItem,
    covers: &[CoverImage],
    convert_to_png: bool,
    is_sftp: bool,
) -> Vec<String> {
    if item.target_media_dir.is_empty() || !item.upload_cover {
        return Vec::new();
    }
    let Some(cov_idx) = item.selected_cover_idx else {
        return Vec::new();
    };
    let ext = if convert_to_png {
        "png"
    } else {
        covers
            .get(cov_idx)
            .map(|c| c.extension.as_str())
            .unwrap_or("png")
    };
    item.payload
        .launch_stems()
        .into_iter()
        .map(|stem| join_storage_path(is_sftp, &item.target_media_dir, &format!("{stem}.{ext}")))
        .collect()
}

fn create_work_temp_dir(base: Option<&Path>) -> Result<TempDir> {
    if let Some(dir) = base
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).with_context(|| {
            format!(
                "No se pudo crear carpeta temporal personalizada: {}",
                dir.display()
            )
        })?;
        return TempDir::new_in(dir).with_context(|| {
            format!(
                "No se pudo crear subcarpeta temporal en: {}",
                dir.display()
            )
        });
    }
    tempfile::tempdir().context("No se pudo crear carpeta temporal del sistema")
}

/// Streams all selected games (files, atomic folders, compressed archives, and optional
/// BIN/CUE -> CHD conversions) plus their linked covers to MiSTer storage.
pub async fn execute_upload<F>(
    storage: &StorageClient,
    items: &[UploadItem],
    covers: &[CoverImage],
    options: &UploadOptions,
    cancel: &AtomicBool,
    on_event: F,
) -> Result<UploadSummary>
where
    F: Fn(UploadEvent) + Send + Sync,
{
    let mut summary = UploadSummary {
        total_items: items.len(),
        ..Default::default()
    };

    let mut work_indices = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if item.selected && item.target_system.is_some() {
            work_indices.push(idx);
        } else {
            summary.skipped += 1;
        }
    }

    let total = work_indices.len();
    if total == 0 {
        return Ok(summary);
    }

    let is_sftp = storage.is_sftp();
    let mut ensured_dirs: HashSet<String> = HashSet::new();
    let mut seen_cover_targets: HashSet<String> = HashSet::new();
    let mut extracted_archives: HashMap<PathBuf, TempDir> = HashMap::new();

    for (pos, item_idx) in work_indices.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Operación cancelada por el usuario.");
        }

        let item = &items[item_idx];
        let label = item.payload.display_name().to_string();
        on_event(UploadEvent::ItemStarting {
            item_idx,
            current: pos + 1,
            total,
            label: label.clone(),
        });

        if options.dry_run {
            summary.games_uploaded += 1;
            summary.files_transferred += item.payload.effective_output_file_count(item.convert_to_chd);
            summary.bytes_transferred += item.payload.total_bytes();
            if item.convert_to_chd && item.payload.can_convert_to_chd() {
                summary.chds_created += item.payload.convertible_sheets().len();
            }
            if item.upload_cover && item.selected_cover_idx.is_some() {
                for cov_dst in
                    cover_target_paths(item, covers, options.convert_covers_to_png, is_sftp)
                {
                    if seen_cover_targets.insert(cov_dst.to_lowercase()) {
                        summary.covers_uploaded += 1;
                    }
                }
            }
            on_event(UploadEvent::ItemFinished {
                item_idx,
                status: UploadStatus::Done,
            });
            continue;
        }

        let item_result = upload_single_item(
            storage,
            item,
            covers,
            options,
            is_sftp,
            cancel,
            &mut ensured_dirs,
            &mut seen_cover_targets,
            &mut extracted_archives,
            &on_event,
        )
        .await;

        match item_result {
            Ok(outcome) => {
                summary.files_transferred += outcome.files_transferred;
                summary.bytes_transferred += outcome.bytes_transferred;
                summary.covers_uploaded += outcome.covers_uploaded;
                summary.archives_extracted += usize::from(outcome.extracted_archive);
                summary.chds_created += outcome.chds_created;
                if outcome.skipped_entire_game && outcome.covers_uploaded == 0 {
                    summary.skipped += 1;
                    on_event(UploadEvent::ItemFinished {
                        item_idx,
                        status: UploadStatus::SkippedExists,
                    });
                } else {
                    summary.games_uploaded += 1;
                    on_event(UploadEvent::ItemFinished {
                        item_idx,
                        status: UploadStatus::Done,
                    });
                }
            }
            Err(err) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(err);
                }
                let msg = format!("{err:#}");
                summary.failed += 1;
                summary.errors.push((label, msg.clone()));
                on_event(UploadEvent::ItemFinished {
                    item_idx,
                    status: UploadStatus::Failed(msg),
                });
            }
        }
    }

    Ok(summary)
}

struct ItemOutcome {
    files_transferred: usize,
    bytes_transferred: u64,
    covers_uploaded: usize,
    extracted_archive: bool,
    chds_created: usize,
    skipped_entire_game: bool,
}

#[allow(clippy::too_many_arguments)]
async fn upload_single_item<F>(
    storage: &StorageClient,
    item: &UploadItem,
    covers: &[CoverImage],
    options: &UploadOptions,
    is_sftp: bool,
    cancel: &AtomicBool,
    ensured_dirs: &mut HashSet<String>,
    seen_cover_targets: &mut HashSet<String>,
    extracted_archives: &mut HashMap<PathBuf, TempDir>,
    on_event: &F,
) -> Result<ItemOutcome>
where
    F: Fn(UploadEvent) + Send + Sync,
{
    if ensured_dirs.insert(item.target_system_dir.clone()) {
        storage.ensure_dir(&item.target_system_dir).await?;
    }

    let mut files_transferred = 0usize;
    let mut bytes_transferred = 0u64;
    let mut skipped_files = 0usize;
    let mut extracted_archive = false;
    let mut chds_created = 0usize;
    let expected_files = item.payload.effective_output_file_count(item.convert_to_chd);

    let is_single_output_target = matches!(item.payload, UploadPayload::SingleFile { .. })
        || matches!(
            item.payload,
            UploadPayload::Archive {
                single_inner_file: Some(_),
                ..
            }
        )
        || (item.convert_to_chd && single_sheet_stem(item).is_some());

    // Early overwrite check before wasting work extracting archives or compressing to CHD
    let skip_game_payload = is_single_output_target
        && !options.overwrite_games
        && storage.path_exists(&item.target_game_path).await?;

    if skip_game_payload {
        skipped_files = expected_files;
    } else {
        // Resolve source files (extracting archives on demand)
        let extracted_root: Option<&Path> = match &item.payload {
            UploadPayload::Archive {
                archive_path,
                format,
                ..
            } => {
                let canonical = canonical_or_self(archive_path);
                if !extracted_archives.contains_key(&canonical) {
                    let temp_dir = create_work_temp_dir(options.temp_dir.as_deref())?;
                    let dest_path = temp_dir.path().to_path_buf();
                    let src_arch = archive_path.clone();
                    let fmt_label = format.label();
                    on_event(UploadEvent::StageProgress {
                        stage: format!("Extrayendo {fmt_label}..."),
                        done: 0,
                        total: 1,
                    });
                    tokio::task::block_in_place(|| {
                        archives::extract_archive(&src_arch, &dest_path, cancel, |done, total| {
                            on_event(UploadEvent::StageProgress {
                                stage: format!("Extrayendo {fmt_label}..."),
                                done,
                                total,
                            });
                        })
                    })?;
                    extracted_archives.insert(canonical.clone(), temp_dir);
                    extracted_archive = true;
                }
                extracted_archives.get(&canonical).map(|t| t.path())
            }
            _ => None,
        };

        if item.convert_to_chd && item.payload.can_convert_to_chd() {
            let (base_dir, rel_sheets, m3u_rel_paths, allow_in_place_cue_fix) =
                gather_chd_inputs(&item.payload, extracted_root)?;
            let chd_stage_dir = create_work_temp_dir(options.temp_dir.as_deref())?;
            let custom_chdman = options.chdman_path.clone();

            let mut files_to_upload: Vec<(PathBuf, String)> = Vec::new();
            let mut sheet_to_chd_name: HashMap<String, String> = HashMap::new();

            if rel_sheets.len() == 1 {
                let sheet_rel = &rel_sheets[0];
                let src_sheet = join_rel_path(&base_dir, sheet_rel);
                let fname = sheet_rel.rsplit('/').next().unwrap_or(sheet_rel);
                let (stem, _) = split_stem_ext(fname);
                let chd_name = format!("{stem}.chd");
                let out_chd = chd_stage_dir.path().join(&chd_name);

                on_event(UploadEvent::StageProgress {
                    stage: format!("Convirtiendo a CHD: {fname}"),
                    done: 0,
                    total: 1,
                });
                let chd_res = tokio::task::block_in_place(|| {
                    chd::convert_disc_to_chd(
                        &src_sheet,
                        &out_chd,
                        custom_chdman.as_deref(),
                        allow_in_place_cue_fix,
                        cancel,
                        |done, total| {
                            on_event(UploadEvent::StageProgress {
                                stage: format!("Convirtiendo a CHD: {fname}"),
                                done,
                                total,
                            });
                        },
                    )
                })?;
                on_event(UploadEvent::StageProgress {
                    stage: format!(
                        "CHD generado ({} · {}): {chd_name}",
                        chd_res.engine_used,
                        format_bytes(chd_res.output_bytes)
                    ),
                    done: 1,
                    total: 1,
                });
                chds_created += 1;
                files_to_upload.push((out_chd, item.target_game_path.clone()));
            } else {
                // Multi-disc game folder -> convert each sheet to `.chd` + rewrite `.m3u`
                let total_sheets = rel_sheets.len();
                for (s_idx, sheet_rel) in rel_sheets.iter().enumerate() {
                    let src_sheet = join_rel_path(&base_dir, sheet_rel);
                    let fname = sheet_rel.rsplit('/').next().unwrap_or(sheet_rel);
                    let (stem, _) = split_stem_ext(fname);
                    let chd_rel = match sheet_rel.rsplit_once('/') {
                        Some((dir_p, _)) => format!("{dir_p}/{stem}.chd"),
                        None => format!("{stem}.chd"),
                    };
                    let out_chd = join_rel_path(chd_stage_dir.path(), &chd_rel);
                    sheet_to_chd_name.insert(sheet_rel.clone(), chd_rel.clone());

                    let stage_label =
                        format!("Convirtiendo CHD ({}/{}): {fname}", s_idx + 1, total_sheets);
                    tokio::task::block_in_place(|| {
                        chd::convert_disc_to_chd(
                            &src_sheet,
                            &out_chd,
                            custom_chdman.as_deref(),
                            allow_in_place_cue_fix,
                            cancel,
                            |done, total| {
                                on_event(UploadEvent::StageProgress {
                                    stage: stage_label.clone(),
                                    done,
                                    total,
                                });
                            },
                        )
                    })?;
                    chds_created += 1;
                    let dst = join_storage_subpath(is_sftp, &item.target_game_path, &chd_rel);
                    files_to_upload.push((out_chd, dst));
                }

                for m3u_rel in m3u_rel_paths {
                    let src_m3u = join_rel_path(&base_dir, &m3u_rel);
                    let out_m3u = join_rel_path(chd_stage_dir.path(), &m3u_rel);
                    if let Some(p) = out_m3u.parent() {
                        fs::create_dir_all(p)?;
                    }
                    let rewritten = rewrite_m3u_for_chd(&src_m3u, &m3u_rel, &sheet_to_chd_name)?;
                    fs::write(&out_m3u, rewritten)?;
                    let dst = join_storage_subpath(is_sftp, &item.target_game_path, &m3u_rel);
                    files_to_upload.push((out_m3u, dst));
                }
            }

            for (local_file, dst_path) in files_to_upload {
                let parent_dir = parent_dir_for_storage(is_sftp, &dst_path);
                if ensured_dirs.insert(parent_dir.clone()) {
                    storage.ensure_dir(&parent_dir).await?;
                }
                if !options.overwrite_games && storage.path_exists(&dst_path).await? {
                    skipped_files += 1;
                    continue;
                }
                let display_f = local_file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "game.chd".to_string());
                let transferred = storage
                    .copy_local_file_streamed(&local_file, &dst_path, cancel, |sent, tot| {
                        on_event(UploadEvent::FileBytes {
                            file_name: display_f.clone(),
                            sent,
                            total: tot,
                        });
                    })
                    .await?;
                files_transferred += 1;
                bytes_transferred += transferred;
            }
        } else {
            // Normal file/folder upload (including extracted ROMs or non-converted `.bin`/`.cue`)
            match &item.payload {
                UploadPayload::SingleFile {
                    src_path,
                    file_name,
                    ..
                } => {
                    let parent_dir = parent_dir_for_storage(is_sftp, &item.target_game_path);
                    if ensured_dirs.insert(parent_dir.clone()) {
                        storage.ensure_dir(&parent_dir).await?;
                    }
                    let transferred = storage
                        .copy_local_file_streamed(
                            src_path,
                            &item.target_game_path,
                            cancel,
                            |sent, tot| {
                                on_event(UploadEvent::FileBytes {
                                    file_name: file_name.clone(),
                                    sent,
                                    total: tot,
                                });
                            },
                        )
                        .await?;
                    files_transferred += 1;
                    bytes_transferred += transferred;
                }
                UploadPayload::AtomicFolder { files, .. } => {
                    if ensured_dirs.insert(item.target_game_path.clone()) {
                        storage.ensure_dir(&item.target_game_path).await?;
                    }
                    for ff in files {
                        if cancel.load(Ordering::Relaxed) {
                            bail!("Operación cancelada por el usuario.");
                        }
                        let dst_file =
                            join_storage_subpath(is_sftp, &item.target_game_path, &ff.rel_path);
                        let parent_dir = parent_dir_for_storage(is_sftp, &dst_file);
                        if ensured_dirs.insert(parent_dir.clone()) {
                            storage.ensure_dir(&parent_dir).await?;
                        }
                        if !options.overwrite_games && storage.path_exists(&dst_file).await? {
                            skipped_files += 1;
                            continue;
                        }
                        let transferred = storage
                            .copy_local_file_streamed(&ff.src_path, &dst_file, cancel, |sent, tot| {
                                on_event(UploadEvent::FileBytes {
                                    file_name: ff.rel_path.clone(),
                                    sent,
                                    total: tot,
                                });
                            })
                            .await?;
                        files_transferred += 1;
                        bytes_transferred += transferred;
                    }
                }
                UploadPayload::Archive {
                    single_inner_file: Some((rel_path, _)),
                    ..
                } => {
                    let root = extracted_root.context("Falta carpeta extraída del archivo")?;
                    let src_file = join_rel_path(root, rel_path);
                    let fname = rel_path.rsplit('/').next().unwrap_or(rel_path);
                    let parent_dir = parent_dir_for_storage(is_sftp, &item.target_game_path);
                    if ensured_dirs.insert(parent_dir.clone()) {
                        storage.ensure_dir(&parent_dir).await?;
                    }
                    let transferred = storage
                        .copy_local_file_streamed(
                            &src_file,
                            &item.target_game_path,
                            cancel,
                            |sent, tot| {
                                on_event(UploadEvent::FileBytes {
                                    file_name: fname.to_string(),
                                    sent,
                                    total: tot,
                                });
                            },
                        )
                        .await?;
                    files_transferred += 1;
                    bytes_transferred += transferred;
                }
                UploadPayload::Archive {
                    inner_files,
                    inner_root,
                    ..
                } => {
                    let root = extracted_root.context("Falta carpeta extraída del archivo")?;
                    if ensured_dirs.insert(item.target_game_path.clone()) {
                        storage.ensure_dir(&item.target_game_path).await?;
                    }
                    for entry in inner_files {
                        if cancel.load(Ordering::Relaxed) {
                            bail!("Operación cancelada por el usuario.");
                        }
                        let src_file = join_rel_path(root, &entry.path);
                        let rel_dst = strip_inner_root(&entry.path, inner_root);
                        let dst_file =
                            join_storage_subpath(is_sftp, &item.target_game_path, rel_dst);
                        let parent_dir = parent_dir_for_storage(is_sftp, &dst_file);
                        if ensured_dirs.insert(parent_dir.clone()) {
                            storage.ensure_dir(&parent_dir).await?;
                        }
                        if !options.overwrite_games && storage.path_exists(&dst_file).await? {
                            skipped_files += 1;
                            continue;
                        }
                        let transferred = storage
                            .copy_local_file_streamed(&src_file, &dst_file, cancel, |sent, tot| {
                                on_event(UploadEvent::FileBytes {
                                    file_name: rel_dst.to_string(),
                                    sent,
                                    total: tot,
                                });
                            })
                            .await?;
                        files_transferred += 1;
                        bytes_transferred += transferred;
                    }
                }
            }
        }
    }

    let mut covers_uploaded = 0usize;
    if item.upload_cover
        && let Some(cov_idx) = item.selected_cover_idx
        && let Some(cover) = covers.get(cov_idx)
    {
        let cover_targets =
            cover_target_paths(item, covers, options.convert_covers_to_png, is_sftp);
        let mut needed_targets = Vec::new();
        for dst in cover_targets {
            if !seen_cover_targets.insert(dst.to_lowercase()) {
                continue;
            }
            if !options.overwrite_covers && storage.path_exists(&dst).await? {
                continue;
            }
            needed_targets.push(dst);
        }

        if !needed_targets.is_empty() {
            if ensured_dirs.insert(item.target_media_dir.clone()) {
                storage.ensure_dir(&item.target_media_dir).await?;
            }
            let src_cov = cover.path.clone();
            let convert = options.convert_covers_to_png;
            let (bytes, _ext) =
                tokio::task::spawn_blocking(move || imaging::prepare_cover_bytes(&src_cov, convert))
                    .await??;

            for dst in needed_targets {
                storage.write_file(&dst, &bytes).await?;
                covers_uploaded += 1;
            }
        }
    }

    let skipped_entire_game = skipped_files == expected_files && files_transferred == 0;
    Ok(ItemOutcome {
        files_transferred,
        bytes_transferred,
        covers_uploaded,
        extracted_archive,
        chds_created,
        skipped_entire_game,
    })
}

fn gather_chd_inputs(
    payload: &UploadPayload,
    extracted_root: Option<&Path>,
) -> Result<(PathBuf, Vec<String>, Vec<String>, bool)> {
    match payload {
        UploadPayload::AtomicFolder {
            src_dir,
            files,
            convertible_sheets,
            ..
        } => {
            let m3us = files
                .iter()
                .filter(|f| f.rel_path.to_ascii_lowercase().ends_with(".m3u"))
                .map(|f| f.rel_path.clone())
                .collect();
            Ok((src_dir.clone(), convertible_sheets.clone(), m3us, false))
        }
        UploadPayload::Archive {
            inner_files,
            inner_root,
            convertible_sheets,
            ..
        } => {
            let root = extracted_root.context("Falta carpeta extraída del archivo")?;
            let base = if inner_root.is_empty() {
                root.to_path_buf()
            } else {
                join_rel_path(root, inner_root)
            };
            let rel_sheets: Vec<String> = convertible_sheets
                .iter()
                .map(|s| strip_inner_root(s, inner_root).to_string())
                .collect();
            let m3us: Vec<String> = inner_files
                .iter()
                .filter(|f| f.path.to_ascii_lowercase().ends_with(".m3u"))
                .map(|f| strip_inner_root(&f.path, inner_root).to_string())
                .collect();
            Ok((base, rel_sheets, m3us, true))
        }
        UploadPayload::SingleFile { .. } => {
            bail!("Este elemento no contiene un disco .cue/.gdi convertible a CHD.")
        }
    }
}

fn rewrite_m3u_for_chd(
    src_m3u: &Path,
    m3u_rel: &str,
    sheet_to_chd_name: &HashMap<String, String>,
) -> Result<String> {
    let raw = fs::read_to_string(src_m3u)
        .with_context(|| format!("No se pudo leer playlist .m3u: {}", src_m3u.display()))?;
    let m3u_dir = m3u_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut lines = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            lines.push(line.to_string());
            continue;
        }
        let norm = trimmed.replace('\\', "/");
        let full_rel = if m3u_dir.is_empty() {
            norm.clone()
        } else {
            format!("{m3u_dir}/{norm}")
        };
        if let Some(chd_rel) = sheet_to_chd_name.get(&full_rel) {
            let rel_from_m3u = if m3u_dir.is_empty() {
                chd_rel.as_str()
            } else {
                chd_rel
                    .strip_prefix(&format!("{m3u_dir}/"))
                    .unwrap_or(chd_rel.as_str())
            };
            lines.push(rel_from_m3u.to_string());
        } else {
            let (_, ext) = split_stem_ext(&norm);
            if matches!(ext.as_str(), "cue" | "gdi" | "ccd" | "mds") {
                let (stem_part, _) = split_stem_ext(&norm);
                lines.push(format!("{stem_part}.chd"));
            } else {
                lines.push(line.to_string());
            }
        }
    }
    Ok(lines.join("\n"))
}

fn strip_inner_root<'a>(path: &'a str, inner_root: &str) -> &'a str {
    if inner_root.is_empty() {
        return path;
    }
    let prefix = format!("{inner_root}/");
    path.strip_prefix(&prefix).unwrap_or(path)
}

fn join_rel_path(base: &Path, rel_posix: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for seg in rel_posix.split('/').filter(|s| !s.is_empty()) {
        out.push(seg);
    }
    out
}

fn join_storage_subpath(is_sftp: bool, base: &str, rel_posix: &str) -> String {
    if is_sftp {
        posix_join(base, rel_posix)
    } else {
        let mut pb = PathBuf::from(base);
        for seg in rel_posix.split('/').filter(|s| !s.is_empty()) {
            pb.push(seg);
        }
        pb.to_string_lossy().to_string()
    }
}

fn parent_dir_for_storage(is_sftp: bool, path: &str) -> String {
    if is_sftp {
        posix_parent(path)
    } else {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// Aggregates total byte size of items currently marked `selected`.
pub fn selected_total_bytes(items: &[UploadItem]) -> u64 {
    items
        .iter()
        .filter(|i| i.selected && i.target_system.is_some())
        .map(|i| i.payload.total_bytes())
        .sum()
}

/// Counts items in the queue belonging to each target console.
pub fn summarize_queue_systems(items: &[UploadItem]) -> Vec<(&'static str, usize)> {
    let mut counts: BTreeSet<&'static str> = BTreeSet::new();
    let mut map: HashMap<&'static str, usize> = HashMap::new();
    for item in items {
        if let Some(sys) = item.target_system {
            let lbl = sys.short_label();
            counts.insert(lbl);
            *map.entry(lbl).or_default() += 1;
        }
    }
    counts.into_iter().map(|k| (k, map[k])).collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chd_rs::{Chd, VerifyOutcome};
    use image::{DynamicImage, ImageFormat, RgbImage};
    use tempfile::tempdir;

    use super::*;
    use crate::scanner::scan_local_covers;

    fn write_test_jpg(path: &Path) {
        let rgb = RgbImage::from_fn(8, 8, |_, _| image::Rgb([220, 40, 40]));
        DynamicImage::ImageRgb8(rgb)
            .save_with_format(path, ImageFormat::Jpeg)
            .unwrap();
    }

    fn write_test_png(path: &Path) {
        let rgb = RgbImage::from_fn(8, 8, |_, _| image::Rgb([30, 180, 90]));
        DynamicImage::ImageRgb8(rgb)
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    fn valid_cd_track(frames: usize, seed: u64) -> Vec<u8> {
        let mut data = vec![0u8; frames * 2352];
        let mut state = seed | 1;
        for (idx, b) in data.iter_mut().enumerate() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = if (idx / 2352) % 3 == 2 {
                (state >> 16) as u8
            } else {
                ((idx % 241) as u8) ^ (seed as u8)
            };
        }
        data
    }

    #[tokio::test]
    async fn bulk_upload_routes_mixed_roms_and_bin_cue_folders_with_covers() {
        let root = tempdir().unwrap();
        let staging = root.path().join("staging");
        let psx_game_folder = staging
            .join("PSX")
            .join("Castlevania - Symphony of the Night (USA)");
        let batch_psx_dir = staging.join("Batch").join("PSX");
        let covers_root = root.path().join("Covers");
        let mister_games = root.path().join("mister_games");

        fs::create_dir_all(&psx_game_folder).unwrap();
        fs::create_dir_all(&batch_psx_dir).unwrap();
        fs::create_dir_all(covers_root.join("Nintendo - Game Boy Advance")).unwrap();
        fs::create_dir_all(covers_root.join("Nintendo - Super Nintendo Entertainment System"))
            .unwrap();
        fs::create_dir_all(covers_root.join("Sony - PlayStation")).unwrap();
        fs::create_dir_all(mister_games.join("MegaDrive")).unwrap();

        let gba_rom = staging.join("Metroid Fusion (USA).gba");
        let snes_rom = staging.join("Chrono Trigger (USA).sfc");
        let md_rom = staging.join("Sonic 3 (USA).md");
        fs::write(&gba_rom, b"gba-data").unwrap();
        fs::write(&snes_rom, b"snes-data").unwrap();
        fs::write(&md_rom, b"md-data").unwrap();

        fs::write(
            psx_game_folder.join("Castlevania - Symphony of the Night (USA).cue"),
            b"FILE \"Castlevania - Symphony of the Night (USA) (Track 1).bin\" BINARY",
        )
        .unwrap();
        fs::write(
            psx_game_folder.join("Castlevania - Symphony of the Night (USA) (Track 1).bin"),
            b"track1-bytes",
        )
        .unwrap();
        fs::write(
            psx_game_folder.join("Castlevania - Symphony of the Night (USA) (Track 2).bin"),
            b"track2-bytes",
        )
        .unwrap();

        fs::write(batch_psx_dir.join("Crash Bandicoot (USA).chd"), b"crash-chd").unwrap();

        write_test_png(
            &covers_root
                .join("Nintendo - Game Boy Advance")
                .join("Metroid Fusion (USA).png"),
        );
        write_test_png(
            &covers_root
                .join("Nintendo - Super Nintendo Entertainment System")
                .join("Chrono Trigger (USA).png"),
        );
        write_test_jpg(
            &covers_root
                .join("Sony - PlayStation")
                .join("Castlevania _ Symphony of the Night (USA).jpg"),
        );
        write_test_png(
            &covers_root
                .join("Sony - PlayStation")
                .join("Crash Bandicoot (USA).png"),
        );

        let sources = vec![
            gba_rom,
            snes_rom,
            md_rom,
            psx_game_folder,
            staging.join("Batch"),
        ];
        let discovered = discover_upload_sources(&sources).unwrap();
        assert_eq!(discovered.len(), 5);

        let covers = scan_local_covers(&covers_root, true).unwrap();
        let storage = StorageClient::Local;
        let mister_base = mister_games.to_string_lossy().to_string();
        let existing_dirs = resolve_existing_system_dirs(&storage, &mister_base).await;
        assert_eq!(
            existing_dirs.get(&SystemId::Genesis).map(String::as_str),
            Some("MegaDrive")
        );

        let dry_opts = UploadOptions {
            dry_run: true,
            ..Default::default()
        };
        let plan = build_upload_plan(
            discovered,
            &covers,
            &mister_base,
            false,
            &existing_dirs,
            &dry_opts,
        );
        assert!(plan.iter().all(|i| i.selected && i.target_system.is_some()));

        let cancel = AtomicBool::new(false);
        let dry_sum = execute_upload(&storage, &plan, &covers, &dry_opts, &cancel, |_| {})
            .await
            .unwrap();
        assert_eq!(dry_sum.games_uploaded, 5);
        assert_eq!(dry_sum.covers_uploaded, 4);
        assert!(!mister_games.join("GBA").exists());

        let real_opts = UploadOptions::default();
        let sum = execute_upload(&storage, &plan, &covers, &real_opts, &cancel, |_| {})
            .await
            .unwrap();
        assert_eq!(sum.games_uploaded, 5);
        assert_eq!(sum.files_transferred, 7);
        assert_eq!(sum.covers_uploaded, 4);
        assert_eq!(sum.failed, 0);

        assert!(mister_games.join("GBA/Metroid Fusion (USA).gba").is_file());
        assert!(mister_games.join("SNES/Chrono Trigger (USA).sfc").is_file());
        assert!(mister_games.join("MegaDrive/Sonic 3 (USA).md").is_file());
        assert!(!mister_games.join("Genesis").exists());
        assert!(
            mister_games
                .join("PSX/Castlevania - Symphony of the Night (USA)/Castlevania - Symphony of the Night (USA).cue")
                .is_file()
        );
        assert!(
            mister_games
                .join("PSX/Castlevania - Symphony of the Night (USA)/Castlevania - Symphony of the Night (USA) (Track 1).bin")
                .is_file()
        );
        assert!(
            mister_games
                .join("PSX/Castlevania - Symphony of the Night (USA)/Castlevania - Symphony of the Night (USA) (Track 2).bin")
                .is_file()
        );
        assert!(mister_games.join("PSX/Crash Bandicoot (USA).chd").is_file());

        let sotn_png = mister_games.join("PSX/media/Castlevania - Symphony of the Night (USA).png");
        assert!(sotn_png.is_file());
        assert!(fs::read(&sotn_png).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(
            !mister_games
                .join("PSX/media/Castlevania - Symphony of the Night (USA) (Track 1).png")
                .exists()
        );

        let second_sum = execute_upload(&storage, &plan, &covers, &real_opts, &cancel, |_| {})
            .await
            .unwrap();
        assert_eq!(second_sum.files_transferred, 0);
        assert_eq!(second_sum.covers_uploaded, 0);
        assert_eq!(second_sum.skipped, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn converts_bin_cue_folder_to_verified_chd_with_cover() {
        let root = tempdir().unwrap();
        let psx_folder = root
            .path()
            .join("PSX")
            .join("Ridge Racer (USA)");
        let covers_root = root.path().join("Covers");
        let mister_games = root.path().join("mister_games");
        fs::create_dir_all(&psx_folder).unwrap();
        fs::create_dir_all(covers_root.join("PSX")).unwrap();
        fs::create_dir_all(&mister_games).unwrap();

        fs::write(
            psx_folder.join("Ridge Racer (USA).cue"),
            concat!(
                "FILE \"Ridge Racer (USA) (Track 1).bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
            ),
        )
        .unwrap();
        fs::write(
            psx_folder.join("Ridge Racer (USA) (Track 1).bin"),
            valid_cd_track(120, 9),
        )
        .unwrap();
        write_test_jpg(&covers_root.join("PSX").join("Ridge Racer (USA).jpg"));

        let discovered = discover_upload_sources(&[psx_folder]).unwrap();
        assert_eq!(discovered.len(), 1);
        assert!(discovered[0].0.can_convert_to_chd());

        let covers = scan_local_covers(&covers_root, true).unwrap();
        let opts = UploadOptions {
            convert_bin_cue_to_chd: true,
            ..Default::default()
        };
        let mister_base = mister_games.to_string_lossy().to_string();
        let plan = build_upload_plan(discovered, &covers, &mister_base, false, &HashMap::new(), &opts);
        assert!(plan[0].convert_to_chd);
        assert!(plan[0].target_game_path.ends_with("Ridge Racer (USA).chd"));

        let storage = StorageClient::Local;
        let cancel = AtomicBool::new(false);
        let sum = execute_upload(&storage, &plan, &covers, &opts, &cancel, |_| {})
            .await
            .unwrap();
        assert_eq!(sum.failed, 0, "errors: {:?}", sum.errors);
        assert_eq!(sum.games_uploaded, 1);
        assert_eq!(sum.chds_created, 1);
        assert_eq!(sum.covers_uploaded, 1);

        let dst_chd = mister_games.join("PSX").join("Ridge Racer (USA).chd");
        assert!(dst_chd.is_file());
        assert!(!mister_games.join("PSX").join("Ridge Racer (USA)").exists());
        assert!(mister_games.join("PSX/media/Ridge Racer (USA).png").is_file());

        let mut chd = Chd::open(&dst_chd).unwrap();
        assert_eq!(chd.verify().unwrap(), VerifyOutcome::Ok);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inspects_and_extracts_7z_bin_cue_to_chd_and_zip_rom_with_covers() {
        let root = tempdir().unwrap();
        let archive_stage_7z = root.path().join("stage_7z");
        let psx_inner = archive_stage_7z.join("Tekken 3 (USA)");
        let covers_root = root.path().join("Covers");
        let mister_games = root.path().join("mister_games");
        fs::create_dir_all(&psx_inner).unwrap();
        fs::create_dir_all(covers_root.join("PSX")).unwrap();
        fs::create_dir_all(covers_root.join("GBA")).unwrap();
        fs::create_dir_all(&mister_games).unwrap();

        fs::write(
            psx_inner.join("Tekken 3 (USA).cue"),
            concat!(
                "FILE \"Tekken 3 (USA) (Track 1).bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
            ),
        )
        .unwrap();
        fs::write(
            psx_inner.join("Tekken 3 (USA) (Track 1).bin"),
            valid_cd_track(120, 77),
        )
        .unwrap();

        let downloads = root.path().join("Downloads");
        fs::create_dir_all(downloads.join("PSX")).unwrap();
        let psx_7z = downloads.join("PSX").join("Tekken 3 (USA).7z");
        sevenz_rust2::compress_to_path(&archive_stage_7z, &psx_7z).unwrap();

        let gba_zip = downloads.join("Golden Sun (USA).zip");
        {
            let f = fs::File::create(&gba_zip).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file(
                "Golden Sun (USA).gba",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zw.write_all(b"GBA-ROM-DATA").unwrap();
            zw.finish().unwrap();
        }

        write_test_jpg(&covers_root.join("PSX").join("Tekken 3 (USA).jpg"));
        write_test_png(&covers_root.join("GBA").join("Golden Sun (USA).png"));

        let discovered = discover_upload_sources(&[psx_7z, gba_zip]).unwrap();
        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered[0].1, Some(SystemId::Psx));
        assert_eq!(discovered[1].1, Some(SystemId::Gba));

        let covers = scan_local_covers(&covers_root, true).unwrap();
        let opts = UploadOptions {
            convert_bin_cue_to_chd: true,
            ..Default::default()
        };
        let mister_base = mister_games.to_string_lossy().to_string();
        let plan = build_upload_plan(discovered, &covers, &mister_base, false, &HashMap::new(), &opts);

        let storage = StorageClient::Local;
        let cancel = AtomicBool::new(false);
        let sum = execute_upload(&storage, &plan, &covers, &opts, &cancel, |_| {})
            .await
            .unwrap();

        assert_eq!(sum.failed, 0, "errors: {:?}", sum.errors);
        assert_eq!(sum.games_uploaded, 2);
        assert_eq!(sum.archives_extracted, 2);
        assert_eq!(sum.chds_created, 1);
        assert_eq!(sum.covers_uploaded, 2);

        let tekken_chd = mister_games.join("PSX/Tekken 3 (USA).chd");
        assert!(tekken_chd.is_file());
        let mut opened = Chd::open(&tekken_chd).unwrap();
        assert_eq!(opened.verify().unwrap(), VerifyOutcome::Ok);

        assert!(mister_games.join("GBA/Golden Sun (USA).gba").is_file());
        assert!(mister_games.join("PSX/media/Tekken 3 (USA).png").is_file());
        assert!(mister_games.join("GBA/media/Golden Sun (USA).png").is_file());
    }

    #[test]
    fn ambiguous_disc_folder_marks_needs_system_until_default_applied() {
        let root = tempdir().unwrap();
        let loose_cue_dir = root.path().join("Descargas").join("Policenauts (Japan)");
        fs::create_dir_all(&loose_cue_dir).unwrap();
        fs::write(loose_cue_dir.join("Policenauts (Japan).cue"), b"FILE").unwrap();
        fs::write(loose_cue_dir.join("Policenauts (Japan).bin"), b"BIN").unwrap();

        let discovered = discover_upload_sources(&[loose_cue_dir]).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].1, None);

        let mut plan = build_upload_plan(
            discovered,
            &[],
            "/media/fat/games",
            true,
            &HashMap::new(),
            &UploadOptions::default(),
        );
        assert_eq!(plan[0].status, UploadStatus::NeedsSystem);
        assert!(!plan[0].selected);

        let opts_with_sys = UploadOptions {
            default_system: Some(SystemId::Psx),
            ..Default::default()
        };
        rematch_upload_items(
            &mut plan,
            &[],
            "/media/fat/games",
            true,
            &HashMap::new(),
            &opts_with_sys,
        );
        assert_eq!(plan[0].status, UploadStatus::Ready);
        assert!(plan[0].selected);
        assert_eq!(
            plan[0].target_game_path,
            "/media/fat/games/PSX/Policenauts (Japan)"
        );
    }
}
