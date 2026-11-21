use std::path::Path;

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

use crate::models::CoverImage;
use crate::systems::detect_cover_system;

/// Raw file metadata produced by either local or SFTP directory reads.
#[derive(Debug, Clone)]
pub struct RawFileEntry {
    pub full_path: String,
    pub parent_dir: String,
    pub relative_dir: String,
    pub file_name: String,
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif"];

const IGNORED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "ico", "svg", "txt", "nfo", "xml", "json", "ini",
    "cfg", "sav", "srm", "mcd", "mcr", "rtc", "sta", "cht", "ips", "bps", "ups", "pdf", "db",
    "sqlite", "log", "sh", "bat", "exe", "dll", "bak", "sfv", "md5", "sha1", "torrent", "url",
    "lnk", "mgl", "ds_store", "thumbs",
];

const DOC_MARKDOWN_FILES: &[&str] = &[
    "readme.md",
    "changelog.md",
    "license.md",
    "contributing.md",
    "history.md",
    "notes.md",
];

const BIOS_FILES: &[&str] = &[
    "boot.rom",
    "boot0.rom",
    "boot1.rom",
    "boot2.rom",
    "boot3.rom",
    "gba_bios.bin",
    "gb_bios.bin",
    "gbc_bios.bin",
    "sgb_bios.bin",
    "syscard1.pce",
    "syscard2.pce",
    "syscard3.pce",
    "games_express.pce",
    "bios.rom",
    "bios.bin",
    "neogeo.zip",
    "pgm.zip",
    "kick.rom",
    "kick13.rom",
    "kick31.rom",
    "cd_bios.bin",
    "bios_cd_u.bin",
    "bios_cd_e.bin",
    "bios_cd_j.bin",
    "saturn_bios.bin",
    "sega_101.bin",
    "mpr-17933.bin",
    "3do_bios.bin",
    "panafz10.bin",
];

const SUPPORTED_GAME_EXTENSIONS: &[&str] = &[
    // Optical disc / playlists
    "chd", "cue", "m3u", "iso", "cso", "pbp", "ccd", "gdi", "mds", "cdi",
    // Consoles & handhelds
    "nes", "fds", "unf", "sfc", "smc", "bs", "n64", "z64", "v64", "gb", "gbc", "gba", "sgb",
    "md", "gen", "smd", "32x", "sms", "gg", "sg", "pce", "sgx", "ngp", "ngc", "ws", "wsc",
    "a26", "a52", "a78", "j64", "jag", "lnx", "col", "int", "vec", "o2", "vb", "p8", "neo",
    "nds", "3ds", "rvz", "gcm", "sv",
    // Arcade / general ROMs / containers
    "rom", "bin", "zip", "7z", "mra",
    // Microcomputers
    "dsk", "adf", "hdf", "st", "tap", "tzx", "t64", "d64", "prg", "crt", "atr", "xex", "cas",
    "mx1", "mx2", "vdk", "ssd", "dsd", "woz", "nib", "po", "do", "img", "vhd", "cpr", "cdt",
    "sna", "z80", "trd", "scl", "fdi", "hdm", "d88", "xdf", "dim",
];

const DISC_SHEET_EXTENSIONS: &[&str] = &["cue", "ccd", "gdi"];
const DISC_TRACK_EXTENSIONS: &[&str] = &["bin", "img", "raw", "sub", "wav"];

/// Splits a file name into `(stem, lowercase_extension)` without dot.
pub fn split_stem_ext(file_name: &str) -> (&str, String) {
    match file_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, ext.to_lowercase()),
        _ => (file_name, String::new()),
    }
}

/// Returns `true` for directories that must never be scanned as game folders.
pub fn is_ignored_dir(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "media"
        || lower == "system volume information"
        || lower == "$recycle.bin"
        || name.starts_with('.')
}

/// Returns `true` if `ext` (case-insensitive, without dot) is a supported cover image format.
pub fn is_image_extension(ext: &str) -> bool {
    let lower = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&lower.as_str())
}

/// Returns `true` if the file is a hidden dotfile, BIOS, save, image, or auxiliary metadata file.
pub fn is_ignored_non_game_file(file_name: &str, ext: &str) -> bool {
    if file_name.starts_with('.') || file_name.starts_with("._") {
        return true;
    }

    let lower_name = file_name.to_ascii_lowercase();
    if BIOS_FILES.contains(&lower_name.as_str()) || DOC_MARKDOWN_FILES.contains(&lower_name.as_str())
    {
        return true;
    }

    if lower_name.starts_with("scph") && ext == "bin" {
        return true;
    }

    if ext.is_empty() || IGNORED_EXTENSIONS.contains(&ext) {
        return true;
    }

    false
}

/// Returns `true` if `ext` (lowercase, without dot) is a known MiSTer FPGA game/ROM extension.
pub fn is_supported_game_extension(ext: &str) -> bool {
    SUPPORTED_GAME_EXTENSIONS.contains(&ext)
}

