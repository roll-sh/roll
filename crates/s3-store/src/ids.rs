//! Identifiers.

fn random_from(alphabet: &[u8], n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    // 256 is a multiple of 32 and of every alphabet length used here except 62: the slight bias
    // for secrets is irrelevant at 40 characters.
    bytes.iter().map(|b| alphabet[*b as usize % alphabet.len()] as char).collect()
}

const BASE32_LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
const BASE32_UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A blob id: 26 random lowercase base32 characters (130 bits).
pub fn blob_id() -> String {
    random_from(BASE32_LOWER, 26)
}

/// `s3/blobs/<first two chars>/<id>`: the shard spreads load over S3 key partitions, and
/// keys sort like ids.
pub fn blob_key(id: &str) -> String {
    format!("s3/blobs/{}/{id}", &id[..2.min(id.len())])
}

/// The blob id in a blob key.
pub fn blob_of_key(key: &str) -> Option<&str> {
    let id = key.strip_prefix("s3/blobs/")?.split('/').nth(1)?;
    (id.len() == 26 && id.bytes().all(|b| BASE32_LOWER.contains(&b))).then_some(id)
}

pub fn bucket_id() -> String {
    format!("bkt_{}", random_from(BASE32_LOWER, 12))
}

/// Upload ids sort by creation time: 12 hex digits of Unix ms, then 32 random hex digits.
pub fn upload_id(now_ms: i64) -> String {
    format!("{:012x}{}", now_ms.max(0), random_from(b"0123456789abcdef", 32))
}

/// Access key ids look like AWS's: 20 characters.
pub fn access_key_id() -> String {
    format!("RK{}", random_from(BASE32_UPPER, 18))
}

pub fn secret_key() -> String {
    random_from(ALNUM, 40)
}

/// `YYYY-MM-DD` (UTC) of a Unix ms time.
pub fn day_of(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", if m <= 2 { y + 1 } else { y })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let b = blob_id();
        assert_eq!(b.len(), 26);
        assert_eq!(blob_of_key(&blob_key(&b)), Some(b.as_str()));
        assert_eq!(blob_of_key("s3/blobs/ab/short"), None);
        assert_eq!(access_key_id().len(), 20);
        assert_eq!(secret_key().len(), 40);
        assert!(upload_id(1) < upload_id(2));
    }

    #[test]
    fn days() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_792_454_400_000), "2026-10-20");
        assert_eq!(day_of(951_782_400_000), "2000-02-29");
    }
}
