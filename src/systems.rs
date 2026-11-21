use std::path::Path;

/// Canonical gaming console / platform identifiers used to prevent cross-system cover mismatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SystemId {
    Psx,
    Ps2,
    Psp,
    Gba,
    Gbc,
    Gb,
    Nes,
    Snes,
    N64,
    Nds,
    N3ds,
    GameCube,
    VirtualBoy,
    Genesis,
    Sega32x,
    SegaCd,
    Saturn,
    Sms,
    GameGear,
    Dreamcast,
    Sg1000,
    PcEngine,
    PcEngineCd,
    NeoGeo,
    NeoGeoCd,
    NeoGeoPocket,
    WonderSwan,
    Atari2600,
    Atari5200,
    Atari7800,
    AtariLynx,
    AtariJaguar,
    ColecoVision,
    Intellivision,
    Vectrex,
    Arcade,
    Amiga,
    C64,
    Msx,
    ZxSpectrum,
    X68000,
    Pico8,
}

impl SystemId {
    /// Short MiSTer-style identifier shown in badges and CLI output.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Psx => "PSX",
            Self::Ps2 => "PS2",
            Self::Psp => "PSP",
            Self::Gba => "GBA",
            Self::Gbc => "GBC",
            Self::Gb => "GB",
            Self::Nes => "NES",
            Self::Snes => "SNES",
            Self::N64 => "N64",
            Self::Nds => "NDS",
            Self::N3ds => "3DS",
            Self::GameCube => "GC",
            Self::VirtualBoy => "VB",
            Self::Genesis => "MD",
            Self::Sega32x => "32X",
            Self::SegaCd => "MegaCD",
            Self::Saturn => "Saturn",
            Self::Sms => "SMS",
            Self::GameGear => "GG",
            Self::Dreamcast => "DC",
            Self::Sg1000 => "SG1000",
            Self::PcEngine => "PCE",
            Self::PcEngineCd => "PCE-CD",
            Self::NeoGeo => "NeoGeo",
            Self::NeoGeoCd => "NeoGeoCD",
            Self::NeoGeoPocket => "NGP",
            Self::WonderSwan => "WS",
            Self::Atari2600 => "Atari2600",
            Self::Atari5200 => "Atari5200",
            Self::Atari7800 => "Atari7800",
            Self::AtariLynx => "Lynx",
            Self::AtariJaguar => "Jaguar",
            Self::ColecoVision => "Coleco",
            Self::Intellivision => "Intellivision",
            Self::Vectrex => "Vectrex",
            Self::Arcade => "Arcade",
            Self::Amiga => "Amiga",
            Self::C64 => "C64",
            Self::Msx => "MSX",
            Self::ZxSpectrum => "Spectrum",
            Self::X68000 => "X68000",
            Self::Pico8 => "PICO-8",
        }
    }

    /// Full human-readable name for inspector and warnings.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Psx => "PlayStation (PSX)",
            Self::Ps2 => "PlayStation 2 (PS2)",
            Self::Psp => "PlayStation Portable (PSP)",
            Self::Gba => "Game Boy Advance (GBA)",
            Self::Gbc => "Game Boy Color (GBC)",
            Self::Gb => "Game Boy (GB)",
            Self::Nes => "Nintendo NES / Famicom",
            Self::Snes => "Super Nintendo (SNES)",
            Self::N64 => "Nintendo 64 (N64)",
            Self::Nds => "Nintendo DS (NDS)",
            Self::N3ds => "Nintendo 3DS",
            Self::GameCube => "Nintendo GameCube",
            Self::VirtualBoy => "Virtual Boy",
            Self::Genesis => "Sega Mega Drive / Genesis",
            Self::Sega32x => "Sega 32X",
            Self::SegaCd => "Sega Mega-CD / Sega CD",
            Self::Saturn => "Sega Saturn",
            Self::Sms => "Sega Master System",
            Self::GameGear => "Sega Game Gear",
            Self::Dreamcast => "Sega Dreamcast",
            Self::Sg1000 => "Sega SG-1000",
            Self::PcEngine => "PC Engine / TurboGrafx-16",
            Self::PcEngineCd => "PC Engine CD / TurboGrafx-CD",
            Self::NeoGeo => "SNK Neo Geo",
            Self::NeoGeoCd => "SNK Neo Geo CD",
            Self::NeoGeoPocket => "SNK Neo Geo Pocket",
            Self::WonderSwan => "Bandai WonderSwan",
            Self::Atari2600 => "Atari 2600",
            Self::Atari5200 => "Atari 5200",
            Self::Atari7800 => "Atari 7800",
            Self::AtariLynx => "Atari Lynx",
            Self::AtariJaguar => "Atari Jaguar",
            Self::ColecoVision => "ColecoVision",
            Self::Intellivision => "Intellivision",
            Self::Vectrex => "Vectrex",
            Self::Arcade => "Arcade / MAME / FBNeo",
            Self::Amiga => "Commodore Amiga",
            Self::C64 => "Commodore 64",
            Self::Msx => "MSX",
            Self::ZxSpectrum => "ZX Spectrum",
            Self::X68000 => "Sharp X68000",
            Self::Pico8 => "PICO-8",
        }
    }

    /// All supported console identifiers ordered with the most common MiSTer systems first.
    pub const ALL: &'static [SystemId] = &[
        Self::Psx,
        Self::Gba,
        Self::Gbc,
        Self::Gb,
        Self::Snes,
        Self::Nes,
        Self::N64,
        Self::Genesis,
        Self::SegaCd,
        Self::Sega32x,
        Self::Saturn,
        Self::Sms,
        Self::GameGear,
        Self::PcEngine,
        Self::PcEngineCd,
        Self::NeoGeo,
        Self::NeoGeoCd,
        Self::NeoGeoPocket,
        Self::WonderSwan,
        Self::Arcade,
        Self::Atari2600,
        Self::Atari5200,
        Self::Atari7800,
        Self::AtariLynx,
        Self::AtariJaguar,
        Self::ColecoVision,
        Self::Intellivision,
        Self::Vectrex,
        Self::Amiga,
        Self::C64,
        Self::Msx,
        Self::ZxSpectrum,
        Self::X68000,
        Self::Pico8,
        Self::Nds,
        Self::N3ds,
        Self::GameCube,
        Self::VirtualBoy,
        Self::Dreamcast,
        Self::Sg1000,
        Self::Ps2,
        Self::Psp,
    ];

    /// Standard MiSTer FPGA directory name under `games/`.
    pub fn mister_folder_name(self) -> &'static str {
        match self {
            Self::Psx => "PSX",
            Self::Ps2 => "PS2",
            Self::Psp => "PSP",
            Self::Gba => "GBA",
            Self::Gbc => "GBC",
            Self::Gb => "GAMEBOY",
            Self::Nes => "NES",
            Self::Snes => "SNES",
            Self::N64 => "N64",
            Self::Nds => "NDS",
            Self::N3ds => "3DS",
            Self::GameCube => "GameCube",
            Self::VirtualBoy => "VirtualBoy",
            Self::Genesis => "Genesis",
            Self::Sega32x => "S32X",
            Self::SegaCd => "MegaCD",
            Self::Saturn => "Saturn",
            Self::Sms => "SMS",
            Self::GameGear => "GameGear",
            Self::Dreamcast => "Dreamcast",
            Self::Sg1000 => "SG1000",
            Self::PcEngine => "TGFX16",
            Self::PcEngineCd => "TGFX16-CD",
            Self::NeoGeo => "NEOGEO",
            Self::NeoGeoCd => "NeoGeo-CD",
            Self::NeoGeoPocket => "NGP",
            Self::WonderSwan => "WonderSwan",
            Self::Atari2600 => "ATARI2600",
            Self::Atari5200 => "ATARI5200",
            Self::Atari7800 => "ATARI7800",
            Self::AtariLynx => "AtariLynx",
            Self::AtariJaguar => "Jaguar",
            Self::ColecoVision => "Coleco",
            Self::Intellivision => "Intellivision",
            Self::Vectrex => "Vectrex",
            Self::Arcade => "Arcade",
            Self::Amiga => "Amiga",
            Self::C64 => "C64",
            Self::Msx => "MSX",
            Self::ZxSpectrum => "Spectrum",
            Self::X68000 => "X68000",
            Self::Pico8 => "PICO8",
        }
    }

    /// Parses a CLI or user-provided console name (e.g. `"PSX"`, `"gba"`, `"megacd"`).
    pub fn from_cli_str(s: &str) -> Option<Self> {
        detect_system_from_folder_name(s)
    }
}

