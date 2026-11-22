use std::collections::{HashMap, HashSet};

use strsim::{jaro_winkler, normalized_levenshtein};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::models::{CoverImage, MatchCandidate, MatchKind};
use crate::systems::SystemId;

/// Pre-indexed collection of cover images for fast exact and fuzzy game matching.
pub struct CoverIndex {
    entries: Vec<IndexedCover>,
    exact_lower: HashMap<String, Vec<usize>>,
    norm_full: HashMap<String, Vec<usize>>,
    norm_full_no_conn: HashMap<String, Vec<usize>>,
    norm_title: HashMap<String, Vec<usize>>,
    norm_title_no_conn: HashMap<String, Vec<usize>>,
    token_index: HashMap<String, Vec<usize>>,
}

struct IndexedCover {
    file_name: String,
    system: Option<SystemId>,
    clean_title: String,
    tokens: HashSet<String>,
    numbers: Vec<String>,
    regions: Vec<&'static str>,
}

struct ParsedGameName {
    lower_stem: String,
    full_norm: String,
    full_norm_no_conn: String,
    clean_title: String,
    clean_title_no_conn: String,
    alt_no_prefix_title: Option<String>,
    tokens: HashSet<String>,
    numbers: Vec<String>,
    regions: Vec<&'static str>,
}

impl CoverIndex {
    /// Builds an index over all scanned cover images.
    pub fn new(covers: &[CoverImage]) -> Self {
        let mut entries = Vec::with_capacity(covers.len());
        let mut exact_lower: HashMap<String, Vec<usize>> = HashMap::new();
        let mut norm_full: HashMap<String, Vec<usize>> = HashMap::new();
        let mut norm_full_no_conn: HashMap<String, Vec<usize>> = HashMap::new();
        let mut norm_title: HashMap<String, Vec<usize>> = HashMap::new();
        let mut norm_title_no_conn: HashMap<String, Vec<usize>> = HashMap::new();
        let mut token_index: HashMap<String, Vec<usize>> = HashMap::new();

        for (idx, cover) in covers.iter().enumerate() {
            let parsed = parse_name(&cover.stem);
            exact_lower
                .entry(parsed.lower_stem)
                .or_default()
                .push(idx);
            if !parsed.full_norm.is_empty() {
                norm_full.entry(parsed.full_norm).or_default().push(idx);
            }
            if !parsed.full_norm_no_conn.is_empty() {
                norm_full_no_conn
                    .entry(parsed.full_norm_no_conn)
                    .or_default()
                    .push(idx);
            }
            if !parsed.clean_title.is_empty() {
                norm_title
                    .entry(parsed.clean_title.clone())
                    .or_default()
                    .push(idx);
            }
            if !parsed.clean_title_no_conn.is_empty() {
                norm_title_no_conn
                    .entry(parsed.clean_title_no_conn)
                    .or_default()
                    .push(idx);
            }
            if let Some(ref alt) = parsed.alt_no_prefix_title
                && !alt.is_empty()
                && *alt != parsed.clean_title
            {
                norm_title.entry(alt.clone()).or_default().push(idx);
            }

            for token in &parsed.tokens {
                if token.len() >= 2 {
                    token_index.entry(token.clone()).or_default().push(idx);
                }
            }

            entries.push(IndexedCover {
                file_name: cover.file_name.clone(),
                system: cover.system,
                clean_title: parsed.clean_title,
                tokens: parsed.tokens,
                numbers: parsed.numbers,
                regions: parsed.regions,
            });
        }

        Self {
            entries,
            exact_lower,
            norm_full,
            norm_full_no_conn,
            norm_title,
            norm_title_no_conn,
            token_index,
        }
    }

