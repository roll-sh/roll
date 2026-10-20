//! Postgres's Snowball stemmers (`dict_snowball`), generated from Snowball commit
//! d19326ac6c1b9a417fc872f7c2f845265a5e9ece (the one Postgres 18 is synced with) by the
//! Snowball compiler's Rust backend. See COPYING for the Snowball license.

// Generated code and the Snowball project's runtime, kept as they come.
#[allow(clippy::all)]
mod snowball;

use snowball::SnowballEnv;
use snowball::algorithms::*;

/// The languages Postgres has Snowball stemmers for.
pub const LANGUAGES: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "catalan",
    "danish",
    "dutch",
    "english",
    "estonian",
    "finnish",
    "french",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "lithuanian",
    "nepali",
    "norwegian",
    "porter",
    "portuguese",
    "romanian",
    "russian",
    "serbian",
    "spanish",
    "swedish",
    "tamil",
    "turkish",
    "yiddish",
];

/// Stems a lowercased word with the stemmer for `language` (case-insensitive, as Postgres
/// looks it up); None: no such stemmer.
pub fn stem(language: &str, word: &str) -> Option<String> {
    let f: fn(&mut SnowballEnv) -> bool = match language.to_ascii_lowercase().as_str() {
        "arabic" => arabic_stemmer::stem,
        "armenian" => armenian_stemmer::stem,
        "basque" => basque_stemmer::stem,
        "catalan" => catalan_stemmer::stem,
        "danish" => danish_stemmer::stem,
        "dutch" => dutch_stemmer::stem,
        "english" => english_stemmer::stem,
        "estonian" => estonian_stemmer::stem,
        "finnish" => finnish_stemmer::stem,
        "french" => french_stemmer::stem,
        "german" => german_stemmer::stem,
        "greek" => greek_stemmer::stem,
        "hindi" => hindi_stemmer::stem,
        "hungarian" => hungarian_stemmer::stem,
        "indonesian" => indonesian_stemmer::stem,
        "irish" => irish_stemmer::stem,
        "italian" => italian_stemmer::stem,
        "lithuanian" => lithuanian_stemmer::stem,
        "nepali" => nepali_stemmer::stem,
        "norwegian" => norwegian_stemmer::stem,
        "porter" => porter_stemmer::stem,
        "portuguese" => portuguese_stemmer::stem,
        "romanian" => romanian_stemmer::stem,
        "russian" => russian_stemmer::stem,
        "serbian" => serbian_stemmer::stem,
        "spanish" => spanish_stemmer::stem,
        "swedish" => swedish_stemmer::stem,
        "tamil" => tamil_stemmer::stem,
        "turkish" => turkish_stemmer::stem,
        "yiddish" => yiddish_stemmer::stem,
        _ => return None,
    };
    let mut env = SnowballEnv::create(word);
    f(&mut env);
    Some(env.get_current().into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn english() {
        assert_eq!(super::stem("english", "running").as_deref(), Some("run"));
        assert_eq!(super::stem("English", "generously").as_deref(), Some("generous"));
        assert_eq!(super::stem("klingon", "x"), None);
    }
}