/// Detects a console solely from an unambiguous ROM file extension (lowercase, without dot).
/// Shared extensions like `.chd`, `.cue`, `.iso`, `.bin`, `.zip`, `.rom` return `None`.
pub fn detect_system_from_extension(ext: &str) -> Option<SystemId> {
    match ext.to_ascii_lowercase().as_str() {
        "gba" => Some(SystemId::Gba),
        "gbc" => Some(SystemId::Gbc),
        "gb" => Some(SystemId::Gb),
        "nes" | "fds" | "unf" => Some(SystemId::Nes),
        "sfc" | "smc" | "bs" => Some(SystemId::Snes),
        "n64" | "z64" | "v64" => Some(SystemId::N64),
        "nds" => Some(SystemId::Nds),
        "3ds" => Some(SystemId::N3ds),
        "rvz" | "gcm" => Some(SystemId::GameCube),
        "vb" => Some(SystemId::VirtualBoy),
        "md" | "gen" | "smd" => Some(SystemId::Genesis),
        "32x" => Some(SystemId::Sega32x),
        "sms" => Some(SystemId::Sms),
        "gg" => Some(SystemId::GameGear),
        "sg" => Some(SystemId::Sg1000),
        "gdi" | "cdi" => Some(SystemId::Dreamcast),
        "pce" | "sgx" => Some(SystemId::PcEngine),
        "neo" => Some(SystemId::NeoGeo),
        "ngp" | "ngc" => Some(SystemId::NeoGeoPocket),
        "ws" | "wsc" => Some(SystemId::WonderSwan),
        "a26" => Some(SystemId::Atari2600),
        "a52" => Some(SystemId::Atari5200),
        "a78" => Some(SystemId::Atari7800),
        "lnx" => Some(SystemId::AtariLynx),
        "j64" | "jag" => Some(SystemId::AtariJaguar),
        "col" => Some(SystemId::ColecoVision),
        "int" => Some(SystemId::Intellivision),
        "vec" => Some(SystemId::Vectrex),
        "mra" => Some(SystemId::Arcade),
        "adf" | "hdf" => Some(SystemId::Amiga),
        "d64" | "t64" | "prg" | "crt" => Some(SystemId::C64),
        "mx1" | "mx2" => Some(SystemId::Msx),
        "tzx" | "sna" | "z80" => Some(SystemId::ZxSpectrum),
        "p8" => Some(SystemId::Pico8),
        _ => None,
    }
}