    /// Returns up to 5 best candidate covers sorted descending by confidence score.
    pub fn match_game(
        &self,
        game_stem: &str,
        game_system: Option<SystemId>,
        strict_system_match: bool,
    ) -> Vec<MatchCandidate> {
        if self.entries.is_empty() {
            return Vec::new();
        }

        let game = parse_name(game_stem);
        let mut best_by_cover: HashMap<usize, (f64, MatchKind)> = HashMap::new();

        // Tier 1: Case-insensitive exact stem match
        if let Some(indices) = self.exact_lower.get(&game.lower_stem) {
            for &idx in indices {
                if let Some(score) =
                    adjusted_system_score(1.0, game_system, self.entries[idx].system, strict_system_match)
                {
                    record_candidate(&mut best_by_cover, idx, score, MatchKind::Exact);
                }
            }
        }

        // Tier 2: Full normalized match (punctuation, Libretro '_', '&' -> 'and', diacritics)
        if let Some(indices) = self.norm_full.get(&game.full_norm) {
            for &idx in indices {
                if let Some(score) = adjusted_system_score(
                    0.98,
                    game_system,
                    self.entries[idx].system,
                    strict_system_match,
                ) {
                    record_candidate(&mut best_by_cover, idx, score, MatchKind::NormalizedExact);
                }
            }
        }
        if let Some(indices) = self.norm_full_no_conn.get(&game.full_norm_no_conn) {
            for &idx in indices {
                if let Some(score) = adjusted_system_score(
                    0.97,
                    game_system,
                    self.entries[idx].system,
                    strict_system_match,
                ) {
                    record_candidate(&mut best_by_cover, idx, score, MatchKind::NormalizedExact);
                }
            }
        }

        // Tier 3: Clean title match (region/disc tags stripped, articles reordered, Roman numerals)
        let mut title_matches = Vec::new();
        if let Some(indices) = self.norm_title.get(&game.clean_title) {
            title_matches.extend(indices.iter().copied());
        }
        if let Some(ref alt) = game.alt_no_prefix_title
            && let Some(indices) = self.norm_title.get(alt)
        {
            title_matches.extend(indices.iter().copied());
        }
        for idx in title_matches {
            let bonus = region_affinity_bonus(&game.regions, &self.entries[idx].regions);
            if let Some(score) = adjusted_system_score(
                0.94 + bonus,
                game_system,
                self.entries[idx].system,
                strict_system_match,
            ) {
                record_candidate(&mut best_by_cover, idx, score, MatchKind::TitleExact);
            }
        }
        if let Some(indices) = self.norm_title_no_conn.get(&game.clean_title_no_conn) {
            for &idx in indices {
                let bonus = region_affinity_bonus(&game.regions, &self.entries[idx].regions);
                if let Some(score) = adjusted_system_score(
                    0.935 + bonus,
                    game_system,
                    self.entries[idx].system,
                    strict_system_match,
                ) {
                    record_candidate(&mut best_by_cover, idx, score, MatchKind::TitleExact);
                }
            }
        }

        // Tier 4: Fuzzy similarity over token-pruned candidate pool
        if !game.clean_title.is_empty() {
            let candidate_indices = self.collect_fuzzy_candidates(&game.tokens);
            for idx in candidate_indices {
                // Skip covers already matched at higher tiers
                if best_by_cover.contains_key(&idx) {
                    continue;
                }
                let cover = &self.entries[idx];
                if cover.clean_title.is_empty() {
                    continue;
                }
                if !is_system_compatible(game_system, cover.system, strict_system_match) {
                    continue;
                }

                let jaro = jaro_winkler(&game.clean_title, &cover.clean_title);
                let lev = normalized_levenshtein(&game.clean_title, &cover.clean_title);
                let dice = token_dice(&game.tokens, &cover.tokens);
                let mut score = 0.35 * jaro + 0.35 * lev + 0.30 * dice;

                // Sequel-number guard: prevent matching e.g. "Game 2" to "Game 3" or "Game"
                if game.numbers != cover.numbers
                    && (!game.numbers.is_empty() || !cover.numbers.is_empty())
                {
                    score -= 0.22;
                } else if score >= 0.65 && regions_intersect(&game.regions, &cover.regions) {
                    score += 0.02;
                }

                score = score.clamp(0.0, 0.93);
                if let Some(adj_score) =
                    adjusted_system_score(score, game_system, cover.system, strict_system_match)
                    && adj_score >= 0.35
                {
                    record_candidate(&mut best_by_cover, idx, adj_score, MatchKind::Fuzzy);
                }
            }
        }

        let mut results: Vec<MatchCandidate> = best_by_cover
            .into_iter()
            .map(|(cover_idx, (score, kind))| MatchCandidate {
                cover_idx,
                score,
                kind,
            })
            .collect();

        results.sort_by(|a, b| {
            let score_ord = if (b.score - a.score).abs() < 1e-9 {
                std::cmp::Ordering::Equal
            } else {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            };
            score_ord
                .then_with(|| {
                    system_priority_rank(game_system, self.entries[a.cover_idx].system)
                        .cmp(&system_priority_rank(game_system, self.entries[b.cover_idx].system))
                })
                .then_with(|| {
                    self.entries[a.cover_idx]
                        .file_name
                        .to_lowercase()
                        .cmp(&self.entries[b.cover_idx].file_name.to_lowercase())
                })
        });
        results.truncate(5);
        results
    }

