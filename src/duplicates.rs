//! Detection and removal of ROMs/cover-less games that share the same canonical name.
//!
//! Grouping reuses the matcher's normalization (regions, disc tags, accents,
//! articles and roman numerals) so `Final Fantasy VII (USA).chd` and
//! `Final Fantasy 7 (Europe).cue` are treated as duplicates of the same game.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::matcher;
use crate::models::GameEntry;
use crate::scanner::{is_mister_boot_or_demo_stem, split_stem_ext};
use crate::storage::{StorageClient, join_storage_path, posix_normalize, posix_parent};

/// A single game inside a duplicate group.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateGame {
    pub path: String,
    pub file_name: String,
    pub stem: String,
    pub extension: String,
    pub system: Option<String>,
    pub relative_dir: String,
}

/// A set of games whose names normalize to the same key.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateGroup {
    pub key: String,
    pub games: Vec<DuplicateGame>,
    /// Index of the copy recommended to keep (best region / preferred format).
    pub keep_idx: usize,
}

/// Result of a duplicate deletion run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeleteSummary {
    pub deleted: usize,
    pub tracks_deleted: usize,
    pub failed: usize,
    pub trash_dir: Option<String>,
    pub errors: Vec<(String, String)>,
}

/// Builds duplicate groups from a previously scanned game list.
pub fn find_duplicates(games: &[GameEntry]) -> Vec<DuplicateGroup> {
    let mut buckets: HashMap<String, Vec<&GameEntry>> = HashMap::new();
    for game in games {
        if is_mister_boot_or_demo_stem(&game.stem) {
            continue;
        }
        let key = matcher::canonical_game_key(&game.stem);
        if !key.is_empty() {
            buckets.entry(key).or_default().push(game);
        }
    }

    let mut groups: Vec<DuplicateGroup> = buckets
        .into_iter()
        .filter(|(_, entries)| entries.len() > 1)
        .map(|(key, mut entries)| {
            entries.sort_by(|a, b| a.full_path.cmp(&b.full_path));
            let keep_idx = recommended_keep(&entries);
            DuplicateGroup {
                key,
                games: entries
                    .iter()
                    .map(|game| to_duplicate_game(*game))
                    .collect(),
                keep_idx,
            }
        })
        .collect();

    groups.sort_by(|a, b| a.key.cmp(&b.key));
    groups
}

/// Finds cosmetic MiSTer boot/demo ROMs (`mister-boot*`, `mister-demo*`)
/// inside a previously scanned game list, sorted by path.
pub fn find_boot_demo_files(games: &[GameEntry]) -> Vec<DuplicateGame> {
    let mut found: Vec<&GameEntry> = games
        .iter()
        .filter(|game| is_mister_boot_or_demo_stem(&game.stem))
        .collect();
    found.sort_by(|a, b| a.full_path.cmp(&b.full_path));
    found.into_iter().map(to_duplicate_game).collect()
}

/// Returns the paths of every game in `group` except the one at `keep_idx`.
pub fn paths_to_delete(group: &DuplicateGroup, keep_idx: usize) -> Vec<String> {
    group
        .games
        .iter()
        .enumerate()
        .filter(|(idx, _)| *idx != keep_idx)
        .map(|(_, game)| game.path.clone())
        .collect()
}

/// Validates that all requested paths belong to one of the duplicate groups.
pub fn validate_delete_paths(groups: &[DuplicateGroup], paths: &[String]) -> Result<Vec<String>> {
    let known: HashSet<&str> = groups
        .iter()
        .flat_map(|group| group.games.iter().map(|game| game.path.as_str()))
        .collect();

    let mut unique = Vec::new();
    for path in paths {
        if !known.contains(path.as_str()) {
            bail!("La ruta no pertenece a ningún grupo de duplicados: {path}");
        }
        if !unique.contains(path) {
            unique.push(path.clone());
        }
    }
    if unique.is_empty() {
        bail!("No se indicaron rutas para eliminar.");
    }
    Ok(unique)
}