const GENERIC_FOLDER_NAMES: &[&str] = &[
    "media",
    "covers",
    "cover",
    "images",
    "imagenes",
    "boxarts",
    "boxart",
    "named boxarts",
    "named snaps",
    "named titles",
    "box2d",
    "box3d",
    "mix",
    "mixrbv2",
    "screenshots",
    "snaps",
    "titles",
    "games",
    "roms",
    "rpg",
    "action",
    "adventure",
    "sports",
    "racing",
    "fighting",
    "platformers",
    "puzzle",
    "shooter",
    "shooters",
    "usa",
    "europe",
    "japan",
    "world",
    "spain",
    "optimized",
    "favorites",
    "homebrew",
];

// Ordered from most specific to most general; matched only on whole-word boundaries (`" <phrase> "`).
const ORDERED_PHRASES: &[(&str, SystemId)] = &[
    // Handhelds & Nintendo (specific before general)
    ("game boy advance", SystemId::Gba),
    ("gameboy advance", SystemId::Gba),
    ("game boy color", SystemId::Gbc),
    ("gameboy color", SystemId::Gbc),
    ("game boy", SystemId::Gb),
    ("gameboy", SystemId::Gb),
    ("virtual boy", SystemId::VirtualBoy),
    ("super nintendo", SystemId::Snes),
    ("super famicom", SystemId::Snes),
    ("nintendo entertainment system", SystemId::Nes),
    ("nintendo 64", SystemId::N64),
    ("nintendo ds", SystemId::Nds),
    ("nintendo 3ds", SystemId::N3ds),
    ("nintendo gamecube", SystemId::GameCube),
    ("game cube", SystemId::GameCube),
    // Sony (specific before general)
    ("playstation portable", SystemId::Psp),
    ("playstation 2", SystemId::Ps2),
    ("sony playstation", SystemId::Psx),
    ("playstation", SystemId::Psx),
    ("ps one", SystemId::Psx),
    // Sega (CD & 32X before Genesis/Mega Drive)
    ("sega mega cd", SystemId::SegaCd),
    ("mega cd", SystemId::SegaCd),
    ("sega cd", SystemId::SegaCd),
    ("sega 32x", SystemId::Sega32x),
    ("mega drive", SystemId::Genesis),
    ("sega genesis", SystemId::Genesis),
    ("sega saturn", SystemId::Saturn),
    ("master system", SystemId::Sms),
    ("game gear", SystemId::GameGear),
    ("sega dreamcast", SystemId::Dreamcast),
    ("sg 1000", SystemId::Sg1000),
    // NEC (CD before base)
    ("pc engine cd", SystemId::PcEngineCd),
    ("turbografx cd", SystemId::PcEngineCd),
    ("tgfx16 cd", SystemId::PcEngineCd),
    ("pce cd", SystemId::PcEngineCd),
    ("turbografx 16", SystemId::PcEngine),
    ("pc engine", SystemId::PcEngine),
    ("supergrafx", SystemId::PcEngine),
    // SNK (Pocket & CD before Neo Geo)
    ("neo geo pocket", SystemId::NeoGeoPocket),
    ("neogeo pocket", SystemId::NeoGeoPocket),
    ("neo geo cd", SystemId::NeoGeoCd),
    ("neogeo cd", SystemId::NeoGeoCd),
    ("neo geo", SystemId::NeoGeo),
    // Bandai / Atari / Computers / Others
    ("wonderswan color", SystemId::WonderSwan),
    ("wonderswan", SystemId::WonderSwan),
    ("atari 2600", SystemId::Atari2600),
    ("atari 5200", SystemId::Atari5200),
    ("atari 7800", SystemId::Atari7800),
    ("atari lynx", SystemId::AtariLynx),
    ("atari jaguar", SystemId::AtariJaguar),
    ("coleco vision", SystemId::ColecoVision),
    ("commodore amiga", SystemId::Amiga),
    ("commodore 64", SystemId::C64),
    ("zx spectrum", SystemId::ZxSpectrum),
    ("sharp x68000", SystemId::X68000),
    ("pico 8", SystemId::Pico8),
];

