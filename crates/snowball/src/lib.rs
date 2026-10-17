//! The Snowball stemmers (<https://snowballstem.org>, BSD-3-Clause, see `COPYING`): the Rust
//! runtime of the Snowball project and the algorithms its compiler generates from the `.sbl`
//! sources libstemmer is built from, so words stem exactly as libstemmer's UTF-8 stemmers do.
#![allow(clippy::all, clippy::pedantic, dead_code, unused_parens, non_snake_case, non_upper_case_globals, unused_variables, unused_mut, unused_assignments)]

pub mod algorithms;
mod runtime;

pub use runtime::SnowballEnv;

type Stem = fn(&mut SnowballEnv) -> bool;

/// The algorithm for a language (libstemmer's names).
fn algorithm(lang: &str) -> Option<Stem> {
    use algorithms::*;
    Some(match lang {
        "arabic" => arabic::stem,
        "armenian" => armenian::stem,
        "basque" => basque::stem,
        "catalan" => catalan::stem,
        "danish" => danish::stem,
        "dutch" => dutch::stem,
        "english" => english::stem,
        "finnish" => finnish::stem,
        "french" => french::stem,
        "german" => german::stem,
        "greek" => greek::stem,
        "hindi" => hindi::stem,
        "hungarian" => hungarian::stem,
        "indonesian" => indonesian::stem,
        "irish" => irish::stem,
        "italian" => italian::stem,
        "lithuanian" => lithuanian::stem,
        "nepali" => nepali::stem,
        "norwegian" => norwegian::stem,
        "portuguese" => portuguese::stem,
        "romanian" => romanian::stem,
        "russian" => russian::stem,
        "serbian" => serbian::stem,
        "spanish" => spanish::stem,
        "swedish" => swedish::stem,
        "tamil" => tamil::stem,
        "turkish" => turkish::stem,
        "yiddish" => yiddish::stem,
        _ => return None,
    })
}

/// Whether there is a stemmer for `lang`.
pub fn has(lang: &str) -> bool {
    algorithm(lang).is_some()
}

/// `sb_stemmer_stem`: the stem of `word` (None without a stemmer for `lang`).
pub fn stem(lang: &str, word: &str) -> Option<String> {
    let f = algorithm(lang)?;
    let mut env = SnowballEnv::create(word);
    f(&mut env);
    Some(env.get_current().into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn english() {
        assert_eq!(super::stem("english", "running").as_deref(), Some("run"));
        assert_eq!(super::stem("english", "generously").as_deref(), Some("generous"));
        assert_eq!(super::stem("english", "consign").as_deref(), Some("consign"));
        assert_eq!(super::stem("german", "aufeinanderfolgenden").as_deref(), Some("aufeinanderfolg"));
        assert!(super::stem("klingon", "x").is_none());
    }
}