/// Filters files inside a single directory into valid game entries, deduplicating disc tracks
/// (`.bin`, `.img`, `.raw`, `.sub`, `.wav`) whenever a `.cue`, `.ccd`, or `.gdi` sheet exists
/// in that same directory.
pub fn filter_directory_games(entries: Vec<RawFileEntry>) -> Vec<(RawFileEntry, String, String)> {
    let has_disc_sheet = entries.iter().any(|entry| {
        let (_, ext) = split_stem_ext(&entry.file_name);
        DISC_SHEET_EXTENSIONS.contains(&ext.as_str())
    });

    entries
        .into_iter()
        .filter_map(|entry| {
            let (stem, ext) = split_stem_ext(&entry.file_name);
            if is_ignored_non_game_file(&entry.file_name, &ext) {
                return None;
            }
            if !is_supported_game_extension(&ext) {
                return None;
            }
            if has_disc_sheet && DISC_TRACK_EXTENSIONS.contains(&ext.as_str()) {
                return None;
            }
            let stem_owned = stem.to_string();
            Some((entry, stem_owned, ext))
        })
        .collect()
}

/// Scans a local folder (optionally including subfolders) for valid cover image files.
pub fn scan_local_covers(dir: &Path, recursive: bool) -> Result<Vec<CoverImage>> {
    if !dir.exists() {
        bail!("La carpeta de imágenes no existe: {}", dir.display());
    }
    if !dir.is_dir() {
        bail!(
            "La ruta de imágenes no es una carpeta válida: {}",
            dir.display()
        );
    }

    let mut walker = WalkDir::new(dir).follow_links(true);
    if !recursive {
        walker = walker.max_depth(1);
    }

    let mut covers = Vec::new();
    for entry in walker.into_iter() {
        let entry = entry.with_context(|| {
            format!("Error al recorrer carpeta de imágenes: {}", dir.display())
        })?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }
        let Some(file_name_os) = path.file_name() else {
            continue;
        };
        let file_name = file_name_os.to_string_lossy().to_string();
        if file_name.starts_with('.') || file_name.starts_with("._") {
            continue;
        }
        let (stem_slice, ext) = split_stem_ext(&file_name);
        if !is_image_extension(&ext) {
            continue;
        }
        let stem = stem_slice.to_string();
        let system = detect_cover_system(dir, path, &stem);

        covers.push(CoverImage {
            path: path.to_path_buf(),
            file_name,
            stem,
            extension: ext,
            system,
        });
    }

    covers.sort_by(|a, b| {
        a.file_name
            .to_lowercase()
            .cmp(&b.file_name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(covers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str) -> RawFileEntry {
        RawFileEntry {
            full_path: format!("/games/PSX/{name}"),
            parent_dir: "/games/PSX".to_string(),
            relative_dir: String::new(),
            file_name: name.to_string(),
        }
    }

    #[test]
    fn ignores_media_and_hidden_dirs() {
        assert!(is_ignored_dir("media"));
        assert!(is_ignored_dir("Media"));
        assert!(is_ignored_dir(".git"));
        assert!(is_ignored_dir("System Volume Information"));
        assert!(!is_ignored_dir("RPG"));
    }

    #[test]
    fn deduplicates_bin_tracks_when_cue_is_present() {
        let entries = vec![
            raw("Castlevania (USA).cue"),
            raw("Castlevania (USA) (Track 1).bin"),
            raw("Castlevania (USA) (Track 2).bin"),
            raw("boot.rom"),
            raw("readme.txt"),
            raw("save.mcd"),
            raw("cover.png"),
        ];
        let games = filter_directory_games(entries);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].0.file_name, "Castlevania (USA).cue");
        assert_eq!(games[0].1, "Castlevania (USA)");
        assert_eq!(games[0].2, "cue");
    }

    #[test]
    fn keeps_bin_roms_when_no_cue_present() {
        let entries = vec![
            raw("Sonic The Hedgehog (USA).bin"),
            raw("Streets of Rage 2 (USA).md"),
            raw("scph1001.bin"),
        ];
        let games = filter_directory_games(entries);
        let names: Vec<&str> = games.iter().map(|g| g.0.file_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Sonic The Hedgehog (USA).bin", "Streets of Rage 2 (USA).md"]
        );
    }

    #[test]
    fn ignores_non_game_extensions_and_appledouble() {
        assert!(is_ignored_non_game_file("._Crash.chd", "chd"));
        assert!(is_ignored_non_game_file("boot1.rom", "rom"));
        assert!(is_ignored_non_game_file("README.md", "md"));
        assert!(!is_ignored_non_game_file("Sonic 3 (USA).md", "md"));
        assert!(!is_ignored_non_game_file("Sonic Chaos (USA).gg", "gg"));
        assert!(is_ignored_non_game_file("game.sav", "sav"));
        assert!(is_ignored_non_game_file("art.png", "png"));
        assert!(!is_ignored_non_game_file("Metal Slug.mra", "mra"));
    }

    #[test]
    fn ignored_and_supported_extensions_are_disjoint() {
        for ext in IGNORED_EXTENSIONS {
            assert!(
                !SUPPORTED_GAME_EXTENSIONS.contains(ext),
                "Extension .{ext} must not appear in both IGNORED_EXTENSIONS and SUPPORTED_GAME_EXTENSIONS"
            );
        }
    }
}
