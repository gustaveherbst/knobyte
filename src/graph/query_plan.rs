//! Query planning shared by graph search and scope assembly: stop words, identifier-aware
//! tokenisation, conservative stemming and per-term weights.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// One weighted search term.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTerm {
    /// Lowercase search token.
    pub term: String,
    /// Token exactly as the user typed it.
    pub raw: String,
    /// Distinctive identifiers are safe to treat as explicit symbol names.
    pub identifier_like: bool,
    /// Stems broaden recall but carry less evidence than literal query terms.
    pub stem: bool,
    /// Relative contribution to coverage and lexical scoring.
    pub weight: f64,
}

#[derive(Debug, Clone, Default)]
pub struct GraphQueryPlan {
    pub raw: String,
    pub terms: Vec<PlannedTerm>,
    pub explicit_identifiers: Vec<String>,
    pub asks_for_tests: bool,
}

/// Terms grouped by the token the user supplied (`compose`, `composed`, `compos` = 1 concept).
#[derive(Debug, Clone)]
pub struct QueryConcept {
    pub key: String,
    pub raw: String,
    pub weight: f64,
    /// (term, is_stem)
    pub terms: Vec<(String, bool)>,
}

/// English and code-question boilerplate that carries no repository signal.
pub const GRAPH_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "each", "every", "for", "from", "give", "had", "has", "have", "how", "i", "if", "in",
    "into", "is", "it", "its", "just", "may", "me", "might", "more", "my", "need", "no", "not",
    "of", "on", "only", "or", "our", "out", "should", "show", "so", "some", "such", "tell",
    "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those",
    "to", "up", "us", "used", "using", "want", "was", "we", "what", "when", "where", "which",
    "who", "why", "will", "with", "would", "you",
];
const EXTRA_STOP_WORDS: &[&str] = &["work", "works", "your"];

/// Generic code vocabulary: useful, but much less discriminative than domain words.
const LOW_SIGNAL_WORDS: &[&str] = &[
    "agent", "called", "class", "code", "declaration", "declarations", "file", "files",
    "function", "graph", "method", "node", "nodes", "object", "search", "source", "symbol",
    "symbols", "task",
];

pub fn is_stop_word(term: &str) -> bool {
    GRAPH_STOP_WORDS.contains(&term) || EXTRA_STOP_WORDS.contains(&term)
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid query regex"))
}

/// Whether a token looks deliberately identifier-shaped as the user typed it.
pub fn is_distinctive_identifier(token: &str) -> bool {
    static QUALIFIED: OnceLock<Regex> = OnceLock::new();
    if token.contains(['/', '\\']) {
        return true;
    }
    let mut chars = token.chars();
    if token.starts_with('#') && chars.nth(1).is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') {
        return true;
    }
    if re(&QUALIFIED, r"[A-Za-z0-9_$](?:\.|::|#)[A-Za-z0-9_$]").is_match(token) {
        return true;
    }
    if token.chars().any(|c| c == '_' || c == '$' || c.is_ascii_digit()) {
        return true;
    }
    let first_upper = token.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    if first_upper && token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return true;
    }
    token.chars().skip(1).any(|c| c.is_ascii_uppercase())
}

