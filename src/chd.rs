use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::archives::apply_no_window;

/// CD CHD codecs in `chdman createcd` order: `cdlz`, `cdzl`, `cdfl`.
const CD_CODECS: [u32; 4] = [
    u32::from_be_bytes(*b"cdlz"),
    u32::from_be_bytes(*b"cdzl"),
    u32::from_be_bytes(*b"cdfl"),
    0,
];

/// CD hunk size in bytes (`8 * 2448 = 19584`), matching `chdman createcd` default.
const CD_HUNK_BYTES: u32 = 8 * chd_rs::CD_FRAME_SIZE;

/// Detected engines capable of converting disc sheets (`.cue` / `.gdi`) to `.chd`.
#[derive(Debug, Clone)]
pub struct ChdToolsStatus {
    pub builtin_available: bool,
    pub chdman_path: Option<PathBuf>,
}

/// Summary returned after converting a disc image set to `.chd`.
#[derive(Debug, Clone)]
pub struct ChdConversionResult {
    pub output_bytes: u64,
    pub engine_used: String,
}

/// Returns `true` if `ext` (lowercase, without dot) can be converted to `.chd`.
pub fn is_convertible_sheet_ext(ext: &str, chdman_available: bool) -> bool {
    match ext {
        "cue" | "gdi" => true,
        "toc" | "nrg" => chdman_available,
        _ => false,
    }
}

