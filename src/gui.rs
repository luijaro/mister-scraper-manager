use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use eframe::egui;
use rfd::FileDialog;

use crate::archives;
use crate::chd;
use crate::config::{AppConfig, MediaPlacement, SftpAuthMode, StorageMode};
use crate::discover::{self, DiscoverOptions, DiscoveredMister};
use crate::duplicates::{self, DeleteSummary, DuplicateGame, DuplicateGroup};
use crate::imaging;
use crate::models::{AnalysisResult, CoverImage, MatchKind, MatchStatus, SyncSummary};
use crate::scanner;
use crate::storage::{StorageClient, posix_join, posix_normalize, posix_parent};
use crate::sync::{self, ApplyEvent};
use crate::systems::SystemId;
use crate::uploader::{
    self, UploadEvent, UploadItem, UploadOptions, UploadPayload, UploadStatus, UploadSummary,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppTab {
    CoverScraper,
    BulkUploader,
    Duplicates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyTask {
    None,
    TestingConnection,
    Discovering,
    BrowsingRemote,
    Analyzing,
    Applying,
    StagingUpload,
    Uploading,
    ScanningDuplicates,
    DeletingDuplicates,
    DeletingBootDemos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterTab {
    All,
    Ready,
    Doubtful,
    Unmatched,
    AlreadyExists,
}

struct StagedUploadData {
    discovered: Vec<(UploadPayload, Option<SystemId>)>,
    covers: Option<Vec<CoverImage>>,
    existing_dirs: Option<HashMap<SystemId, String>>,
    games_base: String,
    is_sftp: bool,
}

enum WorkerMsg {
    Progress {
        current: Option<(usize, usize)>,
        message: String,
    },
    ConnectionTested(std::result::Result<String, String>),
    DiscoveryProgress {
        scanned: usize,
        total: usize,
        found: usize,
    },
    DiscoveryFinished(std::result::Result<Vec<DiscoveredMister>, String>),
    DuplicatesFound {
        result: std::result::Result<(Vec<DuplicateGroup>, Vec<DuplicateGame>, String, bool), String>,
    },
    DuplicatesDeleted(std::result::Result<DeleteSummary, String>),
    BootDemosDeleted(std::result::Result<DeleteSummary, String>),
    RemoteDirsLoaded {
        path: String,
        result: std::result::Result<Vec<String>, String>,
    },
    AnalysisFinished(std::result::Result<AnalysisResult, String>),
    ApplyItemFinished {
        item_idx: usize,
        result: std::result::Result<(), String>,
    },
    ApplyFinished(std::result::Result<SyncSummary, String>),
    UploadStaged(std::result::Result<StagedUploadData, String>),
    UploadItemStarting {
        item_idx: usize,
        current: usize,
        total: usize,
        label: String,
    },
    UploadStageProgress {
        stage: String,
        done: u64,
        total: u64,
        current: usize,
        total_items: usize,
    },
    UploadFileBytes {
        file_name: String,
        sent: u64,
        total: u64,
        current: usize,
        total_items: usize,
    },
    UploadItemFinished {
        item_idx: usize,
        status: UploadStatus,
    },
    UploadFinished(std::result::Result<UploadSummary, String>),
}

/// Launches the desktop GUI application.
pub fn run_gui() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([980.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "MiSTer Cover Scraper & Bulk Uploader (ConsoleMode)",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("Error en interfaz gráfica: {e}"))?;

    Ok(())
}

struct GuiApp {
    config: AppConfig,
    runtime: tokio::runtime::Runtime,
    tx: mpsc::Sender<WorkerMsg>,
    rx: mpsc::Receiver<WorkerMsg>,
    cancel_flag: Arc<AtomicBool>,
    busy: BusyTask,
    active_tab: AppTab,

    status_text: String,
    status_is_error: bool,
    progress_fraction: Option<f32>,
    show_password: bool,

    // Cover scraper state
    analysis: Option<AnalysisResult>,
    selected_item_idx: Option<usize>,
    filter_tab: FilterTab,
    search_query: String,
    manual_cover_search: String,

    // Bulk uploader state
    upload_items: Vec<UploadItem>,
    upload_covers: Vec<CoverImage>,
    upload_covers_loaded_from: Option<(String, bool)>,
    upload_existing_dirs: HashMap<SystemId, String>,
    upload_selected_idx: Option<usize>,
    upload_default_system: Option<SystemId>,
    upload_overwrite_covers: bool,
    upload_manual_cover_search: String,
    detected_archive_tools: archives::ExternalArchiveTools,
    detected_chd_tools: chd::ChdToolsStatus,

    thumbnails: HashMap<PathBuf, std::result::Result<egui::TextureHandle, String>>,

    // Remote SFTP folder browser modal state
    browser_open: bool,
    browser_path: String,
    browser_dirs: Vec<String>,
    browser_error: Option<String>,

    // MiSTer network discovery modal state
    discovery_open: bool,
    discovery_results: Vec<DiscoveredMister>,
    discovery_error: Option<String>,
    discovery_scanned: (usize, usize),

    // Duplicate games state
    duplicate_groups: Vec<DuplicateGroup>,
    duplicate_root: String,
    duplicate_is_sftp: bool,
    duplicate_keep: HashMap<usize, usize>,
    duplicate_pending_paths: Vec<String>,
    duplicate_confirm_open: bool,

    // Cosmetic mister-boot / mister-demo files state
    boot_demo_files: Vec<DuplicateGame>,
    boot_demo_selected: HashSet<String>,
    boot_demo_root: String,
    boot_demo_is_sftp: bool,
    boot_demo_pending_paths: Vec<String>,
    boot_demo_confirm_open: bool,
    boot_demo_prompt_from_scraper: bool,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");
        let (tx, rx) = mpsc::channel();
        let config = AppConfig::load();
        let detected_archive_tools = archives::detect_external_archive_tools();
        let custom_chdman = (!config.upload_chdman_path.trim().is_empty())
            .then(|| PathBuf::from(config.upload_chdman_path.trim()));
        let detected_chd_tools = chd::detect_chd_tools(custom_chdman.as_deref());
        let browser_path = if config.sftp.remote_path.trim().is_empty() {
            "/media/fat/games".to_string()
        } else {
            config.sftp.remote_path.clone()
        };

        Self {
            config,
            runtime,
            tx,
            rx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            busy: BusyTask::None,
            active_tab: AppTab::CoverScraper,
            status_text: "Listo. Elige la carpeta de juegos, la carpeta de imágenes y pulsa \"Analizar y Emparejar\".".to_string(),
            status_is_error: false,
            progress_fraction: None,
            show_password: false,
            analysis: None,
            selected_item_idx: None,
            filter_tab: FilterTab::All,
            search_query: String::new(),
            manual_cover_search: String::new(),
            upload_items: Vec::new(),
            upload_covers: Vec::new(),
            upload_covers_loaded_from: None,
            upload_existing_dirs: HashMap::new(),
            upload_selected_idx: None,
            upload_default_system: None,
            upload_overwrite_covers: false,
            upload_manual_cover_search: String::new(),
            detected_archive_tools,
            detected_chd_tools,
            thumbnails: HashMap::new(),
            browser_open: false,
            browser_path,
            browser_dirs: Vec::new(),
            browser_error: None,
            discovery_open: false,
            discovery_results: Vec::new(),
            discovery_error: None,
            discovery_scanned: (0, 0),
            duplicate_groups: Vec::new(),
            duplicate_root: String::new(),
            duplicate_is_sftp: false,
            duplicate_keep: HashMap::new(),
            duplicate_pending_paths: Vec::new(),
            duplicate_confirm_open: false,
            boot_demo_files: Vec::new(),
            boot_demo_selected: HashSet::new(),
            boot_demo_root: String::new(),
            boot_demo_is_sftp: false,
            boot_demo_pending_paths: Vec::new(),
            boot_demo_confirm_open: false,
            boot_demo_prompt_from_scraper: false,
        }
    }

    fn is_busy(&self) -> bool {
        self.busy != BusyTask::None
    }

    fn persist_config(&self) {
        let _ = self.config.save();
    }

    fn active_upload_games_base(&self) -> String {
        match self.config.storage_mode {
            StorageMode::Local => self.config.upload_local_games_base.trim().to_string(),
            StorageMode::Sftp => self.config.upload_sftp_games_base.trim().to_string(),
        }
    }

    fn refresh_detected_chd_tools(&mut self) {
        let custom = (!self.config.upload_chdman_path.trim().is_empty())
            .then(|| PathBuf::from(self.config.upload_chdman_path.trim()));
        self.detected_chd_tools = chd::detect_chd_tools(custom.as_deref());
    }

    fn current_upload_options(&self) -> UploadOptions {
        let chdman_path = (!self.config.upload_chdman_path.trim().is_empty())
            .then(|| PathBuf::from(self.config.upload_chdman_path.trim()));
        let temp_dir = (!self.config.upload_temp_dir.trim().is_empty())
            .then(|| PathBuf::from(self.config.upload_temp_dir.trim()));
        UploadOptions {
            default_system: self.upload_default_system,
            overwrite_games: self.config.upload_overwrite_games,
            overwrite_covers: self.upload_overwrite_covers,
            convert_covers_to_png: self.config.options.convert_to_png,
            strict_system_match: self.config.options.strict_system_match,
            cover_threshold: self.config.options.threshold,
            wrap_disc_in_folder: self.config.upload_wrap_disc_in_folder,
            media_placement: self.config.options.media_placement,
            convert_bin_cue_to_chd: self.config.upload_convert_to_chd,
            chdman_path,
            temp_dir,
            dry_run: self.config.options.dry_run,
        }
    }

    fn poll_worker_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Progress { current, message } => {
                    self.status_text = message;
                    self.status_is_error = false;
                    self.progress_fraction = current.map(|(cur, tot)| {
                        if tot == 0 {
                            0.0
                        } else {
                            cur as f32 / tot as f32
                        }
                    });
                }
                WorkerMsg::ConnectionTested(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(info) => {
                            self.status_text = info;
                            self.status_is_error = false;
                        }
                        Err(err) => {
                            self.status_text = format!("Error SFTP: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::DiscoveryProgress {
                    scanned,
                    total,
                    found,
                } => {
                    self.discovery_scanned = (scanned, total);
                    self.status_text = format!(
                        "Buscando MiSTer... {scanned}/{total} hosts escaneados, {found} candidato(s)."
                    );
                    self.status_is_error = false;
                    self.progress_fraction = Some(if total == 0 {
                        0.0
                    } else {
                        scanned as f32 / total as f32
                    });
                }
                WorkerMsg::DiscoveryFinished(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(results) => {
                            self.discovery_results = results;
                            self.discovery_error = None;
                            self.discovery_open = true;
                            if self.discovery_results.is_empty() {
                                self.status_text =
                                    "No se encontró ninguna MiSTer en la red local.".to_string();
                                self.status_is_error = true;
                            } else {
                                self.status_text = format!(
                                    "Se encontraron {} candidato(s) MiSTer. Selecciona uno.",
                                    self.discovery_results.len()
                                );
                                self.status_is_error = false;
                            }
                        }
                        Err(err) => {
                            self.discovery_results.clear();
                            self.discovery_error = Some(err.clone());
                            self.discovery_open = true;
                            self.status_text = format!("Error al buscar MiSTer: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::DuplicatesFound { result } => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match result {
                        Ok((groups, boot_demos, root, is_sftp)) => {
                            self.duplicate_root = root.clone();
                            self.duplicate_is_sftp = is_sftp;
                            self.duplicate_keep.clear();
                            for (idx, group) in groups.iter().enumerate() {
                                self.duplicate_keep.insert(idx, group.keep_idx);
                            }
                            let removable: usize = groups
                                .iter()
                                .map(|group| group.games.len().saturating_sub(1))
                                .sum();
                            self.duplicate_groups = groups;

                            self.boot_demo_root = root;
                            self.boot_demo_is_sftp = is_sftp;
                            self.boot_demo_selected.clear();
                            for item in &boot_demos {
                                self.boot_demo_selected.insert(item.path.clone());
                            }
                            let boot_count = boot_demos.len();
                            self.boot_demo_files = boot_demos;

                            self.status_text = match (self.duplicate_groups.is_empty(), boot_count) {
                                (true, 0) => {
                                    "No se encontraron juegos duplicados ni archivos mister-boot/mister-demo.".to_string()
                                }
                                (true, n) => {
                                    format!("Sin duplicados · {n} archivo(s) mister-boot/mister-demo detectado(s).")
                                }
                                (false, 0) => format!(
                                    "{} grupo(s) de duplicados · {} copia(s) marcadas para eliminar.",
                                    self.duplicate_groups.len(),
                                    removable
                                ),
                                (false, n) => format!(
                                    "{} grupo(s) de duplicados ({removable} copia(s)) · {n} archivo(s) mister-boot/mister-demo.",
                                    self.duplicate_groups.len()
                                ),
                            };
                            self.status_is_error = false;
                        }
                        Err(err) => {
                            self.duplicate_groups.clear();
                            self.boot_demo_files.clear();
                            self.boot_demo_selected.clear();
                            self.status_text = format!("Error buscando duplicados: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::DuplicatesDeleted(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(summary) => {
                            let dry = self.config.options.dry_run;
                            let verb = if dry { "Se eliminarían" } else { "Eliminados" };
                            let trash_note = summary
                                .trash_dir
                                .as_ref()
                                .map(|dir| format!("Papelera: {dir}"))
                                .unwrap_or_default();
                            self.status_text = format!(
                                "{verb} {} duplicado(s) y {} pista(s), {} fallidos. {trash_note}",
                                summary.deleted, summary.tracks_deleted, summary.failed
                            );
                            self.status_is_error = summary.failed > 0;

                            if !dry && summary.failed == 0 {
                                let removed = self.duplicate_pending_paths.clone();
                                for group in &mut self.duplicate_groups {
                                    group.games.retain(|game| !removed.contains(&game.path));
                                }
                                self.duplicate_groups.retain(|group| group.games.len() > 1);
                                self.duplicate_keep.clear();
                                for (idx, group) in self.duplicate_groups.iter().enumerate() {
                                    self.duplicate_keep.insert(
                                        idx,
                                        group.keep_idx.min(group.games.len().saturating_sub(1)),
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            self.status_text = format!("Error eliminando duplicados: {err}");
                            self.status_is_error = true;
                        }
                    }
                    self.duplicate_pending_paths.clear();
                }
                WorkerMsg::BootDemosDeleted(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(summary) => {
                            let dry = self.config.options.dry_run;
                            let verb = if dry { "Se eliminarían" } else { "Eliminados" };
                            let trash_note = summary
                                .trash_dir
                                .as_ref()
                                .map(|dir| format!("Papelera: {dir}"))
                                .unwrap_or_default();
                            self.status_text = format!(
                                "{verb} {} archivo(s) mister-boot/mister-demo y {} pista(s), {} fallidos. {trash_note}",
                                summary.deleted, summary.tracks_deleted, summary.failed
                            );
                            self.status_is_error = summary.failed > 0;

                            if !dry {
                                let failed_paths: HashSet<&str> = summary
                                    .errors
                                    .iter()
                                    .map(|(path, _)| path.as_str())
                                    .collect();
                                let removed: HashSet<String> = self
                                    .boot_demo_pending_paths
                                    .iter()
                                    .filter(|path| !failed_paths.contains(path.as_str()))
                                    .cloned()
                                    .collect();
                                if !removed.is_empty() {
                                    self.boot_demo_files.retain(|g| !removed.contains(&g.path));
                                    self.boot_demo_selected.retain(|path| !removed.contains(path));
                                    if let Some(ref mut analysis) = self.analysis {
                                        analysis
                                            .boot_demo_files
                                            .retain(|g| !removed.contains(&g.full_path));
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            self.status_text = format!("Error eliminando mister-boot/mister-demo: {err}");
                            self.status_is_error = true;
                        }
                    }
                    self.boot_demo_pending_paths.clear();
                }
                WorkerMsg::RemoteDirsLoaded { path, result } => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match result {
                        Ok(dirs) => {
                            self.browser_path = path;
                            self.browser_dirs = dirs;
                            self.browser_error = None;
                            self.status_text = format!("Explorando carpeta remota: {}", self.browser_path);
                            self.status_is_error = false;
                        }
                        Err(err) => {
                            self.browser_error = Some(err.clone());
                            self.status_text = format!("No se pudo listar carpeta remota: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::AnalysisFinished(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(analysis) => {
                            let total = analysis.items.len();
                            let covers = analysis.covers.len();
                            let ready = analysis.items.iter().filter(|i| i.selected_for_apply).count();
                            let exists = analysis
                                .items
                                .iter()
                                .filter(|i| i.status == MatchStatus::AlreadyExists)
                                .count();
                            let unmatched = analysis
                                .items
                                .iter()
                                .filter(|i| i.status == MatchStatus::Unmatched)
                                .count();

                            let boot_count = analysis.boot_demo_files.len();
                            if boot_count > 0 {
                                let detected = duplicates::find_boot_demo_files(&analysis.boot_demo_files);
                                self.boot_demo_selected.clear();
                                for item in &detected {
                                    self.boot_demo_selected.insert(item.path.clone());
                                }
                                self.boot_demo_files = detected;
                                self.boot_demo_root = analysis.games_root.clone();
                                self.boot_demo_is_sftp = analysis.is_sftp;
                                self.boot_demo_prompt_from_scraper = true;
                                self.boot_demo_confirm_open = true;
                            } else if self.boot_demo_root == analysis.games_root {
                                self.boot_demo_files.clear();
                                self.boot_demo_selected.clear();
                            }

                            self.selected_item_idx = if total > 0 { Some(0) } else { None };
                            self.analysis = Some(analysis);
                            let boot_note = if boot_count > 0 {
                                format!(" · {boot_count} archivo(s) mister-boot/mister-demo detectado(s)")
                            } else {
                                String::new()
                            };
                            self.status_text = format!(
                                "Análisis completado: {total} juegos, {covers} portadas indexadas ({ready} listas, {exists} ya existen, {unmatched} sin cover){boot_note}."
                            );
                            self.status_is_error = false;
                        }
                        Err(err) => {
                            self.status_text = format!("Error en análisis: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::ApplyItemFinished { item_idx, result } => {
                    if let Some(ref mut analysis) = self.analysis
                        && let Some(item) = analysis.items.get_mut(item_idx)
                    {
                        match result {
                            Ok(()) => {
                                if !self.config.options.dry_run {
                                    item.status = MatchStatus::Applied;
                                    item.selected_for_apply = false;
                                    item.existing_cover_name = Some(item.target_file_name.clone());
                                }
                            }
                            Err(err) => {
                                item.status = MatchStatus::Failed(err);
                            }
                        }
                    }
                }
                WorkerMsg::ApplyFinished(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(summary) => {
                            let dry = self.config.options.dry_run;
                            self.status_text = format!(
                                "{}: {} portadas {}, {} omitidas, {} errores. Consejo ConsoleMode: ejecuta 'Rebuild Game Database' / 'Optimize Artwork'.",
                                if dry { "Simulación completada" } else { "Sincronización completada" },
                                summary.copied,
                                if dry { "se copiarían" } else { "copiadas/subidas" },
                                summary.skipped + summary.already_exists,
                                summary.failed
                            );
                            self.status_is_error = summary.failed > 0;
                        }
                        Err(err) => {
                            self.status_text = format!("Error al aplicar portadas: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::UploadStaged(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(data) => {
                            if let Some(covers) = data.covers {
                                self.upload_covers = covers;
                                self.upload_covers_loaded_from = Some((
                                    self.config.images_dir.trim().to_string(),
                                    self.config.options.recursive_images,
                                ));
                            }
                            if let Some(dirs) = data.existing_dirs {
                                self.upload_existing_dirs = dirs;
                            }

                            let opts = self.current_upload_options();
                            let mut existing_keys: HashSet<(PathBuf, String)> = self
                                .upload_items
                                .iter()
                                .map(|i| i.payload.source_identity())
                                .collect();

                            let newly_planned = uploader::build_upload_plan(
                                data.discovered,
                                &self.upload_covers,
                                &data.games_base,
                                data.is_sftp,
                                &self.upload_existing_dirs,
                                &opts,
                            );
                            let mut added = 0usize;
                            for item in newly_planned {
                                let key = item.payload.source_identity();
                                if existing_keys.insert(key) {
                                    self.upload_items.push(item);
                                    added += 1;
                                }
                            }
                            if self.upload_selected_idx.is_none() && !self.upload_items.is_empty() {
                                self.upload_selected_idx = Some(0);
                            }

                            let total_q = self.upload_items.len();
                            let with_cov = self
                                .upload_items
                                .iter()
                                .filter(|i| i.selected_cover_idx.is_some())
                                .count();
                            self.status_text = format!(
                                "Cola actualizada (+{added} agregados, {total_q} en total, {with_cov} con carátula, {} portadas indexadas).",
                                self.upload_covers.len()
                            );
                            self.status_is_error = false;
                        }
                        Err(err) => {
                            self.status_text = format!("Error al preparar carga: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
                WorkerMsg::UploadItemStarting {
                    item_idx,
                    current,
                    total,
                    label,
                } => {
                    if let Some(item) = self.upload_items.get_mut(item_idx)
                        && !self.config.options.dry_run
                    {
                        item.status = UploadStatus::Uploading;
                    }
                    let verb = if self.config.options.dry_run {
                        "Simulando"
                    } else if self.config.storage_mode == StorageMode::Sftp {
                        "Subiendo"
                    } else {
                        "Copiando"
                    };
                    self.status_text = format!("[{current}/{total}] {verb}: {label}");
                    self.status_is_error = false;
                    self.progress_fraction = if total > 0 {
                        Some((current.saturating_sub(1)) as f32 / total as f32)
                    } else {
                        None
                    };
                }
                WorkerMsg::UploadStageProgress {
                    stage,
                    done,
                    total,
                    current,
                    total_items,
                } => {
                    let stage_frac = if total > 0 {
                        (done as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if total_items > 0 {
                        let base = (current.saturating_sub(1)) as f32 / total_items as f32;
                        self.progress_fraction = Some(base + (stage_frac * 0.5) / total_items as f32);
                    }
                    let pct = (stage_frac * 100.0).round() as u32;
                    self.status_text = format!("[{current}/{total_items}] {stage} ({pct}%)");
                    self.status_is_error = false;
                }
                WorkerMsg::UploadFileBytes {
                    file_name,
                    sent,
                    total,
                    current,
                    total_items,
                } => {
                    let item_frac = if total > 0 {
                        sent as f32 / total as f32
                    } else {
                        1.0
                    };
                    if total_items > 0 {
                        let base = (current.saturating_sub(1)) as f32 / total_items as f32;
                        self.progress_fraction = Some(base + item_frac / total_items as f32);
                    }
                    let verb = if self.config.storage_mode == StorageMode::Sftp {
                        "Subiendo"
                    } else {
                        "Copiando"
                    };
                    self.status_text = format!(
                        "[{current}/{total_items}] {verb} {file_name} — {} / {}",
                        uploader::format_bytes(sent),
                        uploader::format_bytes(total)
                    );
                }
                WorkerMsg::UploadItemFinished { item_idx, status } => {
                    if let Some(item) = self.upload_items.get_mut(item_idx) {
                        if !self.config.options.dry_run {
                            item.status = status.clone();
                            if matches!(status, UploadStatus::Done | UploadStatus::SkippedExists) {
                                item.selected = false;
                            }
                        } else {
                            item.status = UploadStatus::Ready;
                        }
                    }
                }
                WorkerMsg::UploadFinished(res) => {
                    self.busy = BusyTask::None;
                    self.progress_fraction = None;
                    match res {
                        Ok(summary) => {
                            let dry = self.config.options.dry_run;
                            let mut extras = Vec::new();
                            if summary.archives_extracted > 0 {
                                extras.push(format!("{} archivos extraídos", summary.archives_extracted));
                            }
                            if summary.chds_created > 0 {
                                extras.push(format!("{} CHD generados", summary.chds_created));
                            }
                            let extra_txt = if extras.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", extras.join(", "))
                            };
                            self.status_text = format!(
                                "{}: {} juegos {}, {} archivos ({}), {} carátulas {}{extra_txt}, {} errores. Consejo ConsoleMode: ejecuta 'Rebuild Game Database'.",
                                if dry { "Simulación completada" } else { "Carga completada" },
                                summary.games_uploaded,
                                if dry { "se cargarían" } else { "cargados" },
                                summary.files_transferred,
                                uploader::format_bytes(summary.bytes_transferred),
                                summary.covers_uploaded,
                                if dry { "se crearían" } else { "creadas" },
                                summary.failed
                            );
                            self.status_is_error = summary.failed > 0;
                        }
                        Err(err) => {
                            self.status_text = format!("Error en carga masiva: {err}");
                            self.status_is_error = true;
                        }
                    }
                }
            }
        }
    }

    fn start_discovery(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        self.persist_config();
        self.busy = BusyTask::Discovering;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.discovery_results.clear();
        self.discovery_error = None;
        self.discovery_scanned = (0, 0);
        self.discovery_open = false;
        self.status_text = "Buscando MiSTer en la red local...".to_string();
        self.status_is_error = false;
        self.progress_fraction = None;

        let options = DiscoverOptions {
            subnet: None,
            port: self.config.sftp.port,
            timeout: std::time::Duration::from_millis(400),
            verify: true,
            sftp_user: self.config.sftp.username.clone(),
            sftp_password: if self.config.sftp.password.trim().is_empty() {
                None
            } else {
                Some(self.config.sftp.password.clone())
            },
            sftp_key: (self.config.sftp.auth_mode == SftpAuthMode::PrivateKey
                && !self.config.sftp.private_key_path.trim().is_empty())
            .then(|| PathBuf::from(self.config.sftp.private_key_path.trim())),
            sftp_passphrase: (!self.config.sftp.passphrase.trim().is_empty())
                .then(|| self.config.sftp.passphrase.clone()),
            concurrency: 128,
        };
        let tx = self.tx.clone();
        let cancel = Arc::clone(&self.cancel_flag);

        self.runtime.spawn(async move {
            let tx_progress = tx.clone();
            let ctx_progress = ctx.clone();
            let result = discover::discover(&options, &cancel, move |progress| {
                let _ = tx_progress.send(WorkerMsg::DiscoveryProgress {
                    scanned: progress.scanned,
                    total: progress.total,
                    found: progress.found,
                });
                ctx_progress.request_repaint();
            })
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::DiscoveryFinished(result));
            ctx.request_repaint();
        });
    }

    fn start_scan_duplicates(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        self.persist_config();

        let is_sftp = self.config.storage_mode == StorageMode::Sftp;
        let root = match self.config.storage_mode {
            StorageMode::Local => self.config.local_games_dir.trim().to_string(),
            StorageMode::Sftp => self.config.sftp.remote_path.trim().to_string(),
        };
        if root.is_empty() {
            self.status_text = "Indica la carpeta de juegos antes de buscar duplicados.".to_string();
            self.status_is_error = true;
            return;
        }

        self.busy = BusyTask::ScanningDuplicates;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.duplicate_groups.clear();
        self.duplicate_keep.clear();
        self.duplicate_pending_paths.clear();
        self.boot_demo_files.clear();
        self.boot_demo_selected.clear();
        self.boot_demo_pending_paths.clear();
        self.status_text = "Buscando juegos duplicados y archivos mister-boot/mister-demo...".to_string();
        self.status_is_error = false;
        self.progress_fraction = None;

        let sftp_cfg = self.config.sftp.clone();
        let tx = self.tx.clone();
        let cancel = Arc::clone(&self.cancel_flag);
        let root_for_message = root.clone();

        self.runtime.spawn(async move {
            let scanned = async {
                let storage = if is_sftp {
                    StorageClient::connect_sftp(&sftp_cfg).await?
                } else {
                    StorageClient::Local
                };
                let tx_progress = tx.clone();
                let ctx_progress = ctx.clone();
                let result = storage
                    .scan_games(&root, &cancel, move |dir| {
                        let _ = tx_progress.send(WorkerMsg::Progress {
                            current: None,
                            message: format!("Escaneando: {dir}"),
                        });
                        ctx_progress.request_repaint();
                    })
                    .await;
                storage.close().await;
                result
            }
            .await;

            let result: std::result::Result<(Vec<DuplicateGroup>, Vec<DuplicateGame>, String, bool), String> =
                match scanned {
                    Ok(games) => Ok((
                        duplicates::find_duplicates(&games),
                        duplicates::find_boot_demo_files(&games),
                        root_for_message,
                        is_sftp,
                    )),
                    Err(err) => Err(format!("{err:#}")),
                };
            let _ = tx.send(WorkerMsg::DuplicatesFound { result });
            ctx.request_repaint();
        });
    }

    fn start_delete_duplicates(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }

        let mut paths = Vec::new();
        for (group_idx, group) in self.duplicate_groups.iter().enumerate() {
            let keep = self
                .duplicate_keep
                .get(&group_idx)
                .copied()
                .unwrap_or(group.keep_idx);
            for (game_idx, game) in group.games.iter().enumerate() {
                if game_idx != keep {
                    paths.push(game.path.clone());
                }
            }
        }
        if paths.is_empty() {
            self.status_text = "Selecciona qué copia conservar en cada grupo primero.".to_string();
            self.status_is_error = true;
            return;
        }

        self.persist_config();
        self.busy = BusyTask::DeletingDuplicates;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.duplicate_pending_paths = paths.clone();
        self.duplicate_confirm_open = false;
        self.status_text = "Eliminando duplicados...".to_string();
        self.status_is_error = false;

        let is_sftp = self.duplicate_is_sftp;
        let root = self.duplicate_root.clone();
        let dry_run = self.config.options.dry_run;
        let sftp_cfg = self.config.sftp.clone();
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let result = async {
                let storage = if is_sftp {
                    StorageClient::connect_sftp(&sftp_cfg).await?
                } else {
                    StorageClient::Local
                };
                let tx_progress = tx.clone();
                let ctx_progress = ctx.clone();
                let outcome = duplicates::delete_paths(
                    &storage,
                    &root,
                    is_sftp,
                    &paths,
                    dry_run,
                    move |message| {
                        let _ = tx_progress.send(WorkerMsg::Progress {
                            current: None,
                            message: message.to_string(),
                        });
                        ctx_progress.request_repaint();
                    },
                )
                .await;
                storage.close().await;
                outcome
            }
            .await
            .map_err(|err| format!("{err:#}"));

            let _ = tx.send(WorkerMsg::DuplicatesDeleted(result));
            ctx.request_repaint();
        });
    }

    fn start_delete_boot_demos(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }

        let paths: Vec<String> = self
            .boot_demo_files
            .iter()
            .filter(|item| self.boot_demo_selected.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        if paths.is_empty() {
            self.status_text =
                "Selecciona al menos un archivo mister-boot / mister-demo para eliminar.".to_string();
            self.status_is_error = true;
            return;
        }

        self.persist_config();
        self.busy = BusyTask::DeletingBootDemos;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.boot_demo_pending_paths = paths.clone();
        self.boot_demo_confirm_open = false;
        self.status_text = "Eliminando archivos mister-boot / mister-demo...".to_string();
        self.status_is_error = false;

        let is_sftp = self.boot_demo_is_sftp;
        let root = self.boot_demo_root.clone();
        let dry_run = self.config.options.dry_run;
        let sftp_cfg = self.config.sftp.clone();
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let result = async {
                let storage = if is_sftp {
                    StorageClient::connect_sftp(&sftp_cfg).await?
                } else {
                    StorageClient::Local
                };
                let tx_progress = tx.clone();
                let ctx_progress = ctx.clone();
                let outcome = duplicates::delete_paths(
                    &storage,
                    &root,
                    is_sftp,
                    &paths,
                    dry_run,
                    move |message| {
                        let _ = tx_progress.send(WorkerMsg::Progress {
                            current: None,
                            message: message.to_string(),
                        });
                        ctx_progress.request_repaint();
                    },
                )
                .await;
                storage.close().await;
                outcome
            }
            .await
            .map_err(|err| format!("{err:#}"));

            let _ = tx.send(WorkerMsg::BootDemosDeleted(result));
            ctx.request_repaint();
        });
    }

    fn start_test_connection(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        self.persist_config();
        self.busy = BusyTask::TestingConnection;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.status_text = format!(
            "Conectando a {}:{} por SSH/SFTP...",
            self.config.sftp.host, self.config.sftp.port
        );
        self.status_is_error = false;

        let sftp_cfg = self.config.sftp.clone();
        let target_raw = match self.active_tab {
            AppTab::CoverScraper | AppTab::Duplicates => self.config.sftp.remote_path.clone(),
            AppTab::BulkUploader => self.config.upload_sftp_games_base.clone(),
        };
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let result = async {
                let storage = StorageClient::connect_sftp(&sftp_cfg).await?;
                let target = if target_raw.trim().is_empty() {
                    "/"
                } else {
                    target_raw.trim()
                };
                let key_note = if storage.server_key_info().is_some() {
                    " [Clave SSH verificada]"
                } else {
                    ""
                };
                let dirs = storage.list_subdirs(target).await;
                let info = match dirs {
                    Ok(list) => format!(
                        "Conexión SFTP correcta con {}:{}{}. Se encontraron {} subcarpetas en {}.",
                        sftp_cfg.host,
                        sftp_cfg.port,
                        key_note,
                        list.len(),
                        target
                    ),
                    Err(_) => format!(
                        "Conexión SFTP correcta con {}:{}{} (la ruta '{}' no existe aún o no se pudo listar).",
                        sftp_cfg.host, sftp_cfg.port, key_note, target
                    ),
                };
                storage.close().await;
                Ok::<String, anyhow::Error>(info)
            }
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::ConnectionTested(result));
            ctx.request_repaint();
        });
    }

    fn start_browse_remote(&mut self, path: String, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        self.persist_config();
        self.busy = BusyTask::BrowsingRemote;
        self.browser_open = true;
        self.browser_error = None;

        let norm = posix_normalize(&path);
        let sftp_cfg = self.config.sftp.clone();
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let result = async {
                let storage = StorageClient::connect_sftp(&sftp_cfg).await?;
                let dirs = storage.list_subdirs(&norm).await;
                storage.close().await;
                dirs
            }
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::RemoteDirsLoaded { path: norm, result });
            ctx.request_repaint();
        });
    }

    fn start_analysis(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        let images_str = self.config.images_dir.trim().to_string();
        if images_str.is_empty() {
            self.status_text = "Selecciona una carpeta de imágenes primero.".to_string();
            self.status_is_error = true;
            return;
        }
        let games_root = match self.config.storage_mode {
            StorageMode::Local => self.config.local_games_dir.trim().to_string(),
            StorageMode::Sftp => self.config.sftp.remote_path.trim().to_string(),
        };
        if games_root.is_empty() {
            self.status_text = "Indica la carpeta de juegos (local o remota en MiSTer).".to_string();
            self.status_is_error = true;
            return;
        }

        self.persist_config();
        self.busy = BusyTask::Analyzing;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.status_text = "Iniciando escaneo...".to_string();
        self.status_is_error = false;
        self.progress_fraction = None;

        let storage_mode = self.config.storage_mode;
        let sftp_cfg = self.config.sftp.clone();
        let options = self.config.options.clone();
        let images_dir = PathBuf::from(images_str);
        let cancel = Arc::clone(&self.cancel_flag);
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let res = async {
                let storage = match storage_mode {
                    StorageMode::Local => StorageClient::Local,
                    StorageMode::Sftp => {
                        let _ = tx.send(WorkerMsg::Progress {
                            current: None,
                            message: format!(
                                "Conectando a {}:{} por SFTP...",
                                sftp_cfg.host, sftp_cfg.port
                            ),
                        });
                        ctx.request_repaint();
                        StorageClient::connect_sftp(&sftp_cfg).await?
                    }
                };

                let tx_prog = tx.clone();
                let ctx_prog = ctx.clone();
                let out = sync::analyze(
                    &storage,
                    &games_root,
                    &images_dir,
                    &options,
                    &cancel,
                    move |message| {
                        let _ = tx_prog.send(WorkerMsg::Progress {
                            current: None,
                            message,
                        });
                        ctx_prog.request_repaint();
                    },
                )
                .await;

                storage.close().await;
                out
            }
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::AnalysisFinished(res));
            ctx.request_repaint();
        });
    }

    fn start_apply(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        let Some(ref analysis) = self.analysis else {
            return;
        };
        let selected_count = analysis.items.iter().filter(|i| i.selected_for_apply).count();
        if selected_count == 0 {
            self.status_text = "No hay portadas seleccionadas para aplicar.".to_string();
            self.status_is_error = false;
            return;
        }

        self.persist_config();
        self.busy = BusyTask::Applying;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.progress_fraction = Some(0.0);

        let items = analysis.items.clone();
        let covers = analysis.covers.clone();
        let is_sftp_analysis = analysis.is_sftp;
        let sftp_cfg = self.config.sftp.clone();
        let options = self.config.options.clone();
        let cancel = Arc::clone(&self.cancel_flag);
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let res = async {
                let storage = if is_sftp_analysis {
                    let _ = tx.send(WorkerMsg::Progress {
                        current: None,
                        message: format!(
                            "Conectando a {}:{} para subir portadas...",
                            sftp_cfg.host, sftp_cfg.port
                        ),
                    });
                    ctx.request_repaint();
                    StorageClient::connect_sftp(&sftp_cfg).await?
                } else {
                    StorageClient::Local
                };

                let tx_evt = tx.clone();
                let ctx_evt = ctx.clone();
                let summary =
                    sync::apply(&storage, &items, &covers, &options, &cancel, move |evt| {
                        match evt {
                            ApplyEvent::ItemStarting {
                                current,
                                total,
                                file_name,
                            } => {
                                let verb = if options.dry_run {
                                    "Simulando"
                                } else if is_sftp_analysis {
                                    "Subiendo"
                                } else {
                                    "Copiando"
                                };
                                let _ = tx_evt.send(WorkerMsg::Progress {
                                    current: Some((current.saturating_sub(1), total)),
                                    message: format!("[{current}/{total}] {verb}: {file_name}"),
                                });
                            }
                            ApplyEvent::ItemFinished { item_idx, result } => {
                                let _ =
                                    tx_evt.send(WorkerMsg::ApplyItemFinished { item_idx, result });
                            }
                        }
                        ctx_evt.request_repaint();
                    })
                    .await;

                storage.close().await;
                summary
            }
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::ApplyFinished(res));
            ctx.request_repaint();
        });
    }

    fn start_stage_uploads(&mut self, paths: Vec<PathBuf>, force_reload_covers: bool, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        self.persist_config();
        self.busy = BusyTask::StagingUpload;
        self.progress_fraction = None;
        self.status_text = if paths.is_empty() {
            "Recargando portadas y carpetas destino...".to_string()
        } else {
            format!("Analizando {} rutas agregadas (archivos y carpetas)...", paths.len())
        };
        self.status_is_error = false;

        let images_str = self.config.images_dir.trim().to_string();
        let recursive_images = self.config.options.recursive_images;
        let need_covers = force_reload_covers
            || (!images_str.is_empty()
                && self.upload_covers_loaded_from.as_ref()
                    != Some(&(images_str.clone(), recursive_images)));
        let games_base = self.active_upload_games_base();
        let storage_mode = self.config.storage_mode;
        let sftp_cfg = self.config.sftp.clone();
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let res = async {
                let discovered = if paths.is_empty() {
                    Vec::new()
                } else {
                    tokio::task::spawn_blocking(move || uploader::discover_upload_sources(&paths))
                        .await??
                };

                let covers = if need_covers {
                    if images_str.is_empty() {
                        Some(Vec::new())
                    } else {
                        let dir = PathBuf::from(&images_str);
                        if dir.is_dir() {
                            Some(
                                tokio::task::spawn_blocking(move || {
                                    scanner::scan_local_covers(&dir, recursive_images)
                                })
                                .await??,
                            )
                        } else {
                            Some(Vec::new())
                        }
                    }
                } else {
                    None
                };

                let is_sftp = storage_mode == StorageMode::Sftp;
                let existing_dirs = if games_base.is_empty() {
                    None
                } else if !is_sftp {
                    let storage = StorageClient::Local;
                    Some(uploader::resolve_existing_system_dirs(&storage, &games_base).await)
                } else if !sftp_cfg.host.trim().is_empty() {
                    match StorageClient::connect_sftp(&sftp_cfg).await {
                        Ok(storage) => {
                            let dirs =
                                uploader::resolve_existing_system_dirs(&storage, &games_base).await;
                            storage.close().await;
                            Some(dirs)
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };

                Ok::<StagedUploadData, anyhow::Error>(StagedUploadData {
                    discovered,
                    covers,
                    existing_dirs,
                    games_base,
                    is_sftp,
                })
            }
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::UploadStaged(res));
            ctx.request_repaint();
        });
    }

    fn start_execute_upload(&mut self, ctx: egui::Context) {
        if self.is_busy() {
            return;
        }
        let games_base = self.active_upload_games_base();
        if games_base.is_empty() {
            self.status_text =
                "Indica la carpeta raíz de juegos en la MiSTer (ej. D:\\MiSTer\\games o /media/fat/games)."
                    .to_string();
            self.status_is_error = true;
            return;
        }

        // Ensure paths are up-to-date right before starting upload
        self.rematch_upload_queue();

        let ready_count = self
            .upload_items
            .iter()
            .filter(|i| i.selected && i.target_system.is_some())
            .count();
        if ready_count == 0 {
            self.status_text =
                "No hay juegos listos seleccionados en la cola (asigna una consola si falta)."
                    .to_string();
            self.status_is_error = true;
            return;
        }

        self.persist_config();
        self.busy = BusyTask::Uploading;
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.progress_fraction = Some(0.0);

        let items = self.upload_items.clone();
        let covers = self.upload_covers.clone();
        let options = self.current_upload_options();
        let storage_mode = self.config.storage_mode;
        let sftp_cfg = self.config.sftp.clone();
        let cancel = Arc::clone(&self.cancel_flag);
        let tx = self.tx.clone();

        self.runtime.spawn(async move {
            let res = async {
                let storage = match storage_mode {
                    StorageMode::Local => StorageClient::Local,
                    StorageMode::Sftp => {
                        let _ = tx.send(WorkerMsg::Progress {
                            current: None,
                            message: format!(
                                "Conectando a {}:{} por SFTP para cargar juegos...",
                                sftp_cfg.host, sftp_cfg.port
                            ),
                        });
                        ctx.request_repaint();
                        StorageClient::connect_sftp(&sftp_cfg).await?
                    }
                };

                let pos_state = Arc::new(std::sync::Mutex::new((0usize, 0usize, 0u64, String::new())));
                let pos_for_cb = Arc::clone(&pos_state);
                let tx_evt = tx.clone();
                let ctx_evt = ctx.clone();

                let summary = uploader::execute_upload(
                    &storage,
                    &items,
                    &covers,
                    &options,
                    &cancel,
                    move |evt| match evt {
                        UploadEvent::ItemStarting {
                            item_idx,
                            current,
                            total,
                            label,
                        } => {
                            if let Ok(mut g) = pos_for_cb.lock() {
                                *g = (current, total, 0, String::new());
                            }
                            let _ = tx_evt.send(WorkerMsg::UploadItemStarting {
                                item_idx,
                                current,
                                total,
                                label,
                            });
                            ctx_evt.request_repaint();
                        }
                        UploadEvent::StageProgress {
                            stage,
                            done,
                            total,
                            ..
                        } => {
                            let (cur, tot_items, should_send) = if let Ok(mut g) = pos_for_cb.lock() {
                                let stage_changed = g.3 != stage;
                                let throttle_ok = stage_changed
                                    || done == 0
                                    || done >= total
                                    || done.saturating_sub(g.2) >= 2_097_152;
                                if throttle_ok {
                                    g.2 = done;
                                    g.3 = stage.clone();
                                }
                                (g.0, g.1, throttle_ok)
                            } else {
                                (0, 0, false)
                            };
                            if should_send {
                                let _ = tx_evt.send(WorkerMsg::UploadStageProgress {
                                    stage,
                                    done,
                                    total,
                                    current: cur,
                                    total_items: tot_items,
                                });
                                ctx_evt.request_repaint();
                            }
                        }
                        UploadEvent::FileBytes {
                            file_name,
                            sent,
                            total,
                            ..
                        } => {
                            let (cur, tot_items, should_send) = if let Ok(mut g) = pos_for_cb.lock() {
                                let throttle_ok =
                                    sent == 0 || sent == total || sent.saturating_sub(g.2) >= 524_288;
                                if throttle_ok {
                                    g.2 = sent;
                                }
                                (g.0, g.1, throttle_ok)
                            } else {
                                (0, 0, false)
                            };
                            if should_send {
                                let _ = tx_evt.send(WorkerMsg::UploadFileBytes {
                                    file_name,
                                    sent,
                                    total,
                                    current: cur,
                                    total_items: tot_items,
                                });
                                ctx_evt.request_repaint();
                            }
                        }
                        UploadEvent::ItemFinished { item_idx, status } => {
                            let _ = tx_evt.send(WorkerMsg::UploadItemFinished { item_idx, status });
                            ctx_evt.request_repaint();
                        }
                    },
                )
                .await;

                storage.close().await;
                summary
            }
            .await
            .map_err(|e| format!("{e:#}"));

            let _ = tx.send(WorkerMsg::UploadFinished(res));
            ctx.request_repaint();
        });
    }

    fn reapply_threshold_to_analysis(&mut self) {
        if let Some(ref mut analysis) = self.analysis {
            sync::rematch_analysis(analysis, &self.config.options);
        }
        self.rematch_upload_queue();
    }

    fn recompute_all_target_paths(&mut self) {
        let convert_to_png = self.config.options.convert_to_png;
        if let Some(ref mut analysis) = self.analysis {
            let is_sftp = analysis.is_sftp;
            for item in &mut analysis.items {
                sync::recompute_target_paths(item, &analysis.covers, convert_to_png, is_sftp);
            }
        }
    }

    fn rematch_upload_queue(&mut self) {
        if self.upload_items.is_empty() {
            return;
        }
        let games_base = self.active_upload_games_base();
        let is_sftp = self.config.storage_mode == StorageMode::Sftp;
        let opts = self.current_upload_options();
        uploader::rematch_upload_items(
            &mut self.upload_items,
            &self.upload_covers,
            &games_base,
            is_sftp,
            &self.upload_existing_dirs,
            &opts,
        );
    }

    fn get_or_load_thumbnail(
        &mut self,
        ctx: &egui::Context,
        path: &PathBuf,
    ) -> std::result::Result<&egui::TextureHandle, &String> {
        if !self.thumbnails.contains_key(path) {
            if self.thumbnails.len() >= 64 {
                self.thumbnails.clear();
            }
            let entry = match imaging::load_thumbnail_rgba(path, 280) {
                Ok((w, h, rgba)) => {
                    let color_img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let tex = ctx.load_texture(
                        path.to_string_lossy().as_ref(),
                        color_img,
                        egui::TextureOptions::LINEAR,
                    );
                    Ok(tex)
                }
                Err(err) => Err(format!("{err:#}")),
            };
            self.thumbnails.insert(path.clone(), entry);
        }
        match self.thumbnails.get(path).unwrap() {
            Ok(tex) => Ok(tex),
            Err(err) => Err(err),
        }
    }

    fn render_cover_preview_box(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, cover: Option<&CoverImage>) {
        if let Some(cov) = cover {
            ui.label(format!("Cover asignado: {}", cov.file_name));
            let path_buf = cov.path.clone();
            match self.get_or_load_thumbnail(ctx, &path_buf) {
                Ok(tex) => {
                    let size = tex.size_vec2();
                    let max_w = ui.available_width().min(260.0);
                    let max_h = 210.0;
                    let scale = (max_w / size.x).min(max_h / size.y).min(1.0);
                    ui.vertical_centered(|ui| {
                        ui.image((tex.id(), size * scale));
                    });
                }
                Err(err) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 110, 110),
                        format!("Sin vista previa: {err}"),
                    );
                }
            }
        } else {
            ui.label("Sin portada asignada.");
        }
    }

    fn render_sftp_credentials_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Host/IP:");
            ui.add(
                egui::TextEdit::singleline(&mut self.config.sftp.host)
                    .desired_width(160.0)
                    .hint_text("192.168.1.50 / mister.local"),
            );
            ui.label("Puerto:");
            ui.add(egui::DragValue::new(&mut self.config.sftp.port).range(1..=65535));
            ui.label("Usuario:");
            ui.add(egui::TextEdit::singleline(&mut self.config.sftp.username).desired_width(80.0));
            ui.label("Auth:");
            ui.selectable_value(
                &mut self.config.sftp.auth_mode,
                SftpAuthMode::Password,
                "Contraseña",
            );
            ui.selectable_value(
                &mut self.config.sftp.auth_mode,
                SftpAuthMode::PrivateKey,
                "Clave SSH",
            );

            match self.config.sftp.auth_mode {
                SftpAuthMode::Password => {
                    ui.label("Clave:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.sftp.password)
                            .password(!self.show_password)
                            .desired_width(100.0),
                    );
                    if ui
                        .button(if self.show_password { "Ocultar" } else { "Ver" })
                        .clicked()
                    {
                        self.show_password = !self.show_password;
                    }
                }
                SftpAuthMode::PrivateKey => {
                    ui.label("Archivo:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.sftp.private_key_path)
                            .desired_width(150.0)
                            .hint_text("id_ed25519"),
                    );
                    if ui.button("...").clicked()
                        && let Some(path) = FileDialog::new().pick_file()
                    {
                        self.config.sftp.private_key_path = path.to_string_lossy().to_string();
                    }
                    ui.label("Passphrase:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.sftp.passphrase)
                            .password(true)
                            .desired_width(80.0),
                    );
                }
            }

            ui.separator();
            if ui
                .add_enabled(!self.is_busy(), egui::Button::new("🔎 Buscar MiSTer"))
                .on_hover_text("Escanea la red local buscando hosts con SSH/SFTP de MiSTer")
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_discovery(ctx);
            }
        });
    }

    fn render_duplicates_top_controls(&mut self, ui: &mut egui::Ui, busy: bool, ctx: &egui::Context) {
        ui.add_enabled_ui(!busy, |ui| {
            match self.config.storage_mode {
                StorageMode::Local => {
                    ui.horizontal(|ui| {
                        ui.label("Carpeta de juegos a analizar:");
                        ui.add_sized(
                            [(ui.available_width() - 150.0).max(120.0), 22.0],
                            egui::TextEdit::singleline(&mut self.config.local_games_dir)
                                .hint_text(r"D:\MiSTer\games  o  /media/fat/games"),
                        );
                        if ui.button("Elegir carpeta...").clicked()
                            && let Some(path) = FileDialog::new().pick_folder()
                        {
                            self.config.local_games_dir = path.to_string_lossy().to_string();
                        }
                    });
                }
                StorageMode::Sftp => {
                    self.render_sftp_credentials_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("Ruta de juegos en MiSTer:");
                        ui.add_sized(
                            [(ui.available_width() - 280.0).max(120.0), 22.0],
                            egui::TextEdit::singleline(&mut self.config.sftp.remote_path)
                                .hint_text("/media/fat/games"),
                        );
                    });
                }
            }

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("🔍 Buscar duplicados y boot/demo"))
                    .on_hover_text("Agrupa juegos duplicados y detecta archivos mister-boot / mister-demo")
                    .clicked()
                {
                    self.start_scan_duplicates(ctx.clone());
                }
                let removable: usize = self
                    .duplicate_groups
                    .iter()
                    .map(|group| group.games.len().saturating_sub(1))
                    .sum();
                let label = if self.config.options.dry_run {
                    format!("▶ Simular duplicados ({removable})")
                } else {
                    format!("🗑 Eliminar duplicados ({removable})")
                };
                if ui
                    .add_enabled(!busy && removable > 0, egui::Button::new(label))
                    .clicked()
                {
                    self.duplicate_confirm_open = true;
                }

                let selected_boot = self
                    .boot_demo_files
                    .iter()
                    .filter(|item| self.boot_demo_selected.contains(&item.path))
                    .count();
                let boot_label = if self.config.options.dry_run {
                    format!("▶ Simular boot/demo ({selected_boot})")
                } else {
                    format!("🧹 Eliminar boot/demo ({selected_boot})")
                };
                if ui
                    .add_enabled(!busy && selected_boot > 0, egui::Button::new(boot_label))
                    .clicked()
                {
                    self.boot_demo_prompt_from_scraper = false;
                    self.boot_demo_confirm_open = true;
                }

                ui.checkbox(&mut self.config.options.dry_run, "Simulación (Dry-Run)");
            });
        });
    }

    fn render_top_panel(&mut self, ui: &mut egui::Ui) {
        let busy = self.is_busy();
        let ctx = ui.ctx().clone();

        egui::Panel::top("top_config_panel").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading("MiSTer ConsoleMode Manager");
                ui.separator();
                ui.selectable_value(
                    &mut self.active_tab,
                    AppTab::CoverScraper,
                    "🖼 Scraper de Covers",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    AppTab::BulkUploader,
                    "📦 Cargar Juegos (Bulk)",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    AppTab::Duplicates,
                    "🧹 Duplicados / Limpieza",
                );
                ui.separator();
                ui.label("Almacenamiento:");
                ui.add_enabled_ui(!busy, |ui| {
                    if ui
                        .selectable_value(
                            &mut self.config.storage_mode,
                            StorageMode::Local,
                            "Carpeta Local / SMB",
                        )
                        .changed()
                    {
                        self.upload_existing_dirs.clear();
                        self.rematch_upload_queue();
                    }
                    if ui
                        .selectable_value(
                            &mut self.config.storage_mode,
                            StorageMode::Sftp,
                            "MiSTer Remoto (SFTP / SSH)",
                        )
                        .changed()
                    {
                        self.upload_existing_dirs.clear();
                        self.rematch_upload_queue();
                    }
                });
            });

            ui.separator();

            match self.active_tab {
                AppTab::CoverScraper => self.render_scraper_top_controls(ui, busy, &ctx),
                AppTab::BulkUploader => self.render_uploader_top_controls(ui, busy, &ctx),
                AppTab::Duplicates => self.render_duplicates_top_controls(ui, busy, &ctx),
            }
            ui.add_space(4.0);
        });
    }

    fn render_scraper_top_controls(&mut self, ui: &mut egui::Ui, busy: bool, ctx: &egui::Context) {
        ui.add_enabled_ui(!busy, |ui| {
            match self.config.storage_mode {
                StorageMode::Local => {
                    ui.horizontal(|ui| {
                        ui.label("Carpeta de juegos (ej. PSX, /media/fat/games/PSX o \\\\mister\\fat\\games\\PSX):");
                        ui.add_sized(
                            [(ui.available_width() - 150.0).max(120.0), 22.0],
                            egui::TextEdit::singleline(&mut self.config.local_games_dir)
                                .hint_text(r"D:\MiSTer\games\PSX"),
                        );
                        if ui.button("Elegir carpeta...").clicked()
                            && let Some(path) = FileDialog::new().pick_folder()
                        {
                            self.config.local_games_dir = path.to_string_lossy().to_string();
                        }
                    });
                }
                StorageMode::Sftp => {
                    self.render_sftp_credentials_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("Ruta remota MiSTer:");
                        ui.add_sized(
                            [(ui.available_width() - 280.0).max(120.0), 22.0],
                            egui::TextEdit::singleline(&mut self.config.sftp.remote_path)
                                .hint_text("/media/fat/games/PSX"),
                        );
                        if ui.button("Probar conexión").clicked() {
                            self.start_test_connection(ctx.clone());
                        }
                        if ui.button("Explorar MiSTer...").clicked() {
                            let start_path = if self.config.sftp.remote_path.trim().is_empty() {
                                "/media/fat/games".to_string()
                            } else {
                                self.config.sftp.remote_path.clone()
                            };
                            self.start_browse_remote(start_path, ctx.clone());
                        }
                    });
                }
            }

            ui.horizontal(|ui| {
                ui.label("Carpeta de imágenes (covers locales):");
                ui.add_sized(
                    [(ui.available_width() - 150.0).max(120.0), 22.0],
                    egui::TextEdit::singleline(&mut self.config.images_dir)
                        .hint_text(r"D:\Covers\sony playstation"),
                );
                if ui.button("Elegir carpeta...").clicked()
                    && let Some(path) = FileDialog::new().pick_folder()
                {
                    self.config.images_dir = path.to_string_lossy().to_string();
                }
            });

            ui.horizontal_wrapped(|ui| {
                ui.label("Similitud mínima:");
                let mut pct = (self.config.options.threshold * 100.0).round();
                if ui
                    .add(egui::Slider::new(&mut pct, 50.0..=100.0).suffix("%"))
                    .changed()
                {
                    self.config.options.threshold = (pct / 100.0).clamp(0.5, 1.0);
                    self.reapply_threshold_to_analysis();
                }

                if ui
                    .checkbox(&mut self.config.options.convert_to_png, "Convertir a PNG")
                    .changed()
                {
                    self.recompute_all_target_paths();
                }

                if ui
                    .checkbox(
                        &mut self.config.options.overwrite_existing,
                        "Sobrescribir existentes",
                    )
                    .changed()
                {
                    self.reapply_threshold_to_analysis();
                }

                ui.checkbox(
                    &mut self.config.options.recursive_images,
                    "Imágenes en subcarpetas",
                );

                if ui
                    .checkbox(
                        &mut self.config.options.strict_system_match,
                        "Solo misma consola",
                    )
                    .on_hover_text(
                        "Evita asignar portadas de otra consola (ej. PSX a juegos de GBA) aunque compartan nombre.",
                    )
                    .changed()
                {
                    self.reapply_threshold_to_analysis();
                }

                ui.checkbox(&mut self.config.options.dry_run, "Simulación (Dry-Run)");

                ui.label("Ubicación media/:");
                egui::ComboBox::from_id_salt("media_placement_combo")
                    .selected_text(match self.config.options.media_placement {
                        MediaPlacement::RootMedia => "Raíz: <juegos>/media (ConsoleMode)",
                        MediaPlacement::PerSubfolderMedia => "Por cada subcarpeta",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.config.options.media_placement,
                            MediaPlacement::RootMedia,
                            "Raíz: <juegos>/media (ConsoleMode)",
                        );
                        ui.selectable_value(
                            &mut self.config.options.media_placement,
                            MediaPlacement::PerSubfolderMedia,
                            "Por cada subcarpeta",
                        );
                    });
            });
        });

        ui.separator();

        let ready_count = self
            .analysis
            .as_ref()
            .map(|a| a.items.iter().filter(|i| i.selected_for_apply).count())
            .unwrap_or(0);

        ui.horizontal(|ui| {
            let analyze_btn = egui::Button::new("🔍 Analizar y Emparejar")
                .fill(egui::Color32::from_rgb(38, 92, 168));
            if ui.add_enabled(!busy, analyze_btn).clicked() {
                self.start_analysis(ctx.clone());
            }

            let apply_label = if self.config.options.dry_run {
                format!("▶ Simular Covers Seleccionados ({ready_count})")
            } else {
                format!("✅ Aplicar Covers Seleccionados ({ready_count})")
            };
            let apply_btn =
                egui::Button::new(apply_label).fill(egui::Color32::from_rgb(36, 128, 66));
            if ui.add_enabled(!busy && ready_count > 0, apply_btn).clicked() {
                self.start_apply(ctx.clone());
            }

            if busy {
                let cancel_btn =
                    egui::Button::new("⏹ Cancelar").fill(egui::Color32::from_rgb(160, 48, 48));
                if ui.add(cancel_btn).clicked() {
                    self.cancel_flag.store(true, Ordering::Relaxed);
                    self.status_text = "Cancelando...".to_string();
                }
            }
        });
    }

    fn render_uploader_top_controls(&mut self, ui: &mut egui::Ui, busy: bool, ctx: &egui::Context) {
        ui.add_enabled_ui(!busy, |ui| {
            match self.config.storage_mode {
                StorageMode::Local => {
                    ui.horizontal(|ui| {
                        ui.label("Carpeta raíz games/ de MiSTer (contendrá PSX/, GBA/, SNES/...):");
                        if ui
                            .add_sized(
                                [(ui.available_width() - 150.0).max(120.0), 22.0],
                                egui::TextEdit::singleline(&mut self.config.upload_local_games_base)
                                    .hint_text(r"D:\MiSTer\games  o  \\mister\fat\games"),
                            )
                            .changed()
                        {
                            self.rematch_upload_queue();
                        }
                        if ui.button("Elegir carpeta...").clicked()
                            && let Some(path) = FileDialog::new().pick_folder()
                        {
                            self.config.upload_local_games_base = path.to_string_lossy().to_string();
                            self.start_stage_uploads(Vec::new(), false, ctx.clone());
                        }
                    });
                }
                StorageMode::Sftp => {
                    self.render_sftp_credentials_row(ui);
                    ui.horizontal(|ui| {
                        ui.label("Carpeta raíz games/ en MiSTer:");
                        if ui
                            .add_sized(
                                [(ui.available_width() - 280.0).max(120.0), 22.0],
                                egui::TextEdit::singleline(&mut self.config.upload_sftp_games_base)
                                    .hint_text("/media/fat/games"),
                            )
                            .changed()
                        {
                            self.rematch_upload_queue();
                        }
                        if ui.button("Probar conexión").clicked() {
                            self.start_test_connection(ctx.clone());
                        }
                        if ui.button("Explorar MiSTer...").clicked() {
                            let start_path =
                                if self.config.upload_sftp_games_base.trim().is_empty() {
                                    "/media/fat/games".to_string()
                                } else {
                                    self.config.upload_sftp_games_base.clone()
                                };
                            self.start_browse_remote(start_path, ctx.clone());
                        }
                    });
                }
            }

            ui.horizontal(|ui| {
                ui.label("Carpeta de carátulas (opcional, asigna covers al subir):");
                ui.add_sized(
                    [(ui.available_width() - 290.0).max(120.0), 22.0],
                    egui::TextEdit::singleline(&mut self.config.images_dir)
                        .hint_text(r"D:\Covers (carpeta raíz con subcarpetas por consola)"),
                );
                if ui.button("Elegir carpeta...").clicked()
                    && let Some(path) = FileDialog::new().pick_folder()
                {
                    self.config.images_dir = path.to_string_lossy().to_string();
                    self.start_stage_uploads(Vec::new(), true, ctx.clone());
                }
                if ui.button("Recargar portadas").clicked() {
                    self.start_stage_uploads(Vec::new(), true, ctx.clone());
                }
            });

            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("📄 Agregar archivos / ZIP / 7Z / RAR...")
                    .on_hover_text(
                        "Selecciona ROMs sueltos (.gba, .sfc, .chd...), hojas .cue/.gdi o archivos comprimidos (.zip, .7z, .rar)",
                    )
                    .clicked()
                    && let Some(files) = FileDialog::new()
                        .add_filter(
                            "Juegos y comprimidos",
                            &[
                                "zip", "7z", "rar", "cue", "gdi", "chd", "iso", "m3u", "gba",
                                "gb", "gbc", "nes", "sfc", "smc", "md", "gen", "n64", "z64",
                                "v64", "pce", "sms", "gg", "nds", "bin",
                            ],
                        )
                        .add_filter("Todos los archivos", &["*"])
                        .pick_files()
                {
                    self.start_stage_uploads(files, false, ctx.clone());
                }

                if ui
                    .button("📁 Agregar carpetas (bin/cue o lotes)...")
                    .on_hover_text(
                        "Selecciona una o más carpetas con .cue/.bin, .m3u, comprimidos o subcarpetas por consola",
                    )
                    .clicked()
                    && let Some(folders) = FileDialog::new().pick_folders()
                {
                    self.start_stage_uploads(folders, false, ctx.clone());
                }

                if ui
                    .add_enabled(!self.upload_items.is_empty(), egui::Button::new("🧹 Limpiar cola"))
                    .clicked()
                {
                    self.upload_items.clear();
                    self.upload_selected_idx = None;
                }

                ui.separator();
                ui.label("Consola por defecto (si es ambigua):");
                egui::ComboBox::from_id_salt("upload_default_sys_combo")
                    .selected_text(
                        self.upload_default_system
                            .map(|s| s.short_label())
                            .unwrap_or("Auto-detectar"),
                    )
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_value(&mut self.upload_default_system, None, "Auto-detectar")
                            .changed()
                        {
                            self.rematch_upload_queue();
                        }
                        for &sys in SystemId::ALL {
                            if ui
                                .selectable_value(
                                    &mut self.upload_default_system,
                                    Some(sys),
                                    format!("{} — {}", sys.short_label(), sys.display_name()),
                                )
                                .changed()
                            {
                                self.rematch_upload_queue();
                            }
                        }
                    });
            });

            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.config.upload_overwrite_games, "Sobrescribir juegos");
                ui.checkbox(&mut self.upload_overwrite_covers, "Sobrescribir covers");

                if ui
                    .checkbox(&mut self.config.options.convert_to_png, "Convertir covers a PNG")
                    .changed()
                {
                    self.recompute_all_target_paths();
                }

                if ui
                    .checkbox(
                        &mut self.config.options.strict_system_match,
                        "Solo misma consola",
                    )
                    .changed()
                {
                    self.reapply_threshold_to_analysis();
                }

                if ui
                    .checkbox(
                        &mut self.config.upload_convert_to_chd,
                        "💿 Convertir BIN/CUE → CHD",
                    )
                    .on_hover_text(
                        "Convierte automáticamente discos .cue/.bin (y .gdi) a un solo archivo .chd comprimido usando chd-rs integrado (o chdman si está disponible)",
                    )
                    .changed()
                {
                    self.rematch_upload_queue();
                    self.persist_config();
                }

                if ui
                    .checkbox(
                        &mut self.config.upload_wrap_disc_in_folder,
                        "Subcarpeta por .chd/.iso suelto",
                    )
                    .on_hover_text(
                        "Útil en MiSTer PSX para que cada disco suelto tenga su propia memory card (<PSX>/<Juego>/<Juego>.chd)",
                    )
                    .changed()
                {
                    self.rematch_upload_queue();
                }

                ui.checkbox(&mut self.config.options.dry_run, "Simulación (Dry-Run)");
            });

            egui::CollapsingHeader::new(
                "⚙ Herramientas de conversión y extracción (CHD / ZIP / 7Z / RAR / Temp)",
            )
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let chd_badge = if let Some(ref chdman) = self.detected_chd_tools.chdman_path {
                        format!(
                            "CHD: chd-rs integrado + chdman ({})",
                            chdman.display()
                        )
                    } else if self.detected_chd_tools.builtin_available {
                        "CHD: chd-rs integrado (sin requerir chdman)".to_string()
                    } else {
                        "CHD: no disponible".to_string()
                    };
                    let mut arch_parts = vec!["ZIP/7Z/RAR nativo"];
                    if self.detected_archive_tools.seven_zip.is_some() {
                        arch_parts.push("7-Zip CLI");
                    }
                    if self.detected_archive_tools.unrar.is_some() {
                        arch_parts.push("UnRAR CLI");
                    }
                    if self.detected_archive_tools.bsdtar.is_some() {
                        arch_parts.push("bsdtar");
                    }
                    ui.small(format!(
                        "{chd_badge} · Comprimidos: {}",
                        arch_parts.join(" + ")
                    ));
                });

                ui.horizontal(|ui| {
                    ui.label("chdman opcional:");
                    if ui
                        .add_sized(
                            [(ui.available_width() - 260.0).max(120.0), 22.0],
                            egui::TextEdit::singleline(&mut self.config.upload_chdman_path)
                                .hint_text("Automático (se usa chd-rs integrado si se deja vacío)"),
                        )
                        .changed()
                    {
                        self.refresh_detected_chd_tools();
                    }
                    if ui.button("Elegir...").clicked()
                        && let Some(path) = FileDialog::new()
                            .add_filter("Ejecutable", &["exe"])
                            .pick_file()
                    {
                        self.config.upload_chdman_path = path.to_string_lossy().to_string();
                        self.refresh_detected_chd_tools();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Carpeta temporal opcional:");
                    ui.add_sized(
                        [(ui.available_width() - 260.0).max(120.0), 22.0],
                        egui::TextEdit::singleline(&mut self.config.upload_temp_dir)
                            .hint_text("Temporal del sistema (%TEMP%) — útil si C: tiene poco espacio"),
                    );
                    if ui.button("Elegir...").clicked()
                        && let Some(path) = FileDialog::new().pick_folder()
                    {
                        self.config.upload_temp_dir = path.to_string_lossy().to_string();
                    }
                });
            });
        });

        ui.separator();

        let ready_count = self
            .upload_items
            .iter()
            .filter(|i| i.selected && i.target_system.is_some())
            .count();
        let total_bytes = uploader::selected_total_bytes(&self.upload_items);

        ui.horizontal(|ui| {
            let verb_label = if self.config.options.dry_run {
                format!(
                    "▶ Simular Carga ({ready_count} juegos · {})",
                    uploader::format_bytes(total_bytes)
                )
            } else if self.config.storage_mode == StorageMode::Sftp {
                format!(
                    "🚀 Subir Juegos y Carátulas a MiSTer ({ready_count} · {})",
                    uploader::format_bytes(total_bytes)
                )
            } else {
                format!(
                    "🚀 Copiar Juegos y Carátulas ({ready_count} · {})",
                    uploader::format_bytes(total_bytes)
                )
            };

            let go_btn = egui::Button::new(verb_label).fill(egui::Color32::from_rgb(36, 128, 66));
            if ui.add_enabled(!busy && ready_count > 0, go_btn).clicked() {
                self.start_execute_upload(ctx.clone());
            }

            if busy {
                let cancel_btn =
                    egui::Button::new("⏹ Cancelar").fill(egui::Color32::from_rgb(160, 48, 48));
                if ui.add(cancel_btn).clicked() {
                    self.cancel_flag.store(true, Ordering::Relaxed);
                    self.status_text = "Cancelando...".to_string();
                }
            }
        });
    }

    fn render_bottom_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("bottom_status_panel").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let color = if self.status_is_error {
                    egui::Color32::from_rgb(245, 110, 110)
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, &self.status_text);
            });
            if let Some(frac) = self.progress_fraction {
                ui.add(egui::ProgressBar::new(frac).show_percentage());
            } else if self.is_busy() {
                ui.add(egui::ProgressBar::new(0.0).animate(true));
            }
            ui.add_space(3.0);
        });
    }

    fn render_right_inspector(&mut self, ui: &mut egui::Ui) {
        match self.active_tab {
            AppTab::CoverScraper => self.render_scraper_inspector(ui),
            AppTab::BulkUploader => self.render_uploader_inspector(ui),
            AppTab::Duplicates => self.render_duplicates_inspector(ui),
        }
    }

    fn render_duplicates_inspector(&mut self, ui: &mut egui::Ui) {
        egui::Panel::right("duplicates_inspector_panel")
            .resizable(true)
            .default_size(360.0)
            .min_size(280.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Duplicados y Limpieza");
                ui.separator();
                ui.label(
                    "Agrupa copias duplicadas por nombre normalizado y detecta archivos cosméticos mister-boot / mister-demo (uberyoji Boot ROMs).",
                );
                ui.add_space(6.0);
                if self.duplicate_groups.is_empty() && self.boot_demo_files.is_empty() {
                    ui.label("Pulsa \"Buscar duplicados y boot/demo\" para analizar la carpeta de juegos.");
                    return;
                }

                let selected_boot: Vec<String> = self
                    .boot_demo_files
                    .iter()
                    .filter(|item| self.boot_demo_selected.contains(&item.path))
                    .map(|item| item.path.clone())
                    .collect();

                let removable: Vec<String> = self
                    .duplicate_groups
                    .iter()
                    .enumerate()
                    .flat_map(|(group_idx, group)| {
                        let keep = self
                            .duplicate_keep
                            .get(&group_idx)
                            .copied()
                            .unwrap_or(group.keep_idx);
                        group
                            .games
                            .iter()
                            .enumerate()
                            .filter(|(game_idx, _)| *game_idx != keep)
                            .map(|(_, game)| game.path.clone())
                            .collect::<Vec<_>>()
                    })
                    .collect();

                ui.strong(format!(
                    "Boot/Demo marcados: {} / {}",
                    selected_boot.len(),
                    self.boot_demo_files.len()
                ));
                ui.strong(format!(
                    "Grupos duplicados: {} ({} copias a eliminar)",
                    self.duplicate_groups.len(),
                    removable.len()
                ));
                ui.separator();

                if selected_boot.is_empty() && removable.is_empty() {
                    ui.label("No hay archivos marcados para eliminar.");
                } else {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if !selected_boot.is_empty() {
                            ui.strong("Boot / Demo:");
                            for path in &selected_boot {
                                ui.small(path.as_str());
                            }
                            if !removable.is_empty() {
                                ui.add_space(6.0);
                            }
                        }
                        if !removable.is_empty() {
                            ui.strong("Duplicados:");
                            for path in &removable {
                                ui.small(path.as_str());
                            }
                        }
                    });
                }
                ui.add_space(6.0);
                let is_sftp = if !self.duplicate_groups.is_empty() {
                    self.duplicate_is_sftp
                } else {
                    self.boot_demo_is_sftp
                };
                if is_sftp {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 190, 75),
                        "SFTP: los archivos se eliminarán permanentemente.",
                    );
                } else {
                    ui.label(
                        "Local: los archivos se moverán a .duplicates_trash/<fecha>/ dentro de la carpeta de juegos.",
                    );
                }
                ui.label("Las BIOS reales (boot.rom–boot3.rom) nunca se eliminan.");
                ui.label("Al eliminar una hoja .cue/.ccd/.gdi también se eliminan sus pistas .bin/.img del mismo juego.");
            });
    }

    fn render_scraper_inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::right("right_inspector_panel")
            .resizable(true)
            .default_size(360.0)
            .min_size(280.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Inspector y Asignación Manual");
                ui.separator();

                let Some(item_idx) = self.selected_item_idx else {
                    ui.label("Selecciona un juego de la lista para ver su portada, sugerencias o asignarle otra imagen manualmente.");
                    return;
                };

                let Some(ref analysis) = self.analysis else {
                    ui.label("Ejecuta un análisis para ver detalles.");
                    return;
                };

                if item_idx >= analysis.items.len() {
                    return;
                }

                let item_snapshot = analysis.items[item_idx].clone();
                let is_sftp = analysis.is_sftp;
                let selected_cover = item_snapshot
                    .selected_cover_idx
                    .and_then(|idx| analysis.covers.get(idx).cloned());

                ui.strong(&item_snapshot.game.file_name);
                if let Some(sys) = item_snapshot.game.system {
                    ui.small(format!("Sistema detectado: {}", sys.display_name()));
                }
                if !item_snapshot.game.relative_dir.is_empty() {
                    ui.small(format!("Subcarpeta: {}", item_snapshot.game.relative_dir));
                }
                ui.small(format!(
                    "Origen: {} (.{})",
                    item_snapshot.game.full_path, item_snapshot.game.extension
                ));
                ui.small(format!("Destino: {}", item_snapshot.target_full_path));
                if let Some(ref existing) = item_snapshot.existing_cover_name {
                    ui.colored_label(
                        egui::Color32::from_rgb(130, 195, 255),
                        format!("Ya existe en media/: {existing}"),
                    );
                }
                if let MatchStatus::Failed(ref err) = item_snapshot.status {
                    ui.colored_label(egui::Color32::from_rgb(245, 110, 110), format!("Error: {err}"));
                }

                ui.separator();

                let mut new_assignment: Option<(Option<usize>, f64)> = None;
                let mut candidate_rows = Vec::new();
                if let Some(ref analysis_ref) = self.analysis {
                    for cand in &item_snapshot.candidates {
                        if let Some(cov) = analysis_ref.covers.get(cand.cover_idx) {
                            let sys_tag = cov
                                .system
                                .map(|s| format!("[{}] ", s.short_label()))
                                .unwrap_or_default();
                            candidate_rows.push((
                                cand.cover_idx,
                                format!("{sys_tag}{}", cov.file_name),
                                cand.score,
                                cand.kind,
                            ));
                        }
                    }
                }

                let query_words: Vec<String> = self
                    .manual_cover_search
                    .to_lowercase()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();

                let mut manual_hits = Vec::new();
                if let Some(ref analysis_ref) = self.analysis {
                    for (idx, cov) in analysis_ref.covers.iter().enumerate() {
                        let lower = cov.file_name.to_lowercase();
                        if query_words.iter().all(|w| lower.contains(w)) {
                            let sys_tag = cov
                                .system
                                .map(|s| format!("[{}] ", s.short_label()))
                                .unwrap_or_default();
                            manual_hits.push((idx, format!("{sys_tag}{}", cov.file_name)));
                            if manual_hits.len() >= 40 {
                                break;
                            }
                        }
                    }
                }

                egui::ScrollArea::vertical()
                    .id_salt("inspector_outer_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.render_cover_preview_box(ui, &ctx, selected_cover.as_ref());

                        if selected_cover.is_some()
                            && ui.button("❌ Quitar cover asignado").clicked()
                        {
                            new_assignment = Some((None, 0.0));
                        }

                        ui.separator();
                        ui.strong("Mejores sugerencias:");
                        if candidate_rows.is_empty() {
                            ui.small("No hay sugerencias automáticas cercanas.");
                        } else {
                            for (cov_idx, fname, score, kind) in &candidate_rows {
                                let is_current = item_snapshot.selected_cover_idx == Some(*cov_idx);
                                let label = format!(
                                    "{} [{:.0}% · {}] {}",
                                    if is_current { "✔" } else { "→" },
                                    score * 100.0,
                                    kind.label(),
                                    fname
                                );
                                if ui.selectable_label(is_current, label).clicked() {
                                    new_assignment = Some((Some(*cov_idx), *score));
                                }
                            }
                        }

                        ui.separator();
                        ui.strong("Buscar en todas las portadas:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.manual_cover_search)
                                .hint_text("Escribe parte del título...")
                                .desired_width(f32::INFINITY),
                        );

                        egui::ScrollArea::vertical()
                            .id_salt("manual_covers_scroll")
                            .max_height(180.0)
                            .show(ui, |ui| {
                                for (cov_idx, fname) in &manual_hits {
                                    let is_current =
                                        item_snapshot.selected_cover_idx == Some(*cov_idx);
                                    if ui.selectable_label(is_current, fname).clicked() {
                                        new_assignment = Some((Some(*cov_idx), 1.0));
                                    }
                                }
                            });
                    });

                if let Some((maybe_cov, score)) = new_assignment
                    && let Some(ref mut analysis_mut) = self.analysis
                    && let Some(item) = analysis_mut.items.get_mut(item_idx)
                {
                    match maybe_cov {
                        Some(cov_idx) => {
                            item.selected_cover_idx = Some(cov_idx);
                            item.score = score;
                            item.match_kind = Some(MatchKind::Manual);
                            item.status = MatchStatus::Matched;
                            item.selected_for_apply = true;
                        }
                        None => {
                            item.selected_cover_idx = None;
                            item.selected_for_apply = false;
                            item.status = if item.existing_cover_name.is_some()
                                && !self.config.options.overwrite_existing
                            {
                                MatchStatus::AlreadyExists
                            } else {
                                MatchStatus::Unmatched
                            };
                        }
                    }
                    sync::recompute_target_paths(
                        item,
                        &analysis_mut.covers,
                        self.config.options.convert_to_png,
                        is_sftp,
                    );
                }
            });
    }

    fn render_uploader_inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let busy = self.is_busy();

        egui::Panel::right("upload_inspector_panel")
            .resizable(true)
            .default_size(360.0)
            .min_size(280.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Inspector de Carga");
                ui.separator();

                let Some(item_idx) = self.upload_selected_idx else {
                    ui.label("Selecciona un juego o carpeta de la cola para editar su consola destino, ver los archivos (.bin/.cue) o asignar su carátula.");
                    return;
                };
                if item_idx >= self.upload_items.len() {
                    return;
                }

                let item_snap = self.upload_items[item_idx].clone();
                let selected_cover = item_snap
                    .selected_cover_idx
                    .and_then(|i| self.upload_covers.get(i).cloned());
                let is_sftp = self.config.storage_mode == StorageMode::Sftp;
                let cover_targets = uploader::cover_target_paths(
                    &item_snap,
                    &self.upload_covers,
                    self.config.options.convert_to_png,
                    is_sftp,
                );

                ui.strong(item_snap.payload.display_name());
                match &item_snap.payload {
                    UploadPayload::Archive {
                        format,
                        inspect_backend,
                        compressed_bytes,
                        uncompressed_bytes,
                        archive_name,
                        ..
                    } => {
                        ui.small(format!(
                            "Contenedor {} ({}): {} · Comprimido: {} → Extraído: {}",
                            format.label(),
                            inspect_backend,
                            archive_name,
                            uploader::format_bytes(*compressed_bytes),
                            uploader::format_bytes(*uncompressed_bytes)
                        ));
                    }
                    _ => {
                        ui.small(format!(
                            "Tamaño: {} · {} archivo(s)",
                            uploader::format_bytes(item_snap.payload.total_bytes()),
                            item_snap.payload.file_count()
                        ));
                    }
                }

                if item_snap.payload.can_convert_to_chd() {
                    let mut item_chd = item_snap.convert_to_chd;
                    let toggled = ui
                        .add_enabled(
                            !busy,
                            egui::Checkbox::new(
                                &mut item_chd,
                                "💿 Convertir este juego a .chd antes de subir",
                            ),
                        )
                        .changed();
                    if toggled {
                        let games_base = self.active_upload_games_base();
                        let opts = self.current_upload_options();
                        if let Some(item_mut) = self.upload_items.get_mut(item_idx) {
                            item_mut.convert_to_chd = item_chd;
                            item_mut.convert_to_chd_manual = true;
                            uploader::recompute_upload_item(
                                item_mut,
                                &self.upload_covers,
                                &games_base,
                                is_sftp,
                                &self.upload_existing_dirs,
                                &opts,
                            );
                        }
                        return;
                    }
                }

                // Target system selector
                let mut changed_sys = item_snap.target_system;
                let mut sys_modified = false;
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Consola destino:");
                        egui::ComboBox::from_id_salt("upload_item_sys_combo")
                            .selected_text(
                                changed_sys
                                    .map(|s| format!("{} ({})", s.short_label(), s.mister_folder_name()))
                                    .unwrap_or_else(|| "⚠ Seleccionar consola...".to_string()),
                            )
                            .show_ui(ui, |ui| {
                                for &sys in SystemId::ALL {
                                    if ui
                                        .selectable_value(
                                            &mut changed_sys,
                                            Some(sys),
                                            format!("{} — {}", sys.short_label(), sys.display_name()),
                                        )
                                        .changed()
                                    {
                                        sys_modified = true;
                                    }
                                }
                            });
                    });
                });

                if sys_modified {
                    let games_base = self.active_upload_games_base();
                    let opts = self.current_upload_options();
                    if let Some(item_mut) = self.upload_items.get_mut(item_idx) {
                        item_mut.target_system = changed_sys;
                        if item_mut.cover_match_kind != Some(MatchKind::Manual) {
                            item_mut.selected_cover_idx = None;
                        }
                        uploader::recompute_upload_item(
                            item_mut,
                            &self.upload_covers,
                            &games_base,
                            is_sftp,
                            &self.upload_existing_dirs,
                            &opts,
                        );
                    }
                    return;
                }

                if item_snap.target_game_path.is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 190, 75),
                        "Destino pendiente: asigna una consola o configura la carpeta raíz.",
                    );
                } else {
                    ui.small(format!("Destino juego: {}", item_snap.target_game_path));
                }
                if !cover_targets.is_empty() {
                    ui.small(format!("Destino cover: {}", cover_targets.join(", ")));
                }
                if let UploadStatus::Failed(ref err) = item_snap.status {
                    ui.colored_label(egui::Color32::from_rgb(245, 110, 110), format!("Error: {err}"));
                }

                ui.separator();

                let mut candidate_rows = Vec::new();
                for cand in &item_snap.cover_candidates {
                    if let Some(cov) = self.upload_covers.get(cand.cover_idx) {
                        let sys_tag = cov
                            .system
                            .map(|s| format!("[{}] ", s.short_label()))
                            .unwrap_or_default();
                        candidate_rows.push((
                            cand.cover_idx,
                            format!("{sys_tag}{}", cov.file_name),
                            cand.score,
                            cand.kind,
                        ));
                    }
                }

                let query_words: Vec<String> = self
                    .upload_manual_cover_search
                    .to_lowercase()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                let mut manual_hits = Vec::new();
                for (idx, cov) in self.upload_covers.iter().enumerate() {
                    let lower = cov.file_name.to_lowercase();
                    if query_words.iter().all(|w| lower.contains(w)) {
                        let sys_tag = cov
                            .system
                            .map(|s| format!("[{}] ", s.short_label()))
                            .unwrap_or_default();
                        manual_hits.push((idx, format!("{sys_tag}{}", cov.file_name)));
                        if manual_hits.len() >= 40 {
                            break;
                        }
                    }
                }

                let mut new_cover_assignment: Option<(Option<usize>, f64)> = None;
                let mut upload_cover_toggle = item_snap.upload_cover;

                egui::ScrollArea::vertical()
                    .id_salt("upload_inspector_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let UploadPayload::AtomicFolder { ref files, ref launch_stems, .. } =
                            item_snap.payload
                        {
                            ui.strong(format!("Archivos en la carpeta ({}):", files.len()));
                            egui::ScrollArea::vertical()
                                .id_salt("upload_folder_files_scroll")
                                .max_height(110.0)
                                .show(ui, |ui| {
                                    for ff in files {
                                        ui.small(format!(
                                            "• {} ({})",
                                            ff.rel_path,
                                            uploader::format_bytes(ff.size_bytes)
                                        ));
                                    }
                                });
                            if !launch_stems.is_empty() {
                                ui.small(format!(
                                    "Covers por nombre lanzable: {}",
                                    launch_stems.join(", ")
                                ));
                            }
                            ui.separator();
                        } else if let UploadPayload::Archive {
                            ref inner_files,
                            ref launch_stems,
                            ref convertible_sheets,
                            ..
                        } = item_snap.payload
                        {
                            ui.strong(format!(
                                "Contenido detectado ({} archivos):",
                                inner_files.len()
                            ));
                            egui::ScrollArea::vertical()
                                .id_salt("upload_archive_files_scroll")
                                .max_height(110.0)
                                .show(ui, |ui| {
                                    for entry in inner_files {
                                        ui.small(format!(
                                            "• {} ({})",
                                            entry.path,
                                            uploader::format_bytes(entry.size_bytes)
                                        ));
                                    }
                                });
                            if !convertible_sheets.is_empty() {
                                ui.small(format!(
                                    "Hojas de disco detectadas: {}",
                                    convertible_sheets.join(", ")
                                ));
                            }
                            if !launch_stems.is_empty() {
                                ui.small(format!(
                                    "Covers por nombre lanzable: {}",
                                    launch_stems.join(", ")
                                ));
                            }
                            ui.separator();
                        }

                        if selected_cover.is_some() {
                            ui.add_enabled(
                                !busy,
                                egui::Checkbox::new(&mut upload_cover_toggle, "Subir carátula a media/"),
                            );
                        }
                        self.render_cover_preview_box(ui, &ctx, selected_cover.as_ref());

                        if selected_cover.is_some()
                            && ui.add_enabled(!busy, egui::Button::new("❌ Quitar carátula")).clicked()
                        {
                            new_cover_assignment = Some((None, 0.0));
                        }

                        ui.separator();
                        ui.strong("Sugerencias de carátula:");
                        if candidate_rows.is_empty() {
                            ui.small(if self.upload_covers.is_empty() {
                                "Selecciona una carpeta de imágenes para emparejar portadas."
                            } else {
                                "Sin sugerencias automáticas para esta consola/título."
                            });
                        } else {
                            for (cov_idx, fname, score, kind) in &candidate_rows {
                                let is_current = item_snap.selected_cover_idx == Some(*cov_idx);
                                let label = format!(
                                    "{} [{:.0}% · {}] {}",
                                    if is_current { "✔" } else { "→" },
                                    score * 100.0,
                                    kind.label(),
                                    fname
                                );
                                if ui.add_enabled(!busy, egui::Button::selectable(is_current, label)).clicked() {
                                    new_cover_assignment = Some((Some(*cov_idx), *score));
                                }
                            }
                        }

                        if !self.upload_covers.is_empty() {
                            ui.separator();
                            ui.strong("Buscar en todas las portadas:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.upload_manual_cover_search)
                                    .hint_text("Escribe parte del título...")
                                    .desired_width(f32::INFINITY),
                            );
                            egui::ScrollArea::vertical()
                                .id_salt("upload_manual_covers_scroll")
                                .max_height(150.0)
                                .show(ui, |ui| {
                                    for (cov_idx, fname) in &manual_hits {
                                        let is_current =
                                            item_snap.selected_cover_idx == Some(*cov_idx);
                                        if ui
                                            .add_enabled(
                                                !busy,
                                                egui::Button::selectable(is_current, fname),
                                            )
                                            .clicked()
                                        {
                                            new_cover_assignment = Some((Some(*cov_idx), 1.0));
                                        }
                                    }
                                });
                        }
                    });

                if let Some(item_mut) = self.upload_items.get_mut(item_idx) {
                    item_mut.upload_cover = upload_cover_toggle;
                    if let Some((maybe_cov, score)) = new_cover_assignment {
                        match maybe_cov {
                            Some(cov_idx) => {
                                item_mut.selected_cover_idx = Some(cov_idx);
                                item_mut.cover_score = score;
                                item_mut.cover_match_kind = Some(MatchKind::Manual);
                                item_mut.upload_cover = true;
                            }
                            None => {
                                item_mut.selected_cover_idx = None;
                                item_mut.cover_match_kind = Some(MatchKind::Manual);
                                item_mut.upload_cover = false;
                            }
                        }
                    }
                }
            });
    }

    fn render_central_panel(&mut self, ui: &mut egui::Ui, hovering_files: bool) {
        match self.active_tab {
            AppTab::CoverScraper => self.render_scraper_central_panel(ui),
            AppTab::BulkUploader => self.render_uploader_central_panel(ui, hovering_files),
            AppTab::Duplicates => self.render_duplicates_central_panel(ui),
        }
    }

    fn render_duplicates_central_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            if self.duplicate_groups.is_empty() && self.boot_demo_files.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.heading("Busca duplicados y archivos mister-boot / mister-demo");
                        ui.add_space(8.0);
                        ui.label("• Detecta ROMs duplicados aunque cambie la región, el tag de disco o el formato.");
                        ui.label("• Detecta archivos cosméticos mister-boot / mister-demo (uberyoji Boot ROMs) sin tocar BIOS reales (boot.rom).");
                        ui.label("• Los archivos elegidos se mueven a la papelera (local) o se eliminan tras confirmar (SFTP).");
                        ui.label("• Las pistas .bin/.img de una hoja .cue eliminada también se limpian.");
                        ui.add_space(12.0);
                        ui.label("Pulsa \"🔍 Buscar duplicados y boot/demo\" arriba para empezar.");
                    });
                });
                return;
            }

            let busy = self.is_busy();
            let mut set_keep: Option<(usize, usize)> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                if !self.boot_demo_files.is_empty() {
                    ui.horizontal(|ui| {
                        ui.heading(format!(
                            "Archivos MiSTer Boot / Demo ({})",
                            self.boot_demo_files.len()
                        ));
                        ui.separator();
                        if ui.add_enabled(!busy, egui::Button::new("Marcar todos")).clicked() {
                            for item in &self.boot_demo_files {
                                self.boot_demo_selected.insert(item.path.clone());
                            }
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("Desmarcar todos"))
                            .clicked()
                        {
                            self.boot_demo_selected.clear();
                        }
                    });
                    ui.small(
                        "Pertenecen a uberyoji Boot ROMs (intros cosméticas). No son BIOS reales como boot.rom y es seguro eliminarlos.",
                    );
                    ui.add_space(4.0);
                    for item in &self.boot_demo_files {
                        let mut checked = self.boot_demo_selected.contains(&item.path);
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add_enabled(!busy, egui::Checkbox::without_text(&mut checked))
                                .changed()
                            {
                                if checked {
                                    self.boot_demo_selected.insert(item.path.clone());
                                } else {
                                    self.boot_demo_selected.remove(&item.path);
                                }
                            }
                            ui.label(item.file_name.as_str());
                            if let Some(ref system) = item.system {
                                ui.label(egui::RichText::new(system.as_str()).small());
                            }
                            ui.label(egui::RichText::new(item.path.as_str()).small());
                        });
                    }
                    ui.separator();
                }

                if self.duplicate_groups.is_empty() {
                    ui.label("No se encontraron juegos duplicados.");
                } else {
                    ui.horizontal(|ui| {
                        ui.heading("Juegos duplicados");
                        ui.separator();
                        ui.label(format!(
                            "{} grupo(s) · raíz: {}",
                            self.duplicate_groups.len(),
                            self.duplicate_root
                        ));
                    });
                    ui.label("Selecciona con el círculo qué copia quieres conservar. Las demás se eliminarán.");
                    ui.add_space(4.0);

                    for (group_idx, group) in self.duplicate_groups.iter().enumerate() {
                        let current = self
                            .duplicate_keep
                            .get(&group_idx)
                            .copied()
                            .unwrap_or(group.keep_idx);
                        ui.collapsing(
                            format!("{} · {} copias", group.key, group.games.len()),
                            |ui| {
                                for (game_idx, game) in group.games.iter().enumerate() {
                                    let recommended = game_idx == group.keep_idx;
                                    ui.horizontal_wrapped(|ui| {
                                        if ui.radio(current == game_idx, "").clicked() {
                                            set_keep = Some((group_idx, game_idx));
                                        }
                                        ui.label(game.file_name.as_str());
                                        if recommended {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(120, 210, 150),
                                                "(recomendada)",
                                            );
                                        }
                                        if let Some(ref system) = game.system {
                                            ui.label(egui::RichText::new(system.as_str()).small());
                                        }
                                        ui.label(egui::RichText::new(game.path.as_str()).small());
                                    });
                                }
                            },
                        );
                    }
                }
            });

            if let Some((group_idx, game_idx)) = set_keep {
                self.duplicate_keep.insert(group_idx, game_idx);
            }
        });
    }

    fn render_scraper_central_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(ref mut analysis) = self.analysis else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.heading("Escanea tus juegos y portadas para empezar");
                        ui.add_space(8.0);
                        ui.label("• Compara carpetas y subcarpetas de ROMs contra tu colección de imágenes.");
                        ui.label("• Crea la carpeta media/ dentro de la carpeta de juegos con los nombres exactos de cada ROM.");
                        ui.label("• Convierte automáticamente .jpg / .webp / .bmp a .png para ConsoleMode.");
                        ui.label("• Soporta acceso directo a tu MiSTer FPGA vía SFTP / SSH o carpetas locales / SMB.");
                    });
                });
                return;
            };

            let total = analysis.items.len();
            ui.small(format!(
                "Carpeta analizada: {} · Covers indexados: {}",
                analysis.games_root,
                analysis.covers.len()
            ));
            if let Some(ref warn) = analysis.system_mismatch_warning {
                ui.colored_label(egui::Color32::from_rgb(245, 190, 75), format!("⚠ {warn}"));
            }
            if !self.boot_demo_files.is_empty() && self.boot_demo_root == analysis.games_root {
                let boot_count = self.boot_demo_files.len();
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 190, 75),
                        format!(
                            "ℹ Se detectaron {boot_count} archivo(s) mister-boot / mister-demo (no son BIOS reales)."
                        ),
                    );
                    if ui
                        .add_enabled(
                            self.busy == BusyTask::None,
                            egui::Button::new("🧹 Revisar / eliminar..."),
                        )
                        .clicked()
                    {
                        self.boot_demo_prompt_from_scraper = true;
                        self.boot_demo_confirm_open = true;
                    }
                });
            }
            let exists_count = analysis
                .items
                .iter()
                .filter(|i| i.status == MatchStatus::AlreadyExists)
                .count();
            let ready_count = analysis
                .items
                .iter()
                .filter(|i| i.selected_for_apply)
                .count();
            let doubtful_count = analysis
                .items
                .iter()
                .filter(|i| i.status == MatchStatus::Matched && i.score < 0.90)
                .count();
            let unmatched_count = analysis
                .items
                .iter()
                .filter(|i| i.status == MatchStatus::Unmatched)
                .count();

            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.filter_tab, FilterTab::All, format!("Todos ({total})"));
                ui.selectable_value(
                    &mut self.filter_tab,
                    FilterTab::Ready,
                    format!("Listos ({ready_count})"),
                );
                ui.selectable_value(
                    &mut self.filter_tab,
                    FilterTab::Doubtful,
                    format!("Dudosos <90% ({doubtful_count})"),
                );
                ui.selectable_value(
                    &mut self.filter_tab,
                    FilterTab::Unmatched,
                    format!("Sin cover ({unmatched_count})"),
                );
                ui.selectable_value(
                    &mut self.filter_tab,
                    FilterTab::AlreadyExists,
                    format!("Ya existen ({exists_count})"),
                );

                ui.separator();
                ui.label("Filtrar:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .hint_text("Juego o subcarpeta...")
                        .desired_width(180.0),
                );

                if ui.button("Seleccionar listos").clicked() {
                    for item in &mut analysis.items {
                        if item.selected_cover_idx.is_some()
                            && (item.status == MatchStatus::Matched
                                || self.config.options.overwrite_existing)
                        {
                            item.selected_for_apply = true;
                        }
                    }
                }
                if ui.button("Deseleccionar todos").clicked() {
                    for item in &mut analysis.items {
                        item.selected_for_apply = false;
                    }
                }
            });

            ui.separator();

            let q_lower = self.search_query.to_lowercase();
            let filtered_indices: Vec<usize> = analysis
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    let tab_ok = match self.filter_tab {
                        FilterTab::All => true,
                        FilterTab::Ready => item.selected_for_apply,
                        FilterTab::Doubtful => {
                            item.status == MatchStatus::Matched && item.score < 0.90
                        }
                        FilterTab::Unmatched => item.status == MatchStatus::Unmatched,
                        FilterTab::AlreadyExists => item.status == MatchStatus::AlreadyExists,
                    };
                    if !tab_ok {
                        return false;
                    }
                    if q_lower.is_empty() {
                        return true;
                    }
                    item.game.file_name.to_lowercase().contains(&q_lower)
                        || item.game.relative_dir.to_lowercase().contains(&q_lower)
                })
                .map(|(idx, _)| idx)
                .collect();

            if filtered_indices.is_empty() {
                ui.label("No hay juegos que coincidan con el filtro actual.");
                return;
            }

            let row_height = 28.0;
            let num_rows = filtered_indices.len();
            let covers = &analysis.covers;

            egui::ScrollArea::vertical()
                .id_salt("games_list_scroll")
                .auto_shrink([false, false])
                .show_rows(ui, row_height, num_rows, |ui, row_range| {
                    for row in row_range {
                        let item_idx = filtered_indices[row];
                        let item = &mut analysis.items[item_idx];
                        let is_selected = self.selected_item_idx == Some(item_idx);

                        ui.horizontal(|ui| {
                            ui.set_height(row_height - 2.0);
                            let can_toggle = item.selected_cover_idx.is_some();
                            ui.add_enabled(
                                can_toggle && self.busy == BusyTask::None,
                                egui::Checkbox::without_text(&mut item.selected_for_apply),
                            );

                            let status_color = match item.status {
                                MatchStatus::AlreadyExists => egui::Color32::from_rgb(100, 170, 245),
                                MatchStatus::Matched => {
                                    if item.score >= 0.90 {
                                        egui::Color32::from_rgb(90, 200, 115)
                                    } else {
                                        egui::Color32::from_rgb(235, 190, 75)
                                    }
                                }
                                MatchStatus::Unmatched => egui::Color32::from_rgb(230, 110, 110),
                                MatchStatus::Applied => egui::Color32::from_rgb(70, 220, 170),
                                MatchStatus::Failed(_) => egui::Color32::from_rgb(255, 80, 80),
                            };
                            ui.colored_label(status_color, format!("[{}]", item.status.label()));

                            let cover_desc = match item.selected_cover_idx.and_then(|i| covers.get(i)) {
                                Some(cov) => format!(
                                    "{:.0}% · {}",
                                    item.score * 100.0,
                                    cov.file_name
                                ),
                                None => "—".to_string(),
                            };

                            let game_prefix = match (item.game.system, item.game.relative_dir.is_empty()) {
                                (Some(sys), true) => format!("[{}] ", sys.short_label()),
                                (Some(sys), false) => {
                                    format!("[{} · {}] ", sys.short_label(), item.game.relative_dir)
                                }
                                (None, false) => format!("[{}] ", item.game.relative_dir),
                                (None, true) => String::new(),
                            };

                            let row_text = format!("{game_prefix}{}   →   {cover_desc}", item.game.file_name);
                            if ui.selectable_label(is_selected, row_text).clicked() {
                                self.selected_item_idx = Some(item_idx);
                            }
                        });
                    }
                });
        });
    }

    fn render_uploader_central_panel(&mut self, ui: &mut egui::Ui, hovering_files: bool) {
        let busy = self.is_busy();
        egui::CentralPanel::default().show(ui, |ui| {
            if hovering_files {
                ui.colored_label(
                    egui::Color32::from_rgb(120, 210, 255),
                    "📥 Suelta aquí archivos (.gba, .zip, .7z, .rar, .chd...) o carpetas (.bin/.cue) para añadirlos a la cola.",
                );
                ui.separator();
            }

            if self.upload_items.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(50.0);
                        ui.heading("Carga masiva de juegos con enrutamiento, CHD y carátulas");
                        ui.add_space(10.0);
                        ui.label("• Arrastra y suelta aquí ROMs, archivos comprimidos (.zip, .7z, .rar) o carpetas (.cue + .bin).");
                        ui.label("• Inspecciona internamente los comprimidos, los extrae al vuelo y puede convertir BIN/CUE a .chd.");
                        ui.label("• Detecta automáticamente la consola (GBA, PSX, SNES, MegaDrive...) y crea las carpetas destino y media/.");
                        ui.label("• Empareja y convierte las carátulas faltantes a .png mientras transfiere los juegos en streaming.");
                    });
                });
                return;
            }

            let total = self.upload_items.len();
            let ready = self
                .upload_items
                .iter()
                .filter(|i| i.selected && i.target_system.is_some())
                .count();
            let needs_sys = self
                .upload_items
                .iter()
                .filter(|i| i.target_system.is_none())
                .count();
            let with_cov = self
                .upload_items
                .iter()
                .filter(|i| i.upload_cover && i.selected_cover_idx.is_some())
                .count();
            let sys_summary = uploader::summarize_queue_systems(&self.upload_items)
                .into_iter()
                .map(|(s, c)| format!("{s}: {c}"))
                .collect::<Vec<_>>()
                .join(" · ");

            ui.horizontal_wrapped(|ui| {
                ui.strong(format!(
                    "En cola: {total} | Listos: {ready} | Falta consola: {needs_sys} | Con carátula: {with_cov}"
                ));
                if !sys_summary.is_empty() {
                    ui.small(format!("({sys_summary})"));
                }
                ui.separator();
                if ui.add_enabled(!busy, egui::Button::new("Seleccionar listos")).clicked() {
                    for item in &mut self.upload_items {
                        if item.target_system.is_some() {
                            item.selected = true;
                        }
                    }
                }
                if ui.add_enabled(!busy, egui::Button::new("Deseleccionar todos")).clicked() {
                    for item in &mut self.upload_items {
                        item.selected = false;
                    }
                }
            });

            if self.active_upload_games_base().is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(245, 190, 75),
                    "⚠ Indica la carpeta raíz games/ de destino (ej. D:\\MiSTer\\games o /media/fat/games) para calcular las rutas.",
                );
            }

            ui.separator();

            let row_height = 28.0;
            let num_rows = self.upload_items.len();
            let mut remove_idx: Option<usize> = None;

            egui::ScrollArea::vertical()
                .id_salt("upload_queue_scroll")
                .auto_shrink([false, false])
                .show_rows(ui, row_height, num_rows, |ui, row_range| {
                    for item_idx in row_range {
                        let item = &mut self.upload_items[item_idx];
                        let is_selected = self.upload_selected_idx == Some(item_idx);

                        ui.horizontal(|ui| {
                            ui.set_height(row_height - 2.0);
                            let can_select = item.target_system.is_some();
                            ui.add_enabled(
                                can_select && !busy,
                                egui::Checkbox::without_text(&mut item.selected),
                            );

                            let status_color = match item.status {
                                UploadStatus::Ready => egui::Color32::from_rgb(90, 200, 115),
                                UploadStatus::NeedsSystem => egui::Color32::from_rgb(245, 190, 75),
                                UploadStatus::Uploading => egui::Color32::from_rgb(120, 210, 255),
                                UploadStatus::Done => egui::Color32::from_rgb(70, 220, 170),
                                UploadStatus::SkippedExists => egui::Color32::from_rgb(130, 180, 240),
                                UploadStatus::Failed(_) => egui::Color32::from_rgb(255, 85, 85),
                            };
                            ui.colored_label(status_color, format!("[{}]", item.status.label()));

                            let chd_badge = if item.convert_to_chd && item.payload.can_convert_to_chd() {
                                " → CHD"
                            } else {
                                ""
                            };
                            let kind_badge = if let Some(fmt) = item.payload.archive_format() {
                                format!("🗜 {}{chd_badge}", fmt.label())
                            } else {
                                match &item.payload {
                                    UploadPayload::SingleFile { .. } => "📄".to_string(),
                                    UploadPayload::AtomicFolder { files, .. } => {
                                        format!("📁({}){chd_badge}", files.len())
                                    }
                                    UploadPayload::Archive { .. } => format!("🗜{chd_badge}"),
                                }
                            };
                            let sys_dest = item
                                .target_system
                                .map(|s| {
                                    uploader::target_folder_name(s, &self.upload_existing_dirs)
                                })
                                .unwrap_or_else(|| "SIN CONSOLA".to_string());
                            let cover_desc = if !item.upload_cover {
                                "Sin cover".to_string()
                            } else if let Some(cov) = item
                                .selected_cover_idx
                                .and_then(|i| self.upload_covers.get(i))
                            {
                                format!("{:.0}% · {}", item.cover_score * 100.0, cov.file_name)
                            } else {
                                "Sin cover".to_string()
                            };

                            let label = format!(
                                "{kind_badge} {} ({})  →  [{sys_dest}]  |  Cover: {cover_desc}",
                                item.payload.display_name(),
                                uploader::format_bytes(item.payload.total_bytes())
                            );
                            if ui.selectable_label(is_selected, label).clicked() {
                                self.upload_selected_idx = Some(item_idx);
                            }

                            if ui.add_enabled(!busy, egui::Button::new("✕")).clicked() {
                                remove_idx = Some(item_idx);
                            }
                        });
                    }
                });

            if let Some(idx) = remove_idx {
                self.upload_items.remove(idx);
                if self.upload_selected_idx == Some(idx) {
                    self.upload_selected_idx = if self.upload_items.is_empty() {
                        None
                    } else {
                        Some(idx.min(self.upload_items.len() - 1))
                    };
                } else if let Some(sel) = self.upload_selected_idx
                    && sel > idx
                {
                    self.upload_selected_idx = Some(sel - 1);
                }
            }
        });
    }

    fn render_duplicates_confirm_window(&mut self, ui: &mut egui::Ui) {
        if !self.duplicate_confirm_open {
            return;
        }
        let mut open = self.duplicate_confirm_open;
        let mut accept = false;
        let mut close_requested = false;

        egui::Window::new("Confirmar eliminación de duplicados")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_size([520.0, 320.0])
            .show(ui, |ui| {
                let removable: Vec<String> = self
                    .duplicate_groups
                    .iter()
                    .enumerate()
                    .flat_map(|(group_idx, group)| {
                        let keep = self
                            .duplicate_keep
                            .get(&group_idx)
                            .copied()
                            .unwrap_or(group.keep_idx);
                        group
                            .games
                            .iter()
                            .enumerate()
                            .filter(|(game_idx, _)| *game_idx != keep)
                            .map(|(_, game)| game.path.clone())
                            .collect::<Vec<_>>()
                    })
                    .collect();

                ui.label(format!("Se procesarán {} copia(s):", removable.len()));
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for path in &removable {
                            ui.small(path.as_str());
                        }
                    });
                ui.separator();

                if self.config.options.dry_run {
                    ui.label("Modo simulación: no se borrará ningún archivo.");
                } else if self.duplicate_is_sftp {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 190, 75),
                        "SFTP: los archivos se eliminarán permanentemente.",
                    );
                } else {
                    ui.label("Los archivos se moverán a una papelera recuperable.");
                }
                ui.label("Las pistas de disco del mismo juego también se eliminarán.");

                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        close_requested = true;
                    }
                    let label = if self.config.options.dry_run {
                        "Simular"
                    } else {
                        "Eliminar"
                    };
                    let confirm =
                        egui::Button::new(label).fill(egui::Color32::from_rgb(160, 48, 48));
                    if ui.add(confirm).clicked() {
                        accept = true;
                    }
                });
            });

        self.duplicate_confirm_open = open && !close_requested;
        if accept {
            let ctx = ui.ctx().clone();
            self.start_delete_duplicates(ctx);
        }
    }

    fn render_boot_demo_confirm_window(&mut self, ui: &mut egui::Ui) {
        if !self.boot_demo_confirm_open || self.boot_demo_files.is_empty() {
            return;
        }
        let mut open = self.boot_demo_confirm_open;
        let mut accept = false;
        let mut close_requested = false;
        let mut select_all = false;
        let mut clear_all = false;

        let title = if self.boot_demo_prompt_from_scraper {
            "Archivos mister-boot / mister-demo detectados"
        } else {
            "Confirmar eliminación de mister-boot / mister-demo"
        };

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 380.0])
            .show(ui, |ui| {
                ui.label(format!(
                    "Se encontraron {} archivo(s) cosméticos de uberyoji Boot ROMs en '{}'. ¿Deseas eliminarlos?",
                    self.boot_demo_files.len(),
                    self.boot_demo_root
                ));
                ui.colored_label(
                    egui::Color32::from_rgb(120, 210, 150),
                    "Seguro de borrar: las BIOS reales (boot.rom, boot0.rom–boot3.rom, scph*.bin) nunca se incluyen.",
                );
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    if ui.button("Marcar todos").clicked() {
                        select_all = true;
                    }
                    if ui.button("Desmarcar todos").clicked() {
                        clear_all = true;
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(190.0)
                    .show(ui, |ui| {
                        for item in &self.boot_demo_files {
                            let mut checked = self.boot_demo_selected.contains(&item.path);
                            if ui
                                .checkbox(&mut checked, item.path.as_str())
                                .changed()
                            {
                                if checked {
                                    self.boot_demo_selected.insert(item.path.clone());
                                } else {
                                    self.boot_demo_selected.remove(&item.path);
                                }
                            }
                        }
                    });
                ui.separator();

                if self.config.options.dry_run {
                    ui.label("Modo simulación: no se borrará ningún archivo.");
                } else if self.boot_demo_is_sftp {
                    ui.colored_label(
                        egui::Color32::from_rgb(245, 190, 75),
                        "SFTP: los archivos seleccionados se eliminarán permanentemente.",
                    );
                } else {
                    ui.label("Local: los archivos se moverán a .duplicates_trash/<fecha>/ (papelera recuperable).");
                }
                ui.small("Las pistas .bin/.img asociadas a una hoja .cue/.ccd/.gdi también se limpiarán.");
                ui.small("Consejo: desactiva 'uberyoji Boot ROMS' en update_all (Extra Content) para que no vuelvan a descargarse.");
                ui.add_space(6.0);

                let selected_count = self
                    .boot_demo_files
                    .iter()
                    .filter(|item| self.boot_demo_selected.contains(&item.path))
                    .count();

                ui.horizontal(|ui| {
                    if ui.button("Mantener (no eliminar)").clicked() {
                        close_requested = true;
                    }
                    let label = if self.config.options.dry_run {
                        format!("Simular ({selected_count})")
                    } else {
                        format!("Eliminar seleccionados ({selected_count})")
                    };
                    let confirm =
                        egui::Button::new(label).fill(egui::Color32::from_rgb(160, 48, 48));
                    if ui.add_enabled(selected_count > 0, confirm).clicked() {
                        accept = true;
                    }
                });
            });

        if select_all {
            for item in &self.boot_demo_files {
                self.boot_demo_selected.insert(item.path.clone());
            }
        } else if clear_all {
            self.boot_demo_selected.clear();
        }

        self.boot_demo_confirm_open = open && !close_requested;
        if accept {
            let ctx = ui.ctx().clone();
            self.start_delete_boot_demos(ctx);
        }
    }

    fn render_discovery_window(&mut self, ui: &mut egui::Ui) {
        if !self.discovery_open {
            return;
        }
        let mut open = self.discovery_open;
        let mut selected: Option<String> = None;

        egui::Window::new("Buscar MiSTer en la red local")
            .open(&mut open)
            .default_size([580.0, 400.0])
            .resizable(true)
            .show(ui, |ui| {
                if self.busy == BusyTask::Discovering {
                    let (scanned, total) = self.discovery_scanned;
                    ui.label(format!("Escaneando la red... {scanned}/{total} hosts"));
                    if total > 0 {
                        ui.add(
                            egui::ProgressBar::new(scanned as f32 / total as f32)
                                .show_percentage(),
                        );
                    }
                    return;
                }

                if let Some(ref err) = self.discovery_error {
                    ui.colored_label(egui::Color32::from_rgb(245, 110, 110), err);
                }

                if self.discovery_results.is_empty() {
                    ui.label("No se encontraron candidatos MiSTer en la red local.");
                } else {
                    ui.label("Selecciona la MiSTer que quieras usar:");
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for host in &self.discovery_results {
                                let badge = if host.verified { "✅" } else { "📡" };
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(format!(
                                        "{badge} {}:{} · {} ms",
                                        host.ip, host.port, host.latency_ms
                                    ));
                                    if ui.button("Usar esta IP").clicked() {
                                        selected = Some(host.ip.clone());
                                    }
                                });
                                if let Some(ref banner) = host.ssh_banner {
                                    ui.label(egui::RichText::new(format!("SSH: {banner}")).small());
                                }
                                if let Some(ref note) = host.verification_note {
                                    ui.label(egui::RichText::new(note.as_str()).small());
                                }
                                ui.separator();
                            }
                        });
                }
            });

        self.discovery_open = open;

        if let Some(ip) = selected {
            self.config.sftp.host = ip.clone();
            self.persist_config();
            self.discovery_open = false;
            self.status_text =
                format!("MiSTer seleccionada: {ip}. Pulsa \"Probar conexión\" para validar.");
            self.status_is_error = false;
        }
    }

    fn render_remote_browser_window(&mut self, ui: &mut egui::Ui) {
        if !self.browser_open {
            return;
        }
        let ctx = ui.ctx().clone();
        let busy = self.is_busy();
        let mut open = self.browser_open;
        let mut navigate_to: Option<String> = None;
        let mut select_and_close = false;

        egui::Window::new("Explorar carpetas en MiSTer (SFTP)")
            .open(&mut open)
            .default_size([540.0, 440.0])
            .resizable(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Atajos:");
                    for shortcut in [
                        "/media/fat/games",
                        "/media/usb0/games",
                        "/media/usb1/games",
                        "/",
                    ] {
                        if ui.add_enabled(!busy, egui::Button::new(shortcut)).clicked() {
                            navigate_to = Some(shortcut.to_string());
                        }
                    }
                });

                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("⬆ Subir (..)")).clicked() {
                        navigate_to = Some(posix_parent(&self.browser_path));
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.browser_path)
                            .desired_width((ui.available_width() - 60.0).max(120.0)),
                    );
                    if ui.add_enabled(!busy, egui::Button::new("Ir")).clicked() {
                        navigate_to = Some(self.browser_path.clone());
                    }
                });

                if let Some(ref err) = self.browser_error {
                    ui.colored_label(egui::Color32::from_rgb(245, 110, 110), err);
                }

                ui.separator();

                if self.busy == BusyTask::BrowsingRemote {
                    ui.label("Cargando subcarpetas remotas...");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            if self.browser_dirs.is_empty() {
                                ui.label("(No hay subcarpetas en esta ruta)");
                            } else {
                                for dir in &self.browser_dirs {
                                    if ui.button(format!("📁 {dir}")).clicked() {
                                        navigate_to = Some(posix_join(&self.browser_path, dir));
                                    }
                                }
                            }
                        });
                }

                ui.separator();
                ui.horizontal(|ui| {
                    let confirm_btn = egui::Button::new("Seleccionar esta carpeta")
                        .fill(egui::Color32::from_rgb(36, 128, 66));
                    if ui.add_enabled(!busy, confirm_btn).clicked() {
                        select_and_close = true;
                    }
                });
            });

        self.browser_open = open;

        if select_and_close {
            let norm = posix_normalize(&self.browser_path);
            match self.active_tab {
                AppTab::CoverScraper | AppTab::Duplicates => self.config.sftp.remote_path = norm,
                AppTab::BulkUploader => {
                    self.config.upload_sftp_games_base = norm;
                    self.upload_existing_dirs.clear();
                    self.rematch_upload_queue();
                }
            }
            self.persist_config();
            self.browser_open = false;
        } else if let Some(target) = navigate_to {
            self.start_browse_remote(target, ctx);
        }
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        self.persist_config();
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_worker_messages();

        let (dropped_paths, hovering_files) = ui.input(|i| {
            let paths: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            (paths, !i.raw.hovered_files.is_empty())
        });

        if !dropped_paths.is_empty() && !self.is_busy() {
            self.active_tab = AppTab::BulkUploader;
            self.start_stage_uploads(dropped_paths, false, ui.ctx().clone());
        }

        self.render_top_panel(ui);
        self.render_bottom_panel(ui);
        self.render_right_inspector(ui);
        self.render_central_panel(ui, hovering_files);
        self.render_remote_browser_window(ui);
        self.render_discovery_window(ui);
        self.render_duplicates_confirm_window(ui);
        self.render_boot_demo_confirm_window(ui);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist_config();
    }
}