// Unambiguous standalone tokens (3+ chars) that safely identify a console inside a folder name.
const STANDALONE_TOKENS: &[(&str, SystemId)] = &[
    ("psx", SystemId::Psx),
    ("ps1", SystemId::Psx),
    ("psone", SystemId::Psx),
    ("ps2", SystemId::Ps2),
    ("psp", SystemId::Psp),
    ("gba", SystemId::Gba),
    ("gbc", SystemId::Gbc),
    ("nes", SystemId::Nes),
    ("famicom", SystemId::Nes),
    ("snes", SystemId::Snes),
    ("sfc", SystemId::Snes),
    ("n64", SystemId::N64),
    ("nds", SystemId::Nds),
    ("3ds", SystemId::N3ds),
    ("gamecube", SystemId::GameCube),
    ("virtualboy", SystemId::VirtualBoy),
    ("genesis", SystemId::Genesis),
    ("megadrive", SystemId::Genesis),
    ("s32x", SystemId::Sega32x),
    ("32x", SystemId::Sega32x),
    ("megacd", SystemId::SegaCd),
    ("segacd", SystemId::SegaCd),
    ("saturn", SystemId::Saturn),
    ("sms", SystemId::Sms),
    ("gamegear", SystemId::GameGear),
    ("dreamcast", SystemId::Dreamcast),
    ("sg1000", SystemId::Sg1000),
    ("tgfx16", SystemId::PcEngine),
    ("turbografx", SystemId::PcEngine),
    ("pcecd", SystemId::PcEngineCd),
    ("neogeo", SystemId::NeoGeo),
    ("neogeocd", SystemId::NeoGeoCd),
    ("ngp", SystemId::NeoGeoPocket),
    ("ngpc", SystemId::NeoGeoPocket),
    ("wonderswan", SystemId::WonderSwan),
    ("atari2600", SystemId::Atari2600),
    ("atari5200", SystemId::Atari5200),
    ("atari7800", SystemId::Atari7800),
    ("atarilynx", SystemId::AtariLynx),
    ("jaguar", SystemId::AtariJaguar),
    ("coleco", SystemId::ColecoVision),
    ("colecovision", SystemId::ColecoVision),
    ("intellivision", SystemId::Intellivision),
    ("vectrex", SystemId::Vectrex),
    ("arcade", SystemId::Arcade),
    ("mame", SystemId::Arcade),
    ("fbneo", SystemId::Arcade),
    ("amiga", SystemId::Amiga),
    ("c64", SystemId::C64),
    ("msx", SystemId::Msx),
    ("msx2", SystemId::Msx),
    ("zxspectrum", SystemId::ZxSpectrum),
    ("x68000", SystemId::X68000),
    ("pico8", SystemId::Pico8),
];

