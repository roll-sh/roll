//! Number formatting and parsing exactly as Redis does it, so replies match byte for byte.

/// Format a double like Redis' `d2string` (Redis 7.2+): integral values in the `long long` range
/// print as integers, everything else as the shortest round-trip form in fpconv's notation
/// (`0.1`, `1e+20`, `1e-7`), and `inf` / `-inf` / `nan`.
pub fn fmt_double(d: f64) -> String {
    if d.is_nan() {
        return "nan".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    // Redis (`double2ll`): integral values within ±LLONG_MAX/2 (2^62) print as integers.
    if d.fract() == 0.0 && (-4.611_686_018_427_388e18..=4.611_686_018_427_388e18).contains(&d) {
        return (d as i64).to_string();
    }
    fpconv(d)
}

/// fpconv_dtoa: Grisu2 digits (not always the shortest or closest, so they're generated exactly
/// as the reference does), then the `emit_digits` layout.
fn fpconv(d: f64) -> String {
    let (digits, k) = grisu2(d.abs());
    let digits = String::from_utf8(digits).unwrap();
    let nd = digits.len() as i32;
    let neg = d < 0.0;
    let mut out = String::with_capacity(24);
    if neg {
        out.push('-');
    }
    let absexp = (k + nd - 1).abs();
    if k >= 0 && absexp < nd + 7 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', k as usize));
        return out;
    }
    if k < 0 && (k > -7 || absexp < 4) {
        let offset = nd - k.abs();
        if offset <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-offset) as usize));
            out.push_str(&digits);
        } else {
            out.push_str(&digits[..offset as usize]);
            out.push('.');
            out.push_str(&digits[offset as usize..]);
        }
        return out;
    }
    let nd = nd.min(18 - neg as i32) as usize;
    out.push_str(&digits[..1]);
    if nd > 1 {
        out.push('.');
        out.push_str(&digits[1..nd]);
    }
    out.push('e');
    out.push(if k + digits.len() as i32 - 1 < 0 { '-' } else { '+' });
    let mut x = absexp;
    let mut cent = 0;
    if x > 99 {
        cent = x / 100;
        out.push((b'0' + cent as u8) as char);
        x -= cent * 100;
    }
    if x > 9 {
        let dec = x / 10;
        out.push((b'0' + dec as u8) as char);
        x -= dec * 10;
    } else if cent != 0 {
        out.push('0');
    }
    out.push((b'0' + (x % 10) as u8) as char);
    out
}

#[derive(Clone, Copy)]
struct Fp {
    frac: u64,
    exp: i32,
}

const FRACMASK: u64 = 0x000F_FFFF_FFFF_FFFF;
const EXPMASK: u64 = 0x7FF0_0000_0000_0000;
const HIDDENBIT: u64 = 0x0010_0000_0000_0000;
const EXPBIAS: i32 = 1023 + 52;

const TENS: [u64; 20] = [
    10000000000000000000,
    1000000000000000000,
    100000000000000000,
    10000000000000000,
    1000000000000000,
    100000000000000,
    10000000000000,
    1000000000000,
    100000000000,
    10000000000,
    1000000000,
    100000000,
    10000000,
    1000000,
    100000,
    10000,
    1000,
    100,
    10,
    1,
];

/// `fpconv_powers.h`: normalised powers of ten from 10^-348 in steps of 10^8.
const POWERS_TEN: [(u64, i32); 87] = [
    (18054884314459144840, -1220), (13451937075301367670, -1193), (10022474136428063862, -1166),
    (14934650266808366570, -1140), (11127181549972568877, -1113), (16580792590934885855, -1087),
    (12353653155963782858, -1060), (18408377700990114895, -1034), (13715310171984221708, -1007),
    (10218702384817765436, -980), (15227053142812498563, -954), (11345038669416679861, -927),
    (16905424996341287883, -901), (12595523146049147757, -874), (9384396036005875287, -847),
    (13983839803942852151, -821), (10418772551374772303, -794), (15525180923007089351, -768),
    (11567161174868858868, -741), (17236413322193710309, -715), (12842128665889583758, -688),
    (9568131466127621947, -661), (14257626930069360058, -635), (10622759856335341974, -608),
    (15829145694278690180, -582), (11793632577567316726, -555), (17573882009934360870, -529),
    (13093562431584567480, -502), (9755464219737475723, -475), (14536774485912137811, -449),
    (10830740992659433045, -422), (16139061738043178685, -396), (12024538023802026127, -369),
    (17917957937422433684, -343), (13349918974505688015, -316), (9946464728195732843, -289),
    (14821387422376473014, -263), (11042794154864902060, -236), (16455045573212060422, -210),
    (12259964326927110867, -183), (18268770466636286478, -157), (13611294676837538539, -130),
    (10141204801825835212, -103), (15111572745182864684, -77), (11258999068426240000, -50),
    (16777216000000000000, -24), (12500000000000000000, 3), (9313225746154785156, 30), (13877787807814456755,
    56), (10339757656912845936, 83), (15407439555097886824, 109), (11479437019748901445, 136),
    (17105694144590052135, 162), (12744735289059618216, 189), (9495567745759798747, 216), (14149498560666738074,
    242), (10542197943230523224, 269), (15709099088952724970, 295), (11704190886730495818, 322),
    (17440603504673385349, 348), (12994262207056124023, 375), (9681479787123295682, 402), (14426529090290212157,
    428), (10748601772107342003, 455), (16016664761464807395, 481), (11933345169920330789, 508),
    (17782069995880619868, 534), (13248674568444952270, 561), (9871031767461413346, 588), (14708983551653345445,
    614), (10959046745042015199, 641), (16330252207878254650, 667), (12166986024289022870, 694),
    (18130221999122236476, 720), (13508068024458167312, 747), (10064294952495520794, 774), (14996968138956309548,
    800), (11173611982879273257, 827), (16649979327439178909, 853), (12405201291620119593, 880),
    (9242595204427927429, 907), (13772540099066387757, 933), (10261342003245940623, 960), (15290591125556738113,
    986), (11392378155556871081, 1013), (16975966327722178521, 1039), (12648080533535911531, 1066),
];

