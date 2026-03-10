//! Text analysis pipeline for Chronik search indexes.
//!
//! Registers a custom Tantivy tokenizer with stop word removal and stemming
//! to improve full-text search quality (BM25 NDCG).
//!
//! The analyzer overrides Tantivy's built-in "default" tokenizer so all TEXT
//! and JSON fields automatically benefit without schema changes.

use tantivy::tokenizer::{LowerCaser, SimpleTokenizer, Stemmer, StopWordFilter, TextAnalyzer};
use tantivy::Index;

/// The name used to register the analyzer. We override "default" so all
/// TEXT / JSON fields pick it up without schema changes.
pub const ANALYZER_NAME: &str = "default";

/// English stop words — standard IR list (~127 words).
/// These are high-frequency function words that dilute term weights in BM25.
const ENGLISH_STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "shall",
    "should", "may", "might", "must", "can", "could",
    "of", "in", "to", "for", "with", "on", "at", "by", "from", "as",
    "into", "through", "during", "before", "after", "above", "below",
    "between", "out", "off", "over", "under", "again", "further", "then",
    "once",
    "and", "but", "or", "nor", "not", "so", "yet",
    "both", "either", "neither", "each", "every", "all", "any", "few",
    "more", "most", "other", "some", "such", "no", "only", "own", "same",
    "than", "too", "very", "just", "because",
    "it", "its", "this", "that", "these", "those",
    "i", "you", "he", "she", "we", "they",
    "what", "which", "who", "whom", "how", "where", "when", "why",
    "am", "if", "about", "up", "down", "here", "there",
];

/// Build the Chronik text analyzer pipeline.
///
/// Pipeline: SimpleTokenizer → LowerCaser → StopWordFilter → Stemmer (English)
///
/// The language is currently hardcoded to English. Per-topic language
/// configuration is planned for a future release (see ROADMAP_SEARCH_QUALITY.md).
pub fn build_analyzer() -> TextAnalyzer {
    let stop_words: Vec<String> = ENGLISH_STOP_WORDS.iter().map(|s| s.to_string()).collect();

    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(StopWordFilter::remove(stop_words))
        .filter(Stemmer::new(tantivy::tokenizer::Language::English))
        .build()
}

/// Register the Chronik text analyzer on a Tantivy index.
///
/// Must be called on every `Index` instance — both at index time and at
/// query time — so that the same analyzer is used for tokenizing documents
/// and queries. Tantivy indexes don't persist tokenizer registrations.
pub fn register_analyzer(index: &Index) {
    index.tokenizers().register(ANALYZER_NAME, build_analyzer());
}

/// Check if a query text produces any tokens after analysis.
/// Returns false if all terms are stop words (e.g., "the is a").
pub fn has_tokens(text: &str) -> bool {
    let mut analyzer = build_analyzer();
    let mut stream = analyzer.token_stream(text);
    stream.advance()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyzer_stems_english() {
        let mut analyzer = build_analyzer();
        let mut stream = analyzer.token_stream("running shoes batteries");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        // "running" -> "run", "shoes" -> "shoe", "batteries" -> "batteri"
        assert_eq!(tokens, vec!["run", "shoe", "batteri"]);
    }

    #[test]
    fn test_analyzer_removes_stop_words() {
        let mut analyzer = build_analyzer();
        let mut stream = analyzer.token_stream("the quick brown fox is very fast");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        // "the", "is", "very" removed
        assert_eq!(tokens, vec!["quick", "brown", "fox", "fast"]);
    }

    #[test]
    fn test_analyzer_lowercases() {
        let mut analyzer = build_analyzer();
        let mut stream = analyzer.token_stream("Leather SOFA");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["leather", "sofa"]);
    }

    #[test]
    fn test_stop_word_only_query_produces_empty() {
        let mut analyzer = build_analyzer();
        let mut stream = analyzer.token_stream("the is a");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert!(tokens.is_empty(), "All stop words should be removed");
    }

    #[test]
    fn test_register_on_index() {
        let schema = tantivy::schema::Schema::builder().build();
        let index = Index::create_in_ram(schema);
        register_analyzer(&index);
        // Verify the tokenizer is registered by trying to get it
        let tokenizer = index.tokenizers().get(ANALYZER_NAME);
        assert!(tokenizer.is_some(), "Analyzer should be registered");
    }
}
