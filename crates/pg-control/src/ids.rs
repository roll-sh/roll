const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Short, unambiguous, prefix-typed ids: `prj_x4k9...`, `br_...`, `ep_...`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_string(12))
}

pub fn new_password() -> String {
    random_string(24)
}

fn random_string(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}