/// Searches custom path, app executable folder, `PATH`, and common Windows locations for `chdman`.
pub fn find_chdman(custom_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = custom_path
        && !p.as_os_str().is_empty()
        && p.is_file()
    {
        return Some(p.to_path_buf());
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for name in ["chdman.exe", "chdman"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            for name in ["chdman", "chdman.exe"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    for fixed in [
        r"C:\Program Files\MAME\chdman.exe",
        r"C:\mame\chdman.exe",
        r"C:\chdman\chdman.exe",
    ] {
        let pb = PathBuf::from(fixed);
        if pb.is_file() {
            return Some(pb);
        }
    }

    None
}

/// Reports both the built-in `chd-rs` converter and any optional `chdman` binary found.
pub fn detect_chd_tools(custom_chdman: Option<&Path>) -> ChdToolsStatus {
    ChdToolsStatus {
        builtin_available: true,
        chdman_path: find_chdman(custom_chdman),
    }
}

struct PreparedSheet {
    sheet_path: PathBuf,
    in_place_temp_file: Option<PathBuf>,
    _temp_dir: Option<TempDir>,
}

impl Drop for PreparedSheet {
    fn drop(&mut self) {
        if let Some(ref p) = self.in_place_temp_file {
            let _ = fs::remove_file(p);
        }
    }
}

/// Parses a `.cue` line starting with `FILE` and returns `(referenced_file, trailing_tokens)`.
fn parse_cue_file_line(trimmed: &str) -> Option<(String, &str)> {
    let rest = trimmed.get(..4).filter(|s| s.eq_ignore_ascii_case("FILE"))?;
    let _ = rest;
    let after_kw = trimmed[4..].trim_start();
    if after_kw.is_empty() {
        return None;
    }
    if let Some(inner) = after_kw.strip_prefix('"') {
        let end_quote = inner.find('"')?;
        let file_ref = inner[..end_quote].to_string();
        let trailing = inner[end_quote + 1..].trim();
        Some((file_ref, trailing))
    } else {
        let mut parts = after_kw.splitn(2, char::is_whitespace);
        let file_ref = parts.next()?.trim().to_string();
        let trailing = parts.next().unwrap_or("").trim();
        Some((file_ref, trailing))
    }
}

fn find_track_file_in_dir(cue_dir: &Path, raw_ref: &str) -> Option<PathBuf> {
    let normalized = raw_ref.replace('\\', "/");
    let direct = cue_dir.join(&normalized);
    if !normalized.contains(':') && !normalized.starts_with('/') && direct.is_file() {
        return Some(direct);
    }

    let base_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    if !base_name.is_empty() {
        let in_root = cue_dir.join(base_name);
        if in_root.is_file() {
            return Some(in_root);
        }
    }

    let target_lower = base_name.to_ascii_lowercase();
    let mut best_ci: Option<PathBuf> = None;
    for entry in WalkDir::new(cue_dir)
        .follow_links(false)
        .max_depth(3)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name == base_name {
            return Some(entry.path().to_path_buf());
        }
        if best_ci.is_none() && name.to_ascii_lowercase() == target_lower {
            best_ci = Some(entry.path().to_path_buf());
        }
    }

    best_ci
}

/// Ensures `.cue` sheets have no UTF-8 BOM and all `FILE "..."` references resolve locally,
/// staging into a temporary hardlink workspace (or in-place temp CUE) only when needed.
fn prepare_sheet_for_conversion(sheet_path: &Path, allow_in_place: bool) -> Result<PreparedSheet> {
    let ext = sheet_path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext != "cue" {
        return Ok(PreparedSheet {
            sheet_path: sheet_path.to_path_buf(),
            in_place_temp_file: None,
            _temp_dir: None,
        });
    }

    let raw_bytes = fs::read(sheet_path)
        .with_context(|| format!("No se pudo leer CUE: {}", sheet_path.display()))?;
    let has_bom = raw_bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let content_bytes = if has_bom { &raw_bytes[3..] } else { &raw_bytes[..] };
    let text = String::from_utf8_lossy(content_bytes);

    let cue_dir = sheet_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut parsed_lines: Vec<String> = Vec::new();
    let mut resolved_tracks: Vec<(usize, PathBuf, String)> = Vec::new();
    let mut needs_rewrite = has_bom;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some((file_ref, trailing)) = parse_cue_file_line(trimmed) {
            let exact = cue_dir.join(&file_ref);
            let clean_rel = !file_ref.contains(':')
                && !file_ref.starts_with(['/', '\\'])
                && !file_ref.contains("..")
                && exact.is_file();
            if clean_rel {
                parsed_lines.push(line.to_string());
                resolved_tracks.push((parsed_lines.len() - 1, exact, trailing.to_string()));
            } else {
                let found = find_track_file_in_dir(cue_dir, &file_ref).with_context(|| {
                    format!(
                        "El archivo CUE '{}' referencia '{}', pero no se encontró en {}",
                        sheet_path.display(),
                        file_ref,
                        cue_dir.display()
                    )
                })?;
                needs_rewrite = true;
                parsed_lines.push(line.to_string());
                resolved_tracks.push((parsed_lines.len() - 1, found, trailing.to_string()));
            }
        } else {
            parsed_lines.push(line.to_string());
        }
    }

    if !needs_rewrite {
        return Ok(PreparedSheet {
            sheet_path: sheet_path.to_path_buf(),
            in_place_temp_file: None,
            _temp_dir: None,
        });
    }

    if allow_in_place {
        for (line_idx, track_path, trailing) in &resolved_tracks {
            let rel = track_path
                .strip_prefix(cue_dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| {
                    track_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                });
            parsed_lines[*line_idx] = if trailing.is_empty() {
                format!("FILE \"{rel}\" BINARY")
            } else {
                format!("FILE \"{rel}\" {trailing}")
            };
        }
        let temp_cue = cue_dir.join(".__sanitized_convert.cue");
        fs::write(&temp_cue, parsed_lines.join("\n"))?;
        return Ok(PreparedSheet {
            sheet_path: temp_cue.clone(),
            in_place_temp_file: Some(temp_cue),
            _temp_dir: None,
        });
    }

    let tmp_dir = tempfile::tempdir()?;
    let mut linked_names: HashMap<PathBuf, String> = HashMap::new();
    let mut used_names: HashSet<String> = HashSet::new();

    for (line_idx, track_path, trailing) in &resolved_tracks {
        let link_name = if let Some(existing) = linked_names.get(track_path) {
            existing.clone()
        } else {
            let orig_name = track_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "track.bin".to_string());
            let mut candidate = orig_name.clone();
            let mut counter = 1usize;
            while !used_names.insert(candidate.to_ascii_lowercase()) {
                candidate = format!("{counter}_{orig_name}");
                counter += 1;
            }
            let dst = tmp_dir.path().join(&candidate);
            if fs::hard_link(track_path, &dst).is_err() {
                fs::copy(track_path, &dst).with_context(|| {
                    format!(
                        "No se pudo enlazar ni copiar pista {} para conversión CHD",
                        track_path.display()
                    )
                })?;
            }
            linked_names.insert(track_path.clone(), candidate.clone());
            candidate
        };

        parsed_lines[*line_idx] = if trailing.is_empty() {
            format!("FILE \"{link_name}\" BINARY")
        } else {
            format!("FILE \"{link_name}\" {trailing}")
        };
    }

    let cue_name = sheet_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "disc.cue".to_string());
    let temp_cue = tmp_dir.path().join(cue_name);
    fs::write(&temp_cue, parsed_lines.join("\n"))?;

    Ok(PreparedSheet {
        sheet_path: temp_cue,
        in_place_temp_file: None,
        _temp_dir: Some(tmp_dir),
    })
}

