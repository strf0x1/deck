//! Searching a provider's fetched model list (ADR 0012 §4). Pure: the
//! settings panel owns no ranking of its own.
//!
//! The design choice: construct's search, so both surfaces find a model the
//! same way — every word present in the id or name, case-insensitive, ids
//! that start with the query first. Only tool-capable models are offered: an
//! agent session that picks one without tools breaks on its first tool call.

use deck_runtime::catalog::ModelInfo;

/// How many rows the panel draws at once; typing narrows past this.
pub const SHOWN: usize = 40;

/// Tool-capable models whose id or name contains every word of `query`,
/// prefix matches first.
pub fn search<'a>(models: &'a [ModelInfo], query: &str) -> Vec<&'a ModelInfo> {
    let q = query.to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut hits: Vec<&ModelInfo> = models
        .iter()
        .filter(|model| model.tools)
        .filter(|model| {
            let hay = format!("{} {}", model.id.to_lowercase(), model.name.to_lowercase());
            words.iter().all(|word| hay.contains(word))
        })
        .collect();
    hits.sort_by_key(|model| (!model.id.to_lowercase().starts_with(&q), model.id.clone()));
    hits
}

/// The rows to draw, and how many matched in all.
pub fn shown<'a>(models: &'a [ModelInfo], query: &str) -> (Vec<&'a ModelInfo>, usize) {
    let hits = search(models, query);
    let total = hits.len();
    (hits.into_iter().take(SHOWN).collect(), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, name: &str, tools: bool) -> ModelInfo {
        ModelInfo { id: id.into(), name: name.into(), tools, ..ModelInfo::default() }
    }

    fn all() -> Vec<ModelInfo> {
        vec![
            model("anthropic/claude-sonnet-5-5", "Claude Sonnet", true),
            model("openai/gpt-6", "GPT 6", true),
            model("qwen3-32b", "Qwen 3 32B", true),
            model("diffusion-image", "Image only", false),
        ]
    }

    #[test]
    fn every_word_matches_anywhere_in_the_id_or_name() {
        let models = all();
        assert_eq!(search(&models, "claude").len(), 1);
        assert_eq!(search(&models, "SONNET 5").len(), 1, "words may arrive in any order and case");
        assert_eq!(search(&models, "gpt").first().map(|m| m.id.as_str()), Some("openai/gpt-6"));
        assert!(search(&models, "zzz").is_empty());
        assert_eq!(search(&models, "").len(), 3, "an empty query lists every tool-capable model");
    }

    #[test]
    fn id_prefix_matches_sort_first_and_ids_call_tools() {
        let models = all();
        let hits = search(&models, "qwen");
        assert_eq!(hits.first().map(|m| m.id.as_str()), Some("qwen3-32b"));
        assert!(search(&models, "diffusion").is_empty(), "a model without tools is not offered");
    }

    #[test]
    fn the_panel_draws_a_bounded_page_and_reports_the_total() {
        let models: Vec<ModelInfo> = (0..SHOWN + 5).map(|i| model(&format!("model-{i:03}"), "", true)).collect();
        let (rows, total) = shown(&models, "");
        assert_eq!((rows.len(), total), (SHOWN, SHOWN + 5));
    }
}