// Ultra-short 2-3 char aliases allowed ONLY when the entire normalized folder/tag equals the alias.
const EXACT_ONLY_ALIASES: &[(&str, SystemId)] = &[
    ("gb", SystemId::Gb),
    ("md", SystemId::Genesis),
    ("gg", SystemId::GameGear),
    ("fc", SystemId::Nes),
    ("ws", SystemId::WonderSwan),
    ("wsc", SystemId::WonderSwan),
    ("vb", SystemId::VirtualBoy),
    ("dc", SystemId::Dreamcast),
    ("gc", SystemId::GameCube),
    ("ngc", SystemId::GameCube),
    ("ss", SystemId::Saturn),
    ("pce", SystemId::PcEngine),
    ("lynx", SystemId::AtariLynx),
    ("spectrum", SystemId::ZxSpectrum),
];

fn normalize_alnum_words(s: &str) -> String {
    let mut spaced = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            spaced.push(ch.to_ascii_lowercase());
        } else {
            spaced.push(' ');
        }
    }
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Detects a console from a folder name or tag segment while avoiding substring false positives
/// (for instance, `"64GB SD"` returns `None`).
pub fn detect_system_from_folder_name(name: &str) -> Option<SystemId> {
    let norm = normalize_alnum_words(name);
    if norm.is_empty() || GENERIC_FOLDER_NAMES.contains(&norm.as_str()) {
        return None;
    }

    if let Some((_, sys)) = EXACT_ONLY_ALIASES.iter().find(|&&(a, _)| a == norm) {
        return Some(*sys);
    }

    let padded = format!(" {norm} ");
    for &(phrase, sys) in ORDERED_PHRASES {
        let needle = format!(" {phrase} ");
        if padded.contains(&needle) {
            return Some(sys);
        }
    }

    for tok in norm.split_whitespace() {
        if let Some((_, sys)) = STANDALONE_TOKENS.iter().find(|&&(t, _)| t == tok) {
            return Some(*sys);
        }
    }

    None
}

/// Inspects explicit parenthesized/bracketed tags or PlayStation disc serials inside a stem.
pub fn detect_system_from_stem_tags(stem: &str) -> Option<SystemId> {
    let lower = stem.to_ascii_lowercase();
    for serial_prefix in [
        "slus-", "scus-", "sles-", "sces-", "slps-", "slpm-", "scps-",
    ] {
        if lower.contains(serial_prefix) {
            return Some(SystemId::Psx);
        }
    }

    for (open, close) in [('(', ')'), ('[', ']')] {
        let mut rest = stem;
        while let Some(start) = rest.find(open) {
            let after = &rest[start + 1..];
            let Some(end) = after.find(close) else {
                break;
            };
            let inside = &after[..end];
            if let Some(sys) = detect_system_from_folder_name(inside) {
                return Some(sys);
            }
            rest = &after[end + 1..];
        }
    }

    None
}

fn path_basename_any_sep(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
}

/// Infers the console of a game using its ROM extension, subfolder names, games root folder name,
/// and explicit stem tags.
pub fn detect_game_system(
    games_root: &str,
    relative_dir: &str,
    stem: &str,
    ext: &str,
) -> Option<SystemId> {
    if let Some(sys) = detect_system_from_extension(ext) {
        return Some(sys);
    }

    for segment in relative_dir.split(['/', '\\']).rev() {
        if !segment.is_empty()
            && let Some(sys) = detect_system_from_folder_name(segment)
        {
            return Some(sys);
        }
    }

    let root_name = path_basename_any_sep(games_root);
    if !root_name.is_empty()
        && let Some(sys) = detect_system_from_folder_name(root_name)
    {
        return Some(sys);
    }

    detect_system_from_stem_tags(stem)
}