/// Converts a `.cue`, `.gdi` (or `.toc`/`.nrg` with `chdman`) disc set into a v5 `.chd` file.
/// Uses the built-in Rust `chd-rs` encoder first and falls back to external `chdman` if needed.
pub fn convert_disc_to_chd<F>(
    input_sheet: &Path,
    output_chd: &Path,
    custom_chdman: Option<&Path>,
    allow_in_place_cue_fix: bool,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<ChdConversionResult>
where
    F: FnMut(u64, u64),
{
    if cancel.load(Ordering::Relaxed) {
        bail!("Conversión CHD cancelada por el usuario.");
    }

    if let Some(parent) = output_chd.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("No se pudo crear carpeta destino para CHD: {}", parent.display())
        })?;
    }

    let prepared = prepare_sheet_for_conversion(input_sheet, allow_in_place_cue_fix)?;
    let ext = prepared
        .sheet_path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let total_input_bytes = if matches!(ext.as_str(), "cue" | "gdi") {
        chd_rs::cd_input_size(&prepared.sheet_path).unwrap_or(0)
    } else {
        0
    };

    if matches!(ext.as_str(), "cue" | "gdi") {
        let mut accum_bytes = 0u64;
        on_progress(0, total_input_bytes.max(1));

        let mut progress_cb = |delta: u64| {
            accum_bytes = accum_bytes.saturating_add(delta);
            on_progress(accum_bytes, total_input_bytes.max(accum_bytes).max(1));
        };

        let res = chd_rs::create_cd(
            &prepared.sheet_path,
            output_chd,
            CD_HUNK_BYTES,
            CD_CODECS,
            None,
            &mut progress_cb,
        );

        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(output_chd);
            bail!("Conversión CHD cancelada por el usuario.");
        }

        match res {
            Ok(()) => {
                let out_size = fs::metadata(output_chd).map(|m| m.len()).unwrap_or(0);
                on_progress(
                    total_input_bytes.max(accum_bytes).max(1),
                    total_input_bytes.max(accum_bytes).max(1),
                );
                return Ok(ChdConversionResult {
                    output_bytes: out_size,
                    engine_used: "chd-rs integrado".to_string(),
                });
            }
            Err(native_err) => {
                let _ = fs::remove_file(output_chd);
                if let Some(chdman) = find_chdman(custom_chdman) {
                    return convert_with_chdman_cli(
                        &chdman,
                        &prepared.sheet_path,
                        output_chd,
                        total_input_bytes,
                        cancel,
                        &mut on_progress,
                    );
                }
                bail!(
                    "Error al convertir {} a CHD: {native_err}. (Si es una imagen atípica, puedes indicar la ruta a chdman en Opciones).",
                    input_sheet.display()
                );
            }
        }
    }

    if let Some(chdman) = find_chdman(custom_chdman) {
        return convert_with_chdman_cli(
            &chdman,
            &prepared.sheet_path,
            output_chd,
            total_input_bytes,
            cancel,
            &mut on_progress,
        );
    }

    bail!(
        "El formato .{ext} no está soportado por el conversor integrado sin chdman. Usa .cue/.gdi o configura chdman."
    );
}