/// Deletes the selected duplicate files. On local storage they are moved to
/// `<root>/.duplicates_trash/<timestamp>/` so the operation is recoverable;
/// on SFTP they are removed directly (SFTP has no trash).
pub async fn delete_paths<F>(
    storage: &StorageClient,
    root: &str,
    is_sftp: bool,
    paths: &[String],
    dry_run: bool,
    on_progress: F,
) -> Result<DeleteSummary>
where
    F: Fn(&str) + Send + Sync,
{
    let mut summary = DeleteSummary::default();
    if paths.is_empty() {
        return Ok(summary);
    }

    let trash_dir = if !is_sftp && !dry_run {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let trash = PathBuf::from(root)
            .join(".duplicates_trash")
            .join(stamp.to_string());
        tokio::fs::create_dir_all(&trash)
            .await
            .with_context(|| format!("No se pudo crear la papelera: {}", trash.display()))?;
        summary.trash_dir = Some(trash.to_string_lossy().to_string());
        Some(trash)
    } else {
        None
    };

    let total = paths.len();
    for (idx, path) in paths.iter().enumerate() {
        on_progress(&format!("[{}/{}] {path}", idx + 1, total));

        let Some(relative) = relative_inside(root, path, is_sftp) else {
            summary.failed += 1;
            summary.errors.push((
                path.clone(),
                "La ruta no está dentro de la carpeta de juegos indicada".to_string(),
            ));
            continue;
        };

        let tracks = find_disc_tracks(storage, path, is_sftp).await;

        if dry_run {
            summary.deleted += 1;
            summary.tracks_deleted += tracks.len();
            continue;
        }

        let result = if let Some(ref trash) = trash_dir {
            move_to_trash(path, &relative, trash, &tracks).await
        } else {
            delete_directly(storage, path, &tracks).await
        };

        match result {
            Ok(track_count) => {
                summary.deleted += 1;
                summary.tracks_deleted += track_count;
            }
            Err(err) => {
                summary.failed += 1;
                summary.errors.push((path.clone(), format!("{err:#}")));
            }
        }
    }

    Ok(summary)
}

async fn delete_directly(storage: &StorageClient, path: &str, tracks: &[String]) -> Result<usize> {
    storage.remove_file(path).await?;
    let mut deleted = 0;
    for track in tracks {
        storage.remove_file(track).await?;
        deleted += 1;
    }
    Ok(deleted)
}

async fn move_to_trash(
    path: &str,
    relative: &str,
    trash: &Path,
    tracks: &[String],
) -> Result<usize> {
    let destination = trash.join(relative);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "No se pudo crear la carpeta de papelera: {}",
                parent.display()
            )
        })?;
    }
    tokio::fs::rename(path, &destination)
        .await
        .with_context(|| format!("No se pudo mover a la papelera: {path}"))?;

    let mut moved = 0;
    for track in tracks {
        let track_name = Path::new(track)
            .file_name()
            .with_context(|| format!("Nombre de pista inválido: {track}"))?;
        let track_dest = destination.parent().unwrap_or(trash).join(track_name);
        tokio::fs::rename(track, &track_dest)
            .await
            .with_context(|| format!("No se pudo mover la pista a la papelera: {track}"))?;
        moved += 1;
    }
    Ok(moved)
}