    fn collect_fuzzy_candidates(&self, game_tokens: &HashSet<String>) -> Vec<usize> {
        if self.entries.len() <= 400 {
            return (0..self.entries.len()).collect();
        }

        let mut set = HashSet::new();
        for token in game_tokens {
            if token.len() >= 2
                && let Some(indices) = self.token_index.get(token)
            {
                set.extend(indices.iter().copied());
            }
        }

        if set.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            set.into_iter().collect()
        }
    }
}

fn is_system_compatible(
    game_sys: Option<SystemId>,
    cover_sys: Option<SystemId>,
    strict: bool,
) -> bool {
    !strict || !matches!((game_sys, cover_sys), (Some(g), Some(c)) if g != c)
}

fn adjusted_system_score(
    raw_score: f64,
    game_sys: Option<SystemId>,
    cover_sys: Option<SystemId>,
    strict: bool,
) -> Option<f64> {
    if !is_system_compatible(game_sys, cover_sys, strict) {
        return None;
    }
    let penalty = if !strict && matches!((game_sys, cover_sys), (Some(g), Some(c)) if g != c) {
        0.08
    } else {
        0.0
    };
    Some((raw_score - penalty).max(0.0))
}

fn system_priority_rank(game_sys: Option<SystemId>, cover_sys: Option<SystemId>) -> u8 {
    match (game_sys, cover_sys) {
        (Some(g), Some(c)) if g == c => 0,
        (_, None) => 1,
        (None, _) => 1,
        _ => 2,
    }
}

fn record_candidate(
    best_by_cover: &mut HashMap<usize, (f64, MatchKind)>,
    cover_idx: usize,
    score: f64,
    kind: MatchKind,
) {
    best_by_cover
        .entry(cover_idx)
        .and_modify(|existing| {
            if score > existing.0 {
                *existing = (score, kind);
            }
        })
        .or_insert((score, kind));
}

fn parse_name(stem: &str) -> ParsedGameName {
    let lower_stem = stem.to_lowercase();
    let regions = extract_regions(stem);
    let full_norm = normalize_text(stem, false);
    let full_norm_no_conn = strip_connector_words(&full_norm);

    let without_tags = strip_bracketed_tags(stem);
    let no_disc = strip_trailing_disc_marker(&without_tags);
    let reordered = move_trailing_article(&no_disc);
    let clean_title = normalize_text(&reordered, true);
    let clean_title_no_conn = strip_connector_words(&clean_title);

    let alt_no_prefix_title = strip_leading_number_prefix(&no_disc).map(|stripped| {
        let reordered_alt = move_trailing_article(&stripped);
        normalize_text(&reordered_alt, true)
    });

    let tokens: HashSet<String> = clean_title
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let numbers = extract_numbers(&clean_title);

    ParsedGameName {
        lower_stem,
        full_norm,
        full_norm_no_conn,
        clean_title,
        clean_title_no_conn,
        alt_no_prefix_title,
        tokens,
        numbers,
        regions,
    }
}

fn strip_connector_words(normalized: &str) -> String {
    normalized
        .split_whitespace()
        .filter(|&w| w != "and")
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_diacritics(s: &str) -> String {
    s.nfd().filter(|&c| !is_combining_mark(c)).collect()
}

fn strip_bracketed_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    for ch in s.chars() {
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ => {
                if paren_depth == 0 && bracket_depth == 0 {
                    out.push(ch);
                }
            }
        }
    }
    out
}