fn convert_with_chdman_cli<F>(
    chdman: &Path,
    input_sheet: &Path,
    output_chd: &Path,
    total_input_bytes: u64,
    cancel: &AtomicBool,
    on_progress: &mut F,
) -> Result<ChdConversionResult>
where
    F: FnMut(u64, u64),
{
    on_progress(0, total_input_bytes.max(1));

    let mut cmd = Command::new(chdman);
    cmd.args(["createcd", "-i"])
        .arg(input_sheet)
        .arg("-o")
        .arg(output_chd)
        .arg("-f")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_no_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("No se pudo iniciar chdman en {}", chdman.display()))?;

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(output_chd);
            bail!("Conversión CHD cancelada por el usuario.");
        }
        if let Some(status) = child.try_wait()? {
            if status.success() {
                let out_size = fs::metadata(output_chd).map(|m| m.len()).unwrap_or(0);
                on_progress(total_input_bytes.max(1), total_input_bytes.max(1));
                return Ok(ChdConversionResult {
                    output_bytes: out_size,
                    engine_used: "chdman CLI".to_string(),
                });
            }
            let _ = fs::remove_file(output_chd);
            let mut err_buf = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut err_buf);
            }
            bail!(
                "chdman createcd falló con código {:?}: {}",
                status.code(),
                err_buf.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chd_rs::{Chd, VerifyOutcome};
    use tempfile::tempdir;

    fn sample_cd_frames(num_frames: usize, seed: u64) -> Vec<u8> {
        let mut data = vec![0u8; num_frames * 2352];
        let mut state = seed | 1;
        for (idx, b) in data.iter_mut().enumerate() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = if (idx / 2352) % 3 == 2 {
                (state >> 16) as u8
            } else {
                ((idx % 239) as u8) ^ (seed as u8)
            };
        }
        data
    }

    #[test]
    fn converts_multibin_cue_to_verified_chd() {
        let dir = tempdir().unwrap();
        let game_dir = dir.path().join("Castlevania");
        fs::create_dir_all(&game_dir).unwrap();

        let cue = game_dir.join("Castlevania (USA).cue");
        fs::write(
            &cue,
            concat!(
                "FILE \"Castlevania (USA) (Track 1).bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
                "FILE \"Castlevania (USA) (Track 2).bin\" BINARY\n",
                "  TRACK 02 AUDIO\n",
                "    INDEX 00 00:00:00\n",
                "    INDEX 01 00:02:00\n",
            ),
        )
        .unwrap();
        let t1 = sample_cd_frames(24, 1);
        let t2 = sample_cd_frames(180, 2);
        let raw_total = (t1.len() + t2.len()) as u64;
        fs::write(game_dir.join("Castlevania (USA) (Track 1).bin"), t1).unwrap();
        fs::write(game_dir.join("Castlevania (USA) (Track 2).bin"), t2).unwrap();

        let out_chd = dir.path().join("out").join("Castlevania (USA).chd");
        let cancel = AtomicBool::new(false);
        let res = convert_disc_to_chd(&cue, &out_chd, None, false, &cancel, |_, _| {}).unwrap();

        assert_eq!(res.engine_used, "chd-rs integrado");
        assert!(out_chd.is_file());
        assert!(res.output_bytes > 0 && res.output_bytes < raw_total);
        assert_eq!(&fs::read(&out_chd).unwrap()[..8], b"MComprHD");

        let mut opened = Chd::open(&out_chd).unwrap();
        assert_eq!(opened.verify().unwrap(), VerifyOutcome::Ok);
    }

    #[test]
    fn sanitizes_bom_and_absolute_track_paths_in_cue_without_mutating_source() {
        let dir = tempdir().unwrap();
        let game_dir = dir.path().join("BomGame");
        fs::create_dir_all(&game_dir).unwrap();

        let cue = game_dir.join("BomGame.cue");
        let mut cue_bytes = vec![0xEF, 0xBB, 0xBF];
        cue_bytes.extend_from_slice(
            b"FILE \"C:\\old_rips\\BomGame\\BomGame (Track 1).bin\" BINARY\r\n  TRACK 01 MODE1/2352\r\n    INDEX 01 00:00:00\r\n",
        );
        fs::write(&cue, &cue_bytes).unwrap();
        fs::write(
            game_dir.join("BomGame (Track 1).bin"),
            sample_cd_frames(16, 42),
        )
        .unwrap();

        let out_chd = dir.path().join("BomGame.chd");
        let cancel = AtomicBool::new(false);
        convert_disc_to_chd(&cue, &out_chd, None, false, &cancel, |_, _| {}).unwrap();

        // Source CUE must remain untouched
        assert_eq!(fs::read(&cue).unwrap(), cue_bytes);
        let mut opened = Chd::open(&out_chd).unwrap();
        assert_eq!(opened.verify().unwrap(), VerifyOutcome::Ok);
    }
}
