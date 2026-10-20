//! With ORACLE_DIR set, stems every `<language>.txt` word list there into `<language>.ours`
//! (for comparing with Postgres's `ts_lexize`).
#[test]
fn oracle() {
    let Ok(dir) = std::env::var("ORACLE_DIR") else { return };
    for lang in pg_snowball::LANGUAGES {
        let Ok(words) = std::fs::read_to_string(format!("{dir}/{lang}.txt")) else { continue };
        let out: String = words.lines().map(|w| pg_snowball::stem(lang, w).unwrap() + "\n").collect();
        std::fs::write(format!("{dir}/{lang}.ours"), out).unwrap();
    }
}