fn find_cachedpow10(exp: i32) -> (Fp, i32) {
    const ONE_LOG_TEN: f64 = 0.30102999566398114;
    let approx = (-(exp + 87) as f64 * ONE_LOG_TEN) as i32;
    let mut idx = ((approx + 348) / 8) as usize;
    loop {
        let current = exp + POWERS_TEN[idx].1 + 64;
        if current < -60 {
            idx += 1;
            continue;
        }
        if current > -32 {
            idx -= 1;
            continue;
        }
        let (frac, exp) = POWERS_TEN[idx];
        return (Fp { frac, exp }, -348 + idx as i32 * 8);
    }
}

fn build_fp(d: f64) -> Fp {
    let bits = d.to_bits();
    let mut fp = Fp { frac: bits & FRACMASK, exp: ((bits & EXPMASK) >> 52) as i32 };
    if fp.exp != 0 {
        fp.frac += HIDDENBIT;
        fp.exp -= EXPBIAS;
    } else {
        fp.exp = -EXPBIAS + 1;
    }
    fp
}

fn normalize(fp: &mut Fp) {
    while fp.frac & HIDDENBIT == 0 {
        fp.frac <<= 1;
        fp.exp -= 1;
    }
    let shift = 64 - 52 - 1;
    fp.frac <<= shift;
    fp.exp -= shift;
}

fn normalized_boundaries(fp: &Fp) -> (Fp, Fp) {
    let mut upper = Fp { frac: (fp.frac << 1) + 1, exp: fp.exp - 1 };
    while upper.frac & (HIDDENBIT << 1) == 0 {
        upper.frac <<= 1;
        upper.exp -= 1;
    }
    let u_shift = 64 - 52 - 2;
    upper.frac <<= u_shift;
    upper.exp -= u_shift;
    let l_shift = if fp.frac == HIDDENBIT { 2 } else { 1 };
    let mut lower = Fp { frac: (fp.frac << l_shift) - 1, exp: fp.exp - l_shift };
    lower.frac <<= lower.exp - upper.exp;
    lower.exp = upper.exp;
    (lower, upper)
}

fn multiply(a: &Fp, b: &Fp) -> Fp {
    const LOMASK: u64 = 0x0000_0000_FFFF_FFFF;
    let ah_bl = (a.frac >> 32) * (b.frac & LOMASK);
    let al_bh = (a.frac & LOMASK) * (b.frac >> 32);
    let al_bl = (a.frac & LOMASK) * (b.frac & LOMASK);
    let ah_bh = (a.frac >> 32) * (b.frac >> 32);
    let mut tmp = (ah_bl & LOMASK) + (al_bh & LOMASK) + (al_bl >> 32);
    tmp += 1 << 31;
    Fp { frac: ah_bh + (ah_bl >> 32) + (al_bh >> 32) + (tmp >> 32), exp: a.exp + b.exp + 64 }
}

fn round_digit(digits: &mut [u8], delta: u64, mut rem: u64, kappa: u64, frac: u64) {
    let last = digits.len() - 1;
    while rem < frac
        && delta - rem >= kappa
        && (rem.wrapping_add(kappa) < frac || frac - rem > rem.wrapping_add(kappa).wrapping_sub(frac))
    {
        digits[last] -= 1;
        rem += kappa;
    }
}