/// When a `.cue`/`.ccd`/`.gdi` sheet is removed, its sibling tracks in the same
/// folder would be orphaned. Include them in the deletion.
async fn find_disc_tracks(storage: &StorageClient, path: &str, is_sftp: bool) -> Vec<String> {
    let (name, ext) = match file_name_of(path, is_sftp) {
        Some(value) => value,
        None => return Vec::new(),
    };
    if !["cue", "ccd", "gdi"].contains(&ext.as_str()) {
        return Vec::new();
    }
    let (stem, _) = split_stem_ext(&name);

    let parent = if is_sftp {
        posix_parent(path)
    } else {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    if parent.is_empty() {
        return Vec::new();
    }

    let Ok(files) = storage.list_files(&parent).await else {
        return Vec::new();
    };
    files
        .into_iter()
        .filter(|file| {
            let (file_stem, file_ext) = split_stem_ext(file);
            file_stem.eq_ignore_ascii_case(stem)
                && ["bin", "img", "raw", "sub", "wav"].contains(&file_ext.as_str())
        })
        .map(|file| join_storage_path(is_sftp, &parent, &file))
        .collect()
}

fn to_duplicate_game(game: &GameEntry) -> DuplicateGame {
    DuplicateGame {
        path: game.full_path.clone(),
        file_name: game.file_name.clone(),
        stem: game.stem.clone(),
        extension: game.extension.clone(),
        system: game.system.map(|system| system.display_name().to_string()),
        relative_dir: game.relative_dir.clone(),
    }
}

fn recommended_keep(games: &[&GameEntry]) -> usize {
    let mut best = 0usize;
    let mut best_score = i32::MIN;
    for (idx, game) in games.iter().enumerate() {
        let score = keep_score(game);
        if score > best_score {
            best_score = score;
            best = idx;
        }
    }
    best
}

fn keep_score(game: &GameEntry) -> i32 {
    let lower = game.stem.to_ascii_lowercase();
    let mut score = 0i32;
    if lower.contains("(usa)") || lower.contains("(us)") {
        score += 4;
    } else if lower.contains("(europe)") || lower.contains("(eu)") {
        score += 3;
    }
    match game.extension.as_str() {
        "chd" => score += 3,
        "cue" | "ccd" | "gdi" | "m3u" => score += 2,
        "iso" | "cso" | "pbp" => score += 1,
        "zip" | "7z" | "rar" => score -= 1,
        _ => {}
    }
    score -= (game.full_path.len() / 40) as i32;
    score
}

fn relative_inside(root: &str, path: &str, is_sftp: bool) -> Option<String> {
    if is_sftp {
        let root_norm = posix_normalize(root);
        let root_norm = root_norm.trim_end_matches('/');
        let path_norm = posix_normalize(path);
        let prefix = format!("{root_norm}/");
        let tail = path_norm.strip_prefix(&prefix)?;
        if tail.is_empty() || tail.split('/').any(|part| part.is_empty() || part == ".") {
            return None;
        }
        Some(tail.to_string())
    } else {
        let tail = Path::new(path).strip_prefix(Path::new(root)).ok()?;
        if tail.as_os_str().is_empty() {
            return None;
        }
        if tail
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        Some(tail.to_string_lossy().replace('\\', "/"))
    }
}

fn file_name_of(path: &str, is_sftp: bool) -> Option<(String, String)> {
    let name = if is_sftp {
        path.rsplit('/').next().unwrap_or(path).to_string()
    } else {
        Path::new(path).file_name()?.to_string_lossy().to_string()
    };
    if name.is_empty() {
        None
    } else {
        let (stem, ext) = split_stem_ext(&name);
        Some((stem.to_string(), ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::GameEntry;

    fn game(path: &str, stem: &str, ext: &str) -> GameEntry {
        GameEntry {
            full_path: path.to_string(),
            parent_dir: String::new(),
            relative_dir: String::new(),
            file_name: format!("{stem}.{ext}"),
            stem: stem.to_string(),
            extension: ext.to_string(),
            system: None,
        }
    }

    #[test]
    fn groups_equivalent_names_and_keeps_sequels_apart() {
        let games = vec![
            game(
                "/games/PSX/Final Fantasy VII (USA).chd",
                "Final Fantasy VII (USA)",
                "chd",
            ),
            game(
                "/games/PSX/Final Fantasy 7 (Europe).cue",
                "Final Fantasy 7 (Europe)",
                "cue",
            ),
            game(
                "/games/PSX/Mario Bros 2 (USA).zip",
                "Mario Bros 2 (USA)",
                "zip",
            ),
            game(
                "/games/PSX/Mario Bros 3 (USA).zip",
                "Mario Bros 3 (USA)",
                "zip",
            ),
        ];
        let groups = find_duplicates(&games);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].games.len(), 2);
        // The .chd copy is recommended over the .cue copy.
        assert_eq!(groups[0].games[groups[0].keep_idx].extension, "chd");
    }

    #[test]
    fn paths_to_delete_excludes_kept_copy() {
        let games = vec![
            game("/games/A (USA).zip", "A (USA)", "zip"),
            game("/games/A (Europe).zip", "A (Europe)", "zip"),
        ];
        let group = find_duplicates(&games).remove(0);
        let keep = group.keep_idx;
        let paths = paths_to_delete(&group, keep);
        assert_eq!(paths.len(), 1);
        assert_ne!(paths[0], group.games[keep].path);
    }

    #[test]
    fn rejects_paths_outside_root() {
        assert!(relative_inside("/games", "/games/PSX/A.zip", false).is_some());
        assert!(relative_inside("/games", "/other/A.zip", false).is_none());
        assert!(relative_inside("/games", "/games/../A.zip", false).is_none());
        assert_eq!(
            relative_inside("/media/fat/games", "/media/fat/games/PSX/A.chd", true),
            Some("PSX/A.chd".to_string())
        );
    }

    #[test]
    fn finds_mister_boot_and_demo_and_excludes_them_from_duplicate_groups() {
        let games = vec![
            game("/games/NES/mister-boot.nes", "mister-boot", "nes"),
            game("/games/SNES/mister-boot.sfc", "mister-boot", "sfc"),
            game("/games/PSX/mister-demo.chd", "mister-demo", "chd"),
            game("/games/PSX/boot.rom", "boot", "rom"),
            game("/games/PSX/Crash (USA).chd", "Crash (USA)", "chd"),
        ];
        let found = find_boot_demo_files(&games);
        let paths: Vec<&str> = found.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/games/NES/mister-boot.nes",
                "/games/PSX/mister-demo.chd",
                "/games/SNES/mister-boot.sfc",
            ]
        );
        assert!(find_duplicates(&games).is_empty());
    }

    #[tokio::test]
    async fn deletes_boot_demo_to_local_trash_and_cleans_disc_tracks() {
        let root = tempfile::tempdir().unwrap();
        let psx_dir = root.path().join("PSX");
        std::fs::create_dir_all(&psx_dir).unwrap();

        let demo_chd = psx_dir.join("mister-demo.chd");
        let boot_cue = psx_dir.join("mister-boot.cue");
        let boot_bin = psx_dir.join("mister-boot.bin");
        let real_bios = psx_dir.join("boot.rom");

        std::fs::write(&demo_chd, b"demo").unwrap();
        std::fs::write(&boot_cue, b"FILE").unwrap();
        std::fs::write(&boot_bin, b"track").unwrap();
        std::fs::write(&real_bios, b"bios").unwrap();

        let root_str = root.path().to_string_lossy().to_string();
        let paths = vec![
            demo_chd.to_string_lossy().to_string(),
            boot_cue.to_string_lossy().to_string(),
        ];

        let summary = delete_paths(&StorageClient::Local, &root_str, false, &paths, false, |_| {})
            .await
            .unwrap();

        assert_eq!(summary.deleted, 2);
        assert_eq!(summary.tracks_deleted, 1);
        assert_eq!(summary.failed, 0);
        assert!(!demo_chd.exists());
        assert!(!boot_cue.exists());
        assert!(!boot_bin.exists());
        assert!(real_bios.is_file());

        let trash_dir = PathBuf::from(summary.trash_dir.unwrap());
        assert!(trash_dir.join("PSX").join("mister-demo.chd").is_file());
        assert!(trash_dir.join("PSX").join("mister-boot.cue").is_file());
        assert!(trash_dir.join("PSX").join("mister-boot.bin").is_file());
    }
}
