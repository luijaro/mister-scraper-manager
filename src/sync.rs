use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

use crate::config::{MediaPlacement, SyncOptions};
use crate::imaging;
use crate::matcher::CoverIndex;
use crate::models::{
    AnalysisResult, CoverImage, GameEntry, MatchItem, MatchKind, MatchStatus, SyncSummary,
};
use crate::scanner;
use crate::storage::{StorageClient, join_storage_path};
use crate::systems::SystemId;

/// Progress events emitted while copying or uploading matched covers.
#[derive(Debug, Clone)]
pub enum ApplyEvent {
    ItemStarting {
        current: usize,
        total: usize,
        file_name: String,
    },
    ItemFinished {
        item_idx: usize,
        result: std::result::Result<(), String>,
    },
}

/// Scans games (Local or SFTP) and local covers, computes match candidates, and checks `media/`.
pub async fn analyze<F>(
    storage: &StorageClient,
    games_root: &str,
    images_dir: &Path,
    options: &SyncOptions,
    cancel: &AtomicBool,
    on_progress: F,
) -> Result<AnalysisResult>
where
    F: Fn(String) + Send + Sync,
{
    if cancel.load(Ordering::Relaxed) {
        bail!("Operación cancelada por el usuario.");
    }

    on_progress("Escaneando carpeta de imágenes...".to_string());
    let images_dir_buf = images_dir.to_path_buf();
    let recursive = options.recursive_images;
    let covers = tokio::task::spawn_blocking(move || scanner::scan_local_covers(&images_dir_buf, recursive))
        .await??;

    if cancel.load(Ordering::Relaxed) {
        bail!("Operación cancelada por el usuario.");
    }

    on_progress(format!("Indexando {} imágenes...", covers.len()));
    let index = CoverIndex::new(&covers);

    on_progress("Escaneando carpeta de juegos...".to_string());
    let games = storage
        .scan_games(games_root, cancel, |dir| {
            on_progress(format!("Escaneando: {dir}"));
        })
        .await?;

    let (boot_demo_files, games): (Vec<GameEntry>, Vec<GameEntry>) = games
        .into_iter()
        .partition(|game| scanner::is_mister_boot_or_demo_stem(&game.stem));

    let is_sftp = storage.is_sftp();
    let mut existing_cache: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut seen_targets: HashSet<String> = HashSet::new();
    let mut items = Vec::with_capacity(games.len());

    on_progress("Comparando juegos con portadas...".to_string());
    for game in games {
        if cancel.load(Ordering::Relaxed) {
            bail!("Operación cancelada por el usuario.");
        }

        let target_media_dir = match options.media_placement {
            MediaPlacement::RootMedia => join_storage_path(is_sftp, games_root, "media"),
            MediaPlacement::PerSubfolderMedia => {
                join_storage_path(is_sftp, &game.parent_dir, "media")
            }
        };

        if !existing_cache.contains_key(&target_media_dir) {
            let map = storage.list_existing_media(&target_media_dir).await?;
            existing_cache.insert(target_media_dir.clone(), map);
        }

        let existing_cover_name = existing_cache
            .get(&target_media_dir)
            .and_then(|m| m.get(&game.stem.to_lowercase()))
            .cloned();

        let mut item = MatchItem {
            game,
            target_media_dir,
            target_file_name: String::new(),
            target_full_path: String::new(),
            existing_cover_name,
            selected_cover_idx: None,
            score: 0.0,
            match_kind: None,
            candidates: Vec::new(),
            status: MatchStatus::Unmatched,
            selected_for_apply: false,
        };
        apply_match_decision(
            &mut item,
            &covers,
            &index,
            options,
            is_sftp,
            &mut seen_targets,
        );
        items.push(item);
    }

    let system_mismatch_warning =
        compute_system_mismatch_warning(&items, &covers, options.strict_system_match);

    Ok(AnalysisResult {
        games_root: games_root.to_string(),
        is_sftp,
        covers,
        items,
        boot_demo_files,
        system_mismatch_warning,
    })
}