/// Preserve a compound identifier and expose its camelCase/snake_case components:
/// `BudgetLedger` -> `budgetledger`, `budget`, `ledger`.
pub fn identifier_components(raw: &str) -> Vec<String> {
    let fragments: Vec<&str> = raw.split(['/', '\\', '.', ':', '#', '-']).filter(|f| !f.is_empty()).collect();
    if fragments.len() > 1 || raw.contains(['/', '\\', '.', ':', '#', '-']) {
        let mut out: Vec<String> = Vec::new();
        for f in fragments {
            for c in fragment_components(f) {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        return out;
    }
    fragment_components(raw)
}

fn fragment_components(raw: &str) -> Vec<String> {
    let cleaned: String = raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$').collect();
    let cleaned = cleaned.trim_matches(|c| c == '$' || c == '_');
    if cleaned.is_empty() {
        return Vec::new();
    }
    let mut out = vec![cleaned.to_lowercase()];
    for part in crate::graph::chunks::identifier_components(cleaned) {
        if !out.contains(&part) {
            out.push(part);
        }
    }
    out
}

/// Conservative suffix stemming for identifier-oriented prefix search.
pub fn stem_variants(term: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |v: String| {
        if v.len() >= 3 && v != term && !out.contains(&v) {
            out.push(v);
        }
    };
    let n = term.len();
    if term.ends_with("ing") && n > 5 {
        let base = &term[..n - 3];
        add(base.to_string());
        add(format!("{}e", base));
        let b: Vec<char> = base.chars().collect();
        if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] {
            add(b[..b.len() - 1].iter().collect());
        }
    }
    if (term.ends_with("tion") || term.ends_with("sion")) && n >= 9 {
        add(term[..5].to_string());
    }
    if term.ends_with("ment") && n > 6 {
        add(term[..n - 4].to_string());
    }
    if term.ends_with("ies") && n > 4 {
        add(format!("{}y", &term[..n - 3]));
    } else if ["sses", "xes", "zes", "ches", "shes", "oes"].iter().any(|s| term.ends_with(s)) && n > 4 {
        add(term[..n - 2].to_string());
    } else if term.ends_with('s') && !term.ends_with("ss") && n > 4 {
        add(term[..n - 1].to_string());
    }
    if term.ends_with("ed") && n > 4 {
        add(term[..n - 1].to_string());
        add(term[..n - 2].to_string());
    }
    out
}

/// Turn a natural-language task into bounded, weighted index terms.
pub fn plan_graph_query(raw: &str) -> GraphQueryPlan {
    static TOKENS: OnceLock<Regex> = OnceLock::new();
    static TESTS: OnceLock<Regex> = OnceLock::new();
    let tokens: Vec<&str> = re(&TOKENS, r"[#A-Za-z_$][#A-Za-z0-9_$.:/\\-]*")
        .find_iter(raw)
        .map(|m| m.as_str())
        .collect();
    let mut order: Vec<String> = Vec::new();
    let mut terms: HashMap<String, PlannedTerm> = HashMap::new();
    let mut explicit: Vec<String> = Vec::new();
    let add = |e: PlannedTerm, order: &mut Vec<String>, terms: &mut HashMap<String, PlannedTerm>| {
        if e.term.len() < 2 || is_stop_word(&e.term) {
            return;
        }
        match terms.get(&e.term) {
            Some(cur) if cur.weight >= e.weight => {}
            Some(_) => {
                terms.insert(e.term.clone(), e);
            }
            None => {
                order.push(e.term.clone());
                terms.insert(e.term.clone(), e);
            }
        }
    };
    for raw_token in tokens.iter().take(40) {
        let raw_token = raw_token.trim_end_matches(['.', ':', '-', '/']);
        if raw_token.is_empty() {
            continue;
        }
        let distinctive = is_distinctive_identifier(raw_token);
        let all_upper = raw_token.chars().all(|c| c.is_ascii_uppercase());
        let strongly = raw_token.contains(['/', '\\', '.', ':', '#', '_', '$'])
            || raw_token.chars().any(|c| c.is_ascii_digit())
            || (!all_upper && raw_token.chars().skip(1).any(|c| c.is_ascii_uppercase()));
        let components = identifier_components(raw_token);
        if components.is_empty() {
            continue;
        }
        if distinctive && components.iter().any(|c| !is_stop_word(c)) && !explicit.iter().any(|e| e == raw_token) {
            explicit.push(raw_token.to_string());
        }
        let single = components.len() == 1;
        for (index, term) in components.iter().enumerate() {
            let low = LOW_SIGNAL_WORDS.contains(&term.as_str());
            let weight = if distinctive && index == 0 {
                if strongly {
                    2.0
                } else {
                    1.15
                }
            } else if low {
                0.35
            } else {
                1.0
            };
            add(
                PlannedTerm {
                    term: term.clone(),
                    raw: raw_token.to_string(),
                    identifier_like: distinctive && index == 0,
                    stem: false,
                    weight,
                },
                &mut order,
                &mut terms,
            );
            if index > 0 || single {
                for stem in stem_variants(term) {
                    let derivational = (term.ends_with("tion") || term.ends_with("sion"))
                        && term.len() >= 9
                        && stem == term[..5];
                    let w = if derivational {
                        if low {
                            0.08
                        } else {
                            0.2
                        }
                    } else if low {
                        0.15
                    } else {
                        0.55
                    };
                    add(
                        PlannedTerm {
                            term: stem,
                            raw: raw_token.to_string(),
                            identifier_like: false,
                            stem: true,
                            weight: w,
                        },
                        &mut order,
                        &mut terms,
                    );
                }
            }
        }
    }
    let planned: Vec<PlannedTerm> = order.iter().take(32).filter_map(|t| terms.get(t).cloned()).collect();
    explicit.truncate(16);
    GraphQueryPlan {
        raw: raw.to_string(),
        terms: planned,
        explicit_identifiers: explicit,
        asks_for_tests: re(&TESTS, r"(?i)\b(test|tests|testing|spec|specs|verify|verification)\b").is_match(raw),
    }
}

impl GraphQueryPlan {
    /// Literal (non-stem) terms, de-duplicated in plan order.
    pub fn literal_terms(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in &self.terms {
            if !t.stem && !out.contains(&t.term) {
                out.push(t.term.clone());
            }
        }
        out
    }

    /// Concepts: terms grouped by the raw token that produced them.
    pub fn concepts(&self) -> Vec<QueryConcept> {
        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, QueryConcept> = HashMap::new();
        for e in &self.terms {
            let key = e.raw.to_lowercase();
            let c = map.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                QueryConcept {
                    key: key.clone(),
                    raw: e.raw.clone(),
                    weight: 0.0,
                    terms: Vec::new(),
                }
            });
            c.weight = c.weight.max(e.weight);
            if !c.terms.iter().any(|(t, s)| t == &e.term && *s == e.stem) {
                c.terms.push((e.term.clone(), e.stem));
            }
        }
        order.into_iter().filter_map(|k| map.remove(&k)).collect()
    }
}