/// Infers the console of a local cover image from its subfolder path relative to `images_root`,
/// `images_root`'s own folder name (never inspecting ancestors above `images_root`), and stem tags.
pub fn detect_cover_system(
    images_root: &Path,
    cover_path: &Path,
    stem: &str,
) -> Option<SystemId> {
    if let Some(parent) = cover_path.parent()
        && let Ok(rel) = parent.strip_prefix(images_root)
    {
        let segments: Vec<_> = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(os) => Some(os.to_string_lossy()),
                _ => None,
            })
            .collect();
        for seg in segments.into_iter().rev() {
            if let Some(sys) = detect_system_from_folder_name(&seg) {
                return Some(sys);
            }
        }
    }

    if let Some(root_name) = images_root.file_name()
        && let Some(sys) = detect_system_from_folder_name(&root_name.to_string_lossy())
    {
        return Some(sys);
    }

    detect_system_from_stem_tags(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_similar_console_names_without_false_positives() {
        assert_eq!(detect_system_from_folder_name("GBA"), Some(SystemId::Gba));
        assert_eq!(
            detect_system_from_folder_name("Nintendo - Game Boy Advance"),
            Some(SystemId::Gba)
        );
        assert_eq!(
            detect_system_from_folder_name("Nintendo - Game Boy Color"),
            Some(SystemId::Gbc)
        );
        assert_eq!(
            detect_system_from_folder_name("Nintendo - Game Boy"),
            Some(SystemId::Gb)
        );
        assert_eq!(detect_system_from_folder_name("GB"), Some(SystemId::Gb));
        assert_eq!(
            detect_system_from_folder_name("sony playstation"),
            Some(SystemId::Psx)
        );
        assert_eq!(
            detect_system_from_folder_name("Sony - PlayStation 2"),
            Some(SystemId::Ps2)
        );
        assert_eq!(
            detect_system_from_folder_name("Sega - Mega-CD - Sega CD"),
            Some(SystemId::SegaCd)
        );
        assert_eq!(
            detect_system_from_folder_name("NEC - PC Engine CD - TurboGrafx-CD"),
            Some(SystemId::PcEngineCd)
        );
        assert_eq!(detect_system_from_folder_name("64GB SD"), None);
        assert_eq!(detect_system_from_folder_name("Named_Boxarts"), None);
        assert_eq!(detect_system_from_folder_name("RPG"), None);
    }

    #[test]
    fn detects_game_system_from_extension_folder_and_tags() {
        assert_eq!(
            detect_game_system(r"D:\games\All", "", "Doom (USA)", "gba"),
            Some(SystemId::Gba)
        );
        assert_eq!(
            detect_game_system("/media/fat/games/PSX", "RPG", "Final Fantasy VII (USA)", "chd"),
            Some(SystemId::Psx)
        );
        assert_eq!(
            detect_game_system(r"D:\games\Mixed", "GBA", "Doom (USA)", "zip"),
            Some(SystemId::Gba)
        );
        assert_eq!(
            detect_game_system(r"D:\games\Mixed", "", "Doom [PSX]", "chd"),
            Some(SystemId::Psx)
        );
    }

    #[test]
    fn detect_cover_system_never_inspects_ancestors_above_images_root() {
        let images_root = Path::new(r"C:\Users\Test\PSX\Covers");
        let cover = Path::new(r"C:\Users\Test\PSX\Covers\Named_Boxarts\Doom (USA).png");
        // "PSX" is an ancestor ABOVE images_root, so it must NOT leak into the cover's system.
        assert_eq!(detect_cover_system(images_root, cover, "Doom (USA)"), None);

        let multi_root = Path::new(r"C:\Covers");
        let psx_cover = Path::new(r"C:\Covers\Sony - PlayStation\Named_Boxarts\Doom (USA).png");
        let gba_cover = Path::new(r"C:\Covers\Nintendo - Game Boy Advance\Doom (USA).png");
        assert_eq!(
            detect_cover_system(multi_root, psx_cover, "Doom (USA)"),
            Some(SystemId::Psx)
        );
        assert_eq!(
            detect_cover_system(multi_root, gba_cover, "Doom (USA)"),
            Some(SystemId::Gba)
        );
    }

    #[test]
    fn mister_folder_names_round_trip_and_all_has_unique_variants() {
        let mut seen = std::collections::HashSet::new();
        for &sys in SystemId::ALL {
            assert!(seen.insert(sys), "Duplicate SystemId in ALL: {sys:?}");
            let folder = sys.mister_folder_name();
            assert_eq!(
                detect_system_from_folder_name(folder),
                Some(sys),
                "Folder '{folder}' failed round-trip for {sys:?}"
            );
            assert_eq!(SystemId::from_cli_str(folder), Some(sys));
        }
        assert_eq!(SystemId::ALL.len(), 42);
    }
}