fn strip_trailing_disc_marker(s: &str) -> String {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    for marker in [" - disc ", " - disk ", " - cd ", " disc ", " disk ", " cd "] {
        if let Some(pos) = lower.rfind(marker) {
            let suffix = lower[pos + marker.len()..].trim();
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit() || c == 'a' || c == 'b') {
                return trimmed[..pos].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

fn strip_leading_number_prefix(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let digit_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(0);
    if digit_end == 0 {
        return None;
    }
    let rest = &trimmed[digit_end..];
    for sep in [" - ", ". ", "_ ", ") ", ".- "] {
        if let Some(after) = rest.strip_prefix(sep) {
            let candidate = after.trim();
            if candidate.chars().any(|c| c.is_alphabetic()) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn move_trailing_article(s: &str) -> String {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    for (suffix, article) in [(", the", "the"), (", a", "a"), (", an", "an")] {
        if lower.ends_with(suffix) {
            let base = trimmed[..trimmed.len() - suffix.len()].trim();
            if !base.is_empty() {
                return format!("{article} {base}");
            }
        }
    }
    // Also handle "Title, The - Subtitle" -> "The Title - Subtitle"
    for (needle, article) in [(", the - ", "the"), (", a - ", "a"), (", an - ", "an")] {
        if let Some(pos) = lower.find(needle) {
            let head = trimmed[..pos].trim();
            let tail = trimmed[pos + needle.len()..].trim();
            if !head.is_empty() && !tail.is_empty() {
                return format!("{article} {head} {tail}");
            }
        }
    }
    trimmed.to_string()
}

fn normalize_text(s: &str, convert_roman: bool) -> String {
    let folded = strip_diacritics(&s.to_lowercase());
    let mut spaced = String::with_capacity(folded.len() + 8);

    for ch in folded.chars() {
        if ch == '&' {
            spaced.push_str(" and ");
        } else if ch.is_alphanumeric() {
            spaced.push(ch);
        } else {
            spaced.push(' ');
        }
    }

    let mut tokens: Vec<String> = Vec::new();
    for tok in spaced.split_whitespace() {
        if convert_roman {
            tokens.push(roman_to_arabic(tok).to_string());
        } else {
            tokens.push(tok.to_string());
        }
    }
    tokens.join(" ")
}

fn roman_to_arabic(tok: &str) -> &str {
    match tok {
        "ii" => "2",
        "iii" => "3",
        "iv" => "4",
        "v" => "5",
        "vi" => "6",
        "vii" => "7",
        "viii" => "8",
        "ix" => "9",
        "x" => "10",
        "xi" => "11",
        "xii" => "12",
        "xiii" => "13",
        "xiv" => "14",
        "xv" => "15",
        _ => tok,
    }
}

fn extract_numbers(clean_title: &str) -> Vec<String> {
    clean_title
        .split_whitespace()
        .filter(|tok| !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

fn extract_regions(stem: &str) -> Vec<&'static str> {
    let mut regions = Vec::new();
    let mut rest = stem;
    while let Some(open) = rest.find('(') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find(')') else {
            break;
        };
        let content = after_open[..close].to_ascii_lowercase();
        for part in content.split([',', '/', '-', ';', ' ']) {
            let tag = match part.trim() {
                "usa" | "us" | "u" => Some("usa"),
                "europe" | "eu" | "eur" | "e" => Some("europe"),
                "japan" | "jp" | "jpn" | "j" => Some("japan"),
                "world" | "w" => Some("world"),
                "spain" | "es" | "spa" => Some("spain"),
                "france" | "fr" | "fre" => Some("france"),
                "germany" | "de" | "ger" => Some("germany"),
                "italy" | "it" | "ita" => Some("italy"),
                "brazil" | "br" | "bra" => Some("brazil"),
                "korea" | "kr" | "kor" => Some("korea"),
                "australia" | "au" | "aus" => Some("australia"),
                "asia" | "as" => Some("asia"),
                _ => None,
            };
            if let Some(r) = tag
                && !regions.contains(&r)
            {
                regions.push(r);
            }
        }
        rest = &after_open[close + 1..];
    }
    regions
}

fn regions_intersect(a: &[&'static str], b: &[&'static str]) -> bool {
    a.iter().any(|r| b.contains(r))
}

fn region_affinity_bonus(game_regions: &[&'static str], cover_regions: &[&'static str]) -> f64 {
    if regions_intersect(game_regions, cover_regions) {
        0.03
    } else if cover_regions.contains(&"world") || cover_regions.is_empty() {
        0.015
    } else if cover_regions.contains(&"usa") || cover_regions.contains(&"europe") {
        0.005
    } else {
        0.0
    }
}

fn token_dice(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.iter().filter(|t| b.contains(*t)).count();
    (2.0 * shared as f64) / ((a.len() + b.len()) as f64)
}

/// Returns a canonical key for a game/cover name, after removing region tags,
/// disc markers, accents, articles, connector words and roman numeral differences.
/// Used both for cover matching and for detecting duplicate ROM names.
pub fn canonical_game_key(stem: &str) -> String {
    let parsed = parse_name(stem);
    if !parsed.clean_title.is_empty() {
        parsed.clean_title
    } else {
        parsed.full_norm
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn cover(name: &str) -> CoverImage {
        cover_with_sys(name, None)
    }

    fn cover_with_sys(name: &str, system: Option<SystemId>) -> CoverImage {
        let (stem, ext) = crate::scanner::split_stem_ext(name);
        CoverImage {
            path: PathBuf::from(name),
            file_name: name.to_string(),
            stem: stem.to_string(),
            extension: ext,
            system,
        }
    }

    #[test]
    fn matches_exact_case_insensitive() {
        let covers = vec![cover("Crash Bandicoot (USA).png")];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("crash bandicoot (usa)", None, true);
        assert_eq!(hits[0].kind, MatchKind::Exact);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn matches_libretro_underscore_and_ampersand() {
        let covers = vec![cover("Dungeons _ Dragons - Shadow over Mystara (USA).png")];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("Dungeons & Dragons: Shadow over Mystara (USA)", None, true);
        assert!(!hits.is_empty());
        assert!(hits[0].score >= 0.95);
        assert_eq!(hits[0].kind, MatchKind::NormalizedExact);
    }

    #[test]
    fn matches_roman_numerals_and_disc_tags_and_prefers_region() {
        let covers = vec![
            cover("Final Fantasy 7 (Japan).png"),
            cover("Final Fantasy VII (USA).png"),
        ];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("Final Fantasy 7 (USA) (Disc 1)", None, true);
        assert!(!hits.is_empty());
        assert_eq!(covers[hits[0].cover_idx].file_name, "Final Fantasy VII (USA).png");
        assert!(hits[0].score >= 0.96);
    }

    #[test]
    fn matches_trailing_article_and_accents() {
        let covers = vec![
            cover("The Legend of Zelda - A Link to the Past (USA).png"),
            cover("Pokemon Red Version (USA, Europe).png"),
        ];
        let index = CoverIndex::new(&covers);

        let zelda = index.match_game("Legend of Zelda, The - A Link to the Past (USA)", None, true);
        assert_eq!(
            covers[zelda[0].cover_idx].file_name,
            "The Legend of Zelda - A Link to the Past (USA).png"
        );
        assert!(zelda[0].score >= 0.96);

        let pokemon = index.match_game("Pokémon Red Version (USA)", None, true);
        assert_eq!(
            covers[pokemon[0].cover_idx].file_name,
            "Pokemon Red Version (USA, Europe).png"
        );
        assert!(pokemon[0].score >= 0.96);
    }

    #[test]
    fn sequel_number_guard_blocks_wrong_sequel() {
        let covers = vec![cover("Crash Bandicoot 3 - Warped (USA).png")];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("Crash Bandicoot 2 - Cortex Strikes Back (USA)", None, true);
        let top_score = hits.first().map(|c| c.score).unwrap_or(0.0);
        assert!(
            top_score < 0.78,
            "Expected sequel mismatch score < 0.78, got {top_score}"
        );
    }

    #[test]
    fn fuzzy_matches_minor_typo() {
        let covers = vec![cover("Castlevania - Symphony of the Night (USA).png")];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("Castelvania - Symphny of the Night (USA)", None, true);
        assert!(!hits.is_empty());
        assert!(hits[0].score >= 0.78);
    }

    #[test]
    fn strict_mode_blocks_psx_cover_for_gba_game() {
        let covers = vec![cover_with_sys("Doom (USA).png", Some(SystemId::Psx))];
        let index = CoverIndex::new(&covers);
        let strict_hits = index.match_game("Doom (USA)", Some(SystemId::Gba), true);
        assert!(
            strict_hits.is_empty(),
            "PSX cover must be blocked for GBA game in strict mode"
        );

        let relaxed_hits = index.match_game("Doom (USA)", Some(SystemId::Gba), false);
        assert_eq!(relaxed_hits.len(), 1);
        assert!(relaxed_hits[0].score < 0.95);
    }

    #[test]
    fn prefers_gba_cover_over_psx_cover_with_same_title() {
        let covers = vec![
            cover_with_sys("Doom (USA).png", Some(SystemId::Psx)),
            cover_with_sys("Doom (USA).png", Some(SystemId::Gba)),
        ];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("Doom (USA)", Some(SystemId::Gba), true);
        assert_eq!(hits.len(), 1);
        assert_eq!(covers[hits[0].cover_idx].system, Some(SystemId::Gba));
    }

    #[test]
    fn generic_cover_folder_still_matches_known_game_system() {
        let covers = vec![cover_with_sys("Mario Kart - Super Circuit (USA).png", None)];
        let index = CoverIndex::new(&covers);
        let hits = index.match_game("Mario Kart - Super Circuit (USA)", Some(SystemId::Gba), true);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn canonical_key_ignores_region_disc_and_roman_variants() {
        assert_eq!(
            canonical_game_key("Final Fantasy VII (USA)"),
            canonical_game_key("Final Fantasy 7 (Europe) (Disc 1)")
        );
    }

    #[test]
    fn canonical_key_keeps_sequel_numbers_distinct() {
        assert_ne!(
            canonical_game_key("Mario Bros 2 (USA)"),
            canonical_game_key("Mario Bros 3 (USA)")
        );
    }
}