/// Lowercase identifier components of every identifier in `value`.
pub fn search_components(value: &str) -> HashSet<String> {
    static IDENT: OnceLock<Regex> = OnceLock::new();
    let mut out = HashSet::new();
    for m in re(&IDENT, r"[A-Za-z_$][A-Za-z0-9_$]*").find_iter(value) {
        for c in identifier_components(m.as_str()) {
            out.insert(c);
        }
    }
    out
}

pub fn component_matches(components: &HashSet<String>, term: &str, prefix: bool) -> bool {
    components.contains(term) || (prefix && term.len() >= 3 && components.iter().any(|c| c.starts_with(term)))
}

/// Inverse document frequency with add-one smoothing.
pub fn idf(documents: usize, frequency: usize) -> f64 {
    ((documents as f64 + 1.0) / (frequency as f64 + 1.0)).ln() + 1.0
}

/// Test, fixture, example, benchmark, generated, and other non-production paths.
pub fn is_low_value_graph_path(path: &str) -> bool {
    static DIRS: OnceLock<Regex> = OnceLock::new();
    static FILES: OnceLock<Regex> = OnceLock::new();
    let p = path.to_lowercase();
    re(
        &DIRS,
        r"(^|/)(__tests?__|tests?|specs?|fixtures?|examples?|samples?|benchmarks?|benches|demos?|mocks?|testdata|integration|evaluate|generated|vendor)(/|$)",
    )
    .is_match(&p)
        || re(
            &FILES,
            r"(\.(test|spec)\.[^/]+$)|((^|/)test_[^/]+\.[^/]+$)|(_test\.[^/]+$)|(\.(generated|gen|pb)\.[^/]+$)",
        )
        .is_match(&p)
}

/// Test paths only (a subset of low-value paths).
pub fn is_test_path(path: &str) -> bool {
    static TEST: OnceLock<Regex> = OnceLock::new();
    re(
        &TEST,
        r"(^|/)(__tests?__|tests?|specs?)(/|$)|\.(test|spec)\.[^/]+$|(^|/)test_[^/]+\.[^/]+$|_test\.[^/]+$|(^|/)[^/]+Tests?\.cs$",
    )
    .is_match(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_identifiers_stems_and_stop_words() {
        let p = plan_graph_query("How does the BudgetLedger handle configuration of tokens?");
        let terms: Vec<&str> = p.terms.iter().map(|t| t.term.as_str()).collect();
        assert!(terms.contains(&"budgetledger"));
        assert!(terms.contains(&"budget"));
        assert!(terms.contains(&"ledger"));
        assert!(terms.contains(&"confi"), "{:?}", terms);
        assert!(terms.contains(&"token"));
        assert!(!terms.contains(&"the"));
        assert_eq!(p.explicit_identifiers, vec!["BudgetLedger"]);
        assert!(!p.asks_for_tests);
        assert!(plan_graph_query("where are the tests for parse").asks_for_tests);
        assert!(plan_graph_query("the of and").terms.is_empty());
    }

    #[test]
    fn low_value_paths() {
        assert!(is_low_value_graph_path("tests/graph_test.rs"));
        assert!(is_low_value_graph_path("src/a.test.ts"));
        assert!(!is_low_value_graph_path("src/graph/scope.rs"));
    }
}