/// Re-evaluates matching candidates, threshold, overwrite, and system strictness in memory
/// without re-scanning disk or SFTP.
pub fn rematch_analysis(analysis: &mut AnalysisResult, options: &SyncOptions) {
    let index = CoverIndex::new(&analysis.covers);
    let mut seen_targets = HashSet::new();

    for item in &mut analysis.items {
        if item.status == MatchStatus::Applied {
            let _ = seen_targets.insert(item.target_full_path.to_lowercase());
            continue;
        }
        if item.match_kind == Some(MatchKind::Manual) && item.selected_cover_idx.is_some() {
            item.candidates = index.match_game(
                &item.game.stem,
                item.game.system,
                options.strict_system_match,
            );
            if item.existing_cover_name.is_some() && !options.overwrite_existing {
                item.status = MatchStatus::AlreadyExists;
                item.selected_for_apply = false;
            } else {
                item.status = MatchStatus::Matched;
                recompute_target_paths(
                    item,
                    &analysis.covers,
                    options.convert_to_png,
                    analysis.is_sftp,
                );
                item.selected_for_apply =
                    seen_targets.insert(item.target_full_path.to_lowercase());
            }
            continue;
        }

        apply_match_decision(
            item,
            &analysis.covers,
            &index,
            options,
            analysis.is_sftp,
            &mut seen_targets,
        );
    }

    analysis.system_mismatch_warning = compute_system_mismatch_warning(
        &analysis.items,
        &analysis.covers,
        options.strict_system_match,
    );
}

fn apply_match_decision(
    item: &mut MatchItem,
    covers: &[CoverImage],
    index: &CoverIndex,
    options: &SyncOptions,
    is_sftp: bool,
    seen_targets: &mut HashSet<String>,
) {
    let candidates = index.match_game(
        &item.game.stem,
        item.game.system,
        options.strict_system_match,
    );
    let best = candidates.first().filter(|c| c.score >= options.threshold);

    if item.existing_cover_name.is_some() && !options.overwrite_existing {
        item.selected_cover_idx = best.map(|b| b.cover_idx);
        item.score = best.map(|b| b.score).unwrap_or(0.0);
        item.match_kind = best.map(|b| b.kind);
        item.status = MatchStatus::AlreadyExists;
        item.selected_for_apply = false;
    } else if let Some(b) = best {
        item.selected_cover_idx = Some(b.cover_idx);
        item.score = b.score;
        item.match_kind = Some(b.kind);
        item.status = MatchStatus::Matched;
        item.selected_for_apply = true;
    } else {
        item.selected_cover_idx = None;
        item.score = candidates.first().map(|c| c.score).unwrap_or(0.0);
        item.match_kind = candidates.first().map(|c| c.kind);
        item.status = MatchStatus::Unmatched;
        item.selected_for_apply = false;
    }

    item.candidates = candidates;
    recompute_target_paths(item, covers, options.convert_to_png, is_sftp);

    if item.selected_for_apply {
        let dedup_key = item.target_full_path.to_lowercase();
        if !seen_targets.insert(dedup_key) {
            item.selected_for_apply = false;
        }
    }
}

fn compute_system_mismatch_warning(
    items: &[MatchItem],
    covers: &[CoverImage],
    strict_system_match: bool,
) -> Option<String> {
    if !strict_system_match {
        return None;
    }
    let game_systems: BTreeSet<SystemId> = items.iter().filter_map(|i| i.game.system).collect();
    let cover_systems: BTreeSet<SystemId> = covers.iter().filter_map(|c| c.system).collect();
    if game_systems.is_empty() || cover_systems.is_empty() {
        return None;
    }

    let missing_game_systems: Vec<&str> = game_systems
        .iter()
        .filter(|sys| !cover_systems.contains(sys))
        .map(|sys| sys.short_label())
        .collect();
    if missing_game_systems.is_empty() {
        return None;
    }

    let found_cover_systems: Vec<&str> = cover_systems
        .iter()
        .map(|sys| sys.short_label())
        .collect();

    Some(format!(
        "Aviso de sistema: se detectaron juegos de [{}] pero ninguna portada de ese sistema en la carpeta de imágenes (solo se detectaron portadas de: {}). Revisa la carpeta de imágenes o desactiva \"Solo misma consola\".",
        missing_game_systems.join(", "),
        found_cover_systems.join(", ")
    ))
}