fn generate_digits(fp: &Fp, upper: &Fp, lower: &Fp, k: &mut i32) -> Vec<u8> {
    let wfrac = upper.frac - fp.frac;
    let mut delta = upper.frac - lower.frac;
    let shift = (-upper.exp) as u32;
    let one = 1u64 << shift;
    let mut part1 = upper.frac >> shift;
    let mut part2 = upper.frac & (one - 1);
    let mut digits = Vec::with_capacity(18);
    let mut kappa = 10;
    let mut divp = 10;
    while kappa > 0 {
        let div = TENS[divp];
        let digit = part1 / div;
        if digit != 0 || !digits.is_empty() {
            digits.push(digit as u8 + b'0');
        }
        part1 -= digit * div;
        kappa -= 1;
        let tmp = (part1 << shift).wrapping_add(part2);
        if tmp <= delta {
            *k += kappa;
            round_digit(&mut digits, delta, tmp, div << shift, wfrac);
            return digits;
        }
        divp += 1;
    }
    let mut unit = 18;
    loop {
        part2 = part2.wrapping_mul(10);
        delta = delta.wrapping_mul(10);
        kappa -= 1;
        let digit = part2 >> shift;
        if digit != 0 || !digits.is_empty() {
            digits.push(digit as u8 + b'0');
        }
        part2 &= one - 1;
        if part2 < delta {
            *k += kappa;
            round_digit(&mut digits, delta, part2, one, wfrac.wrapping_mul(TENS[unit]));
            return digits;
        }
        unit -= 1;
    }
}

/// Grisu2 (`fpconv_dtoa.c`): the digits and decimal exponent K (value = digits * 10^K) of a
/// positive finite double.
fn grisu2(d: f64) -> (Vec<u8>, i32) {
    let mut w = build_fp(d);
    let (mut lower, mut upper) = normalized_boundaries(&w);
    normalize(&mut w);
    let (cp, k) = find_cachedpow10(upper.exp);
    w = multiply(&w, &cp);
    upper = multiply(&upper, &cp);
    lower = multiply(&lower, &cp);
    lower.frac += 1;
    upper.frac -= 1;
    let mut kk = -k;
    let digits = generate_digits(&w, &upper, &lower, &mut kk);
    (digits, kk)
}

/// Parse a double the way Redis' `string2d` / `strtod` do: no surrounding whitespace, `inf`,
/// `+inf`, `-inf` and `infinity` accepted (any case), NaN rejected.
pub fn parse_double(s: &[u8]) -> Option<f64> {
    if s.is_empty() || s.len() > 5120 || s[0].is_ascii_whitespace() || s[s.len() - 1].is_ascii_whitespace() {
        return None;
    }
    let s = std::str::from_utf8(s).ok()?;
    let lower = s.to_ascii_lowercase();
    let body = lower.strip_prefix(['+', '-']).unwrap_or(&lower);
    let v = match body {
        "inf" | "infinity" => {
            if lower.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }
        _ => {
            // Rust accepts "nan", "inf" spellings and "1e5"; reject the ones strtod wouldn't produce
            // as numbers here.
            if body.contains("nan") || body.contains("inf") || body.starts_with('_') {
                return None;
            }
            if let Some(hex) = body.strip_prefix("0x") {
                let v = parse_hex_float(hex)?;
                if lower.starts_with('-') { -v } else { v }
            } else {
                s.parse::<f64>().ok()?
            }
        }
    };
    if v.is_nan() { None } else { Some(v) }
}

/// strtod accepts C99 hex floats (`0x1.8p3`).
fn parse_hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = match s.split_once('p') {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in int.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(exp))
}

/// `long double` formatting used by INCRBYFLOAT / HINCRBYFLOAT (`%.17Lf`, trailing zeros removed).
/// Callers that need extended precision pass the exact decimal string instead; this is the
/// fallback for values computed in binary.
pub fn fmt_long_double(d: f64) -> String {
    if d.is_infinite() {
        return if d > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let mut s = format!("{d:.17}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" { "0".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_redis_d2string() {
        for (d, want) in [
            (1.5, "1.5"),
            (0.1, "0.1"),
            (3.0, "3"),
            (-2.0, "-2"),
            (1e20, "1e+20"),
            (1e17, "100000000000000000"),
            (123456789012345678.0, "123456789012345680"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (-1e-7, "-1e-7"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (123.456, "123.456"),
            (0.30000000000000004, "0.30000000000000004"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (-0.0, "-0"),
            (9007199254740993.0, "9007199254740992"),
            (3.14159e-10, "3.14159e-10"),
            // Grisu2 picks a different (still round-tripping) last digit than the shortest form.
            (-997827.3926165526, "-9.978273926165527e+5"),
            (4611686018427387904.0, "4611686018427387904"),
            (5e18, "5e+18"),
            (-9e18, "-9e+18"),
        ] {
            assert_eq!(fmt_double(d), want, "{d:e}");
        }
    }

    #[test]
    fn parses_like_strtod() {
        assert_eq!(parse_double(b"1.5"), Some(1.5));
        assert_eq!(parse_double(b"-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_double(b"+Inf"), Some(f64::INFINITY));
        assert_eq!(parse_double(b"1e3"), Some(1000.0));
        assert_eq!(parse_double(b".5"), Some(0.5));
        assert_eq!(parse_double(b"0x10"), Some(16.0));
        assert_eq!(parse_double(b"nan"), None);
        assert_eq!(parse_double(b" 1"), None);
        assert_eq!(parse_double(b"1 "), None);
        assert_eq!(parse_double(b"abc"), None);
        assert_eq!(parse_double(b""), None);
    }
}
