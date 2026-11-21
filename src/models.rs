use std::path::PathBuf;

use crate::systems::SystemId;

/// Represents a discovered ROM or disc image inside the target MiSTer games folder.
#[derive(Debug, Clone)]
pub struct GameEntry {
    pub full_path: String,
    pub parent_dir: String,
    pub relative_dir: String,
    pub file_name: String,
    pub stem: String,
    pub extension: String,
    pub system: Option<SystemId>,
}

/// Represents a local cover artwork file found in the images folder.
#[derive(Debug, Clone)]
pub struct CoverImage {
    pub path: PathBuf,
    pub file_name: String,
    pub stem: String,
    pub extension: String,
    pub system: Option<SystemId>,
}

/// Classification of how a cover was matched to a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Exact,
    NormalizedExact,
    TitleExact,
    Fuzzy,
    Manual,
}

impl MatchKind {
    /// Human-readable Spanish label for display in UI and CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "Exacto",
            Self::NormalizedExact => "Normalizado",
            Self::TitleExact => "Título exacto",
            Self::Fuzzy => "Similitud",
            Self::Manual => "Manual",
        }
    }
}

/// A candidate cover match with similarity score.
#[derive(Debug, Clone)]
pub struct MatchCandidate {
    pub cover_idx: usize,
    pub score: f64,
    pub kind: MatchKind,
}

/// Current lifecycle status for a game's cover sync item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchStatus {
    AlreadyExists,
    Matched,
    Unmatched,
    Applied,
    Failed(String),
}

impl MatchStatus {
    /// Short Spanish label for status badges.
    pub fn label(&self) -> &'static str {
        match self {
            Self::AlreadyExists => "Ya existe",
            Self::Matched => "Emparejado",
            Self::Unmatched => "Sin cover",
            Self::Applied => "Copiado",
            Self::Failed(_) => "Error",
        }
    }
}

/// Combined matching state and target output path for one game.
#[derive(Debug, Clone)]
pub struct MatchItem {
    pub game: GameEntry,
    pub target_media_dir: String,
    pub target_file_name: String,
    pub target_full_path: String,
    pub existing_cover_name: Option<String>,
    pub selected_cover_idx: Option<usize>,
    pub score: f64,
    pub match_kind: Option<MatchKind>,
    pub candidates: Vec<MatchCandidate>,
    pub status: MatchStatus,
    pub selected_for_apply: bool,
}

/// Output of scanning games + covers and linking candidates.
#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub games_root: String,
    pub is_sftp: bool,
    pub covers: Vec<CoverImage>,
    pub items: Vec<MatchItem>,
    pub system_mismatch_warning: Option<String>,
}

/// Aggregate counters produced by applying cover copies/uploads.
#[derive(Debug, Clone, Default)]
pub struct SyncSummary {
    pub total_games: usize,
    pub already_exists: usize,
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}