/// Recomputes `target_file_name` and `target_full_path` after cover selection or format changes.
pub fn recompute_target_paths(
    item: &mut MatchItem,
    covers: &[CoverImage],
    convert_to_png: bool,
    is_sftp: bool,
) {
    let ext = if convert_to_png {
        "png"
    } else {
        item.selected_cover_idx
            .and_then(|idx| covers.get(idx))
            .map(|c| c.extension.as_str())
            .unwrap_or("png")
    };
    item.target_file_name = format!("{}.{}", item.game.stem, ext);
    item.target_full_path =
        join_storage_path(is_sftp, &item.target_media_dir, &item.target_file_name);
}

/// Copies/converts/uploads all selected cover assignments into the target `media/` folder(s).
pub async fn apply<F>(
    storage: &StorageClient,
    items: &[MatchItem],
    covers: &[CoverImage],
    options: &SyncOptions,
    cancel: &AtomicBool,
    on_event: F,
) -> Result<SyncSummary>
where
    F: Fn(ApplyEvent) + Send + Sync,
{
    let mut summary = SyncSummary {
        total_games: items.len(),
        ..Default::default()
    };

    let mut work_indices = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if item.selected_for_apply && item.selected_cover_idx.is_some() {
            work_indices.push(idx);
        } else if item.status == MatchStatus::AlreadyExists {
            summary.already_exists += 1;
        } else {
            summary.skipped += 1;
        }
    }

    let total = work_indices.len();
    if total == 0 {
        return Ok(summary);
    }

    if options.dry_run {
        for (pos, item_idx) in work_indices.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("Operación cancelada por el usuario.");
            }
            let item = &items[item_idx];
            on_event(ApplyEvent::ItemStarting {
                current: pos + 1,
                total,
                file_name: item.target_file_name.clone(),
            });
            summary.copied += 1;
            on_event(ApplyEvent::ItemFinished {
                item_idx,
                result: Ok(()),
            });
        }
        return Ok(summary);
    }

    // Create each required target `media/` folder once before writing files.
    let mut created_dirs = HashSet::new();
    for &item_idx in &work_indices {
        let dir = &items[item_idx].target_media_dir;
        if created_dirs.insert(dir.clone()) {
            storage.ensure_dir(dir).await?;
        }
    }

    for (pos, item_idx) in work_indices.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Operación cancelada por el usuario.");
        }

        let item = &items[item_idx];
        let Some(cover_idx) = item.selected_cover_idx else {
            continue;
        };
        let Some(cover) = covers.get(cover_idx) else {
            let msg = format!("Índice de cover inválido para {}", item.game.file_name);
            summary.failed += 1;
            summary.errors.push((item.game.file_name.clone(), msg.clone()));
            on_event(ApplyEvent::ItemFinished {
                item_idx,
                result: Err(msg),
            });
            continue;
        };

        on_event(ApplyEvent::ItemStarting {
            current: pos + 1,
            total,
            file_name: item.target_file_name.clone(),
        });

        let src_path = cover.path.clone();
        let convert_to_png = options.convert_to_png;
        let prep_result = tokio::task::spawn_blocking(move || {
            imaging::prepare_cover_bytes(&src_path, convert_to_png)
        })
        .await;

        let write_res: anyhow::Result<()> = match prep_result {
            Ok(Ok((bytes, _ext))) => storage.write_file(&item.target_full_path, &bytes).await,
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::anyhow!("Error en tarea de conversión: {e}")),
        };

        match write_res {
            Ok(()) => {
                summary.copied += 1;
                on_event(ApplyEvent::ItemFinished {
                    item_idx,
                    result: Ok(()),
                });
            }
            Err(err) => {
                let msg = format!("{err:#}");
                summary.failed += 1;
                summary
                    .errors
                    .push((item.game.file_name.clone(), msg.clone()));
                on_event(ApplyEvent::ItemFinished {
                    item_idx,
                    result: Err(msg),
                });
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use image::{DynamicImage, ImageFormat, RgbImage};
    use tempfile::tempdir;

    use super::*;

    fn write_test_png(path: &Path) {
        let rgb = RgbImage::from_fn(8, 8, |_, _| image::Rgb([10, 200, 50]));
        DynamicImage::ImageRgb8(rgb)
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    fn write_test_jpg(path: &Path) {
        let rgb = RgbImage::from_fn(8, 8, |_, _| image::Rgb([220, 40, 40]));
        DynamicImage::ImageRgb8(rgb)
            .save_with_format(path, ImageFormat::Jpeg)
            .unwrap();
    }

    #[tokio::test]
    async fn end_to_end_local_psx_sync_and_idempotency() {
        let root = tempdir().unwrap();
        let psx_dir = root.path().join("PSX");
        let covers_dir = root.path().join("sony playstation");
        fs::create_dir_all(psx_dir.join("RPG")).unwrap();
        fs::create_dir_all(psx_dir.join("Castlevania (USA)")).unwrap();
        fs::create_dir_all(covers_dir.join("Named_Boxarts")).unwrap();

        // Games: loose .chd, subfolder .chd, subfolder .cue + .bin tracks, BIOS
        fs::write(psx_dir.join("Crash Bandicoot (USA).chd"), b"rom1").unwrap();
        fs::write(
            psx_dir.join("RPG").join("Final Fantasy VII (USA) (Disc 1).chd"),
            b"rom2",
        )
        .unwrap();
        fs::write(
            psx_dir
                .join("Castlevania (USA)")
                .join("Castlevania - Symphony of the Night (USA).cue"),
            b"FILE",
        )
        .unwrap();
        fs::write(
            psx_dir
                .join("Castlevania (USA)")
                .join("Castlevania - Symphony of the Night (USA) (Track 1).bin"),
            b"track1",
        )
        .unwrap();
        fs::write(psx_dir.join("boot.rom"), b"bios").unwrap();
        fs::write(psx_dir.join("Unmatched Oddity (USA).chd"), b"rom3").unwrap();

        // Covers: JPG, PNG, Libretro underscore naming
        write_test_jpg(&covers_dir.join("Crash Bandicoot (USA).jpg"));
        write_test_png(
            &covers_dir
                .join("Named_Boxarts")
                .join("Final Fantasy VII (USA).png"),
        );
        write_test_png(&covers_dir.join("Castlevania _ Symphony of the Night (USA).png"));

        let storage = StorageClient::Local;
        let cancel = AtomicBool::new(false);
        let games_str = psx_dir.to_string_lossy().to_string();

        // 1) Dry run must NOT create media folder
        let dry_opts = SyncOptions {
            dry_run: true,
            ..Default::default()
        };
        let dry_analysis = analyze(&storage, &games_str, &covers_dir, &dry_opts, &cancel, |_| {})
            .await
            .unwrap();

        assert_eq!(dry_analysis.items.len(), 4);
        let ready_count = dry_analysis
            .items
            .iter()
            .filter(|i| i.selected_for_apply)
            .count();
        assert_eq!(ready_count, 3);

        let dry_summary = apply(
            &storage,
            &dry_analysis.items,
            &dry_analysis.covers,
            &dry_opts,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(dry_summary.copied, 3);
        assert!(!psx_dir.join("media").exists());

        // 2) Real apply creates PSX/media and converts JPG -> PNG
        let real_opts = SyncOptions::default();
        let summary = apply(
            &storage,
            &dry_analysis.items,
            &dry_analysis.covers,
            &real_opts,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(summary.copied, 3);
        assert_eq!(summary.failed, 0);

        let media_dir: PathBuf = psx_dir.join("media");
        assert!(media_dir.is_dir());
        let crash_cover = media_dir.join("Crash Bandicoot (USA).png");
        let ff7_cover = media_dir.join("Final Fantasy VII (USA) (Disc 1).png");
        let sotn_cover = media_dir.join("Castlevania - Symphony of the Night (USA).png");
        assert!(crash_cover.is_file());
        assert!(ff7_cover.is_file());
        assert!(sotn_cover.is_file());
        assert!(fs::read(&crash_cover).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));

        // Track .bin and BIOS must have NO covers
        assert!(!media_dir.join("boot.png").exists());
        assert!(
            !media_dir
                .join("Castlevania - Symphony of the Night (USA) (Track 1).png")
                .exists()
        );

        // 3) Re-analyzing detects newly written covers as AlreadyExists
        let second_analysis = analyze(
            &storage,
            &games_str,
            &covers_dir,
            &real_opts,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            second_analysis
                .items
                .iter()
                .filter(|i| i.status == MatchStatus::AlreadyExists)
                .count(),
            3
        );
        assert_eq!(
            second_analysis
                .items
                .iter()
                .filter(|i| i.selected_for_apply)
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn gba_game_does_not_receive_psx_cover_with_same_name() {
        let root = tempdir().unwrap();
        let gba_dir = root.path().join("GBA");
        let covers_root = root.path().join("Covers");
        let psx_covers = covers_root.join("Sony - PlayStation");
        let gba_covers = covers_root.join("Nintendo - Game Boy Advance");
        fs::create_dir_all(&gba_dir).unwrap();
        fs::create_dir_all(&psx_covers).unwrap();
        fs::create_dir_all(&gba_covers).unwrap();

        fs::write(gba_dir.join("Doom (USA).gba"), b"gba1").unwrap();
        fs::write(
            gba_dir.join("Mario Kart - Super Circuit (USA).gba"),
            b"gba2",
        )
        .unwrap();

        write_test_png(&psx_covers.join("Doom (USA).png"));
        write_test_png(&gba_covers.join("Mario Kart - Super Circuit (USA).png"));

        let storage = StorageClient::Local;
        let cancel = AtomicBool::new(false);
        let opts = SyncOptions::default();
        let gba_str = gba_dir.to_string_lossy().to_string();

        let analysis = analyze(&storage, &gba_str, &covers_root, &opts, &cancel, |_| {})
            .await
            .unwrap();
        assert_eq!(analysis.items.len(), 2);

        let doom_item = analysis
            .items
            .iter()
            .find(|i| i.game.stem == "Doom (USA)")
            .unwrap();
        assert_eq!(doom_item.status, MatchStatus::Unmatched);
        assert!(doom_item.selected_cover_idx.is_none());
        assert!(!doom_item.selected_for_apply);

        let mk_item = analysis
            .items
            .iter()
            .find(|i| i.game.stem == "Mario Kart - Super Circuit (USA)")
            .unwrap();
        assert_eq!(mk_item.status, MatchStatus::Matched);
        assert!(mk_item.selected_for_apply);

        let summary = apply(
            &storage,
            &analysis.items,
            &analysis.covers,
            &opts,
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(summary.copied, 1);
        assert!(!gba_dir.join("media").join("Doom (USA).png").exists());
        assert!(
            gba_dir
                .join("media")
                .join("Mario Kart - Super Circuit (USA).png")
                .is_file()
        );
    }

    #[tokio::test]
    async fn analyze_separates_mister_boot_and_demo_from_games_and_keeps_bios_ignored() {
        let root = tempdir().unwrap();
        let psx_dir = root.path().join("PSX");
        let covers_dir = root.path().join("Covers");
        fs::create_dir_all(&psx_dir).unwrap();
        fs::create_dir_all(&covers_dir).unwrap();

        fs::write(psx_dir.join("Crash Bandicoot (USA).chd"), b"rom").unwrap();
        fs::write(psx_dir.join("mister-demo.chd"), b"demo").unwrap();
        fs::write(psx_dir.join("mister-boot.cue"), b"FILE").unwrap();
        fs::write(psx_dir.join("boot.rom"), b"bios").unwrap();
        write_test_png(&covers_dir.join("Crash Bandicoot (USA).png"));

        let storage = StorageClient::Local;
        let cancel = AtomicBool::new(false);
        let opts = SyncOptions::default();
        let psx_str = psx_dir.to_string_lossy().to_string();

        let analysis = analyze(&storage, &psx_str, &covers_dir, &opts, &cancel, |_| {})
            .await
            .unwrap();

        assert_eq!(analysis.items.len(), 1);
        assert_eq!(analysis.items[0].game.stem, "Crash Bandicoot (USA)");
        let boot_names: Vec<&str> = analysis
            .boot_demo_files
            .iter()
            .map(|g| g.file_name.as_str())
            .collect();
        assert_eq!(boot_names, vec!["mister-boot.cue", "mister-demo.chd"]);
    }
}
