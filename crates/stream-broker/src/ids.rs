const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Short, unambiguous, prefix-typed ids: `st_x4k9...` for streams, `rk_...` for API keys.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_string(12))
}

pub fn new_secret() -> String {
    random_string(32)
}

pub fn random_string(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("os randomness");
    b
}

pub fn new_uuid() -> uuid::Uuid {
    uuid::Builder::from_random_bytes(random_bytes()).into_uuid()
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}
