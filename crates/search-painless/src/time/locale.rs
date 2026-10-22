//! Locale data of `java.time`: month, day, quarter, am/pm and era names, the localized
//! date/time patterns behind `DateTimeFormatter.ofLocalized*`, and each locale's week
//! definition (`WeekFields.of(locale)`).
//!
//! The data was captured from Elasticsearch 9.5.5's JDK (its CLDR provider) for every
//! available locale into `locales.tsv`, with identical sets shared between locales; it is
//! parsed once on first use. Locales resolve like the text module's number data: the exact
//! `Locale.toString()`, then without script/extensions, language and country, language, and
//! finally the root locale.

use std::collections::HashMap;
use std::sync::OnceLock;

struct Profile {
    months: u16,
    days: u16,
    quarters: u16,
    ampm: u16,
    eras: u16,
    patterns: u16,
    first_day: u8,
    min_days: u8,
}

struct Data {
    months: Vec<Vec<&'static str>>,
    days: Vec<Vec<&'static str>>,
    quarters: Vec<Vec<&'static str>>,
    ampm: Vec<Vec<&'static str>>,
    eras: Vec<Vec<&'static str>>,
    patterns: Vec<Vec<&'static str>>,
    locales: HashMap<&'static str, Profile>,
}

fn data() -> &'static Data {
    static D: OnceLock<Data> = OnceLock::new();
    D.get_or_init(|| {
        let mut d = Data {
            months: Vec::new(),
            days: Vec::new(),
            quarters: Vec::new(),
            ampm: Vec::new(),
            eras: Vec::new(),
            patterns: Vec::new(),
            locales: HashMap::new(),
        };
        for line in include_str!("locales.tsv").lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            let set = || f[2..].to_vec();
            match f[0] {
                "M" => d.months.push(set()),
                "D" => d.days.push(set()),
                "Q" => d.quarters.push(set()),
                "A" => d.ampm.push(set()),
                "E" => d.eras.push(set()),
                "P" => d.patterns.push(set()),
                "L" => {
                    let n = |i: usize| f[i].parse::<u16>().unwrap_or(0);
                    d.locales.insert(
                        f[1],
                        Profile {
                            months: n(2),
                            days: n(3),
                            quarters: n(4),
                            ampm: n(5),
                            eras: n(6),
                            patterns: n(7),
                            first_day: n(8) as u8,
                            min_days: n(9) as u8,
                        },
                    );
                }
                _ => {}
            }
        }
        d
    })
}

fn profile(locale: &str) -> &'static Profile {
    let d = data();
    if let Some(p) = d.locales.get(locale) {
        return p;
    }
    let mut candidates = Vec::new();
    if let Some(i) = locale.find("_#") {
        candidates.push(locale[..i].to_string());
    }
    let parts: Vec<&str> = locale.split('_').collect();
    if parts.len() > 2 {
        candidates.push(format!("{}_{}", parts[0], parts[1]));
    }
    candidates.push(parts[0].to_string());
    for c in &candidates {
        if let Some(p) = d.locales.get(c.as_str()) {
            return p;
        }
    }
    &d.locales[""]
}

/// The style's column in a set: `FULL`, `SHORT`, `NARROW` and their standalone forms.
fn style_index(style: u8) -> usize {
    match style {
        0 => 0,
        2 => 1,
        4 => 2,
        1 => 3,
        3 => 4,
        _ => 5,
    }
}

/// Month name (`month` 1–12).
pub fn month(locale: &str, month: u8, style: u8) -> &'static str {
    let set = &data().months[profile(locale).months as usize];
    set[style_index(style) * 12 + (month as usize).clamp(1, 12) - 1]
}

/// Day-of-week name (`dow` 1 = Monday … 7 = Sunday).
pub fn day(locale: &str, dow: u8, style: u8) -> &'static str {
    let set = &data().days[profile(locale).days as usize];
    set[style_index(style) * 7 + (dow as usize).clamp(1, 7) - 1]
}

/// Quarter name (1–4).
pub fn quarter(locale: &str, q: u8, style: u8) -> &'static str {
    let set = &data().quarters[profile(locale).quarters as usize];
    set[style_index(style) * 4 + (q as usize).clamp(1, 4) - 1]
}

/// AM (0) or PM (1). Elasticsearch's JDK has one form for every style.
pub fn ampm(locale: &str, v: u8) -> &'static str {
    data().ampm[profile(locale).ampm as usize][(v as usize).min(1)]
}

/// Era name: 0 = BC (BCE), 1 = AD (CE). Standalone styles use the format names.
pub fn era(locale: &str, v: u8, style: u8) -> &'static str {
    let col = match style {
        0 | 1 => 0,
        2 | 3 => 1,
        _ => 2,
    };
    data().eras[profile(locale).eras as usize][col * 2 + (v as usize).min(1)]
}

/// `DateTimeFormatterBuilder.getLocalizedDateTimePattern` for the ISO chronology: `date` and
/// `time` are `FormatStyle` ordinals (`FULL` = 0 … `SHORT` = 3), at least one present.
pub fn localized_pattern(locale: &str, date: Option<u8>, time: Option<u8>) -> &'static str {
    let set = &data().patterns[profile(locale).patterns as usize];
    match (date, time) {
        (Some(d), None) => set[d.min(3) as usize],
        (None, Some(t)) => set[4 + t.min(3) as usize],
        (Some(d), Some(t)) => set[8 + d.min(3) as usize * 4 + t.min(3) as usize],
        (None, None) => "",
    }
}

/// `WeekFields.of(locale)`: first day of the week (1 = Monday) and minimal days in the first
/// week.
pub fn week_def(locale: &str) -> (u8, u8) {
    let p = profile(locale);
    (p.first_day, p.min_days)
}


// ------------------------------------------------------------------------------ zone names

struct Zones {
    /// en_US names by zone id: long standard, long daylight, short standard, short daylight.
    base: HashMap<&'static str, [&'static str; 4]>,
    /// Other English profiles: differences from en_US.
    english: Vec<HashMap<&'static str, [&'static str; 4]>>,
    /// Other profiles: names of the UTC zones, then of the GMT zones.
    utc_gmt: Vec<[&'static str; 8]>,
    /// Locale → (English profile?, profile index).
    locales: HashMap<&'static str, (bool, usize)>,
}

fn zones() -> &'static Zones {
    static Z: OnceLock<Zones> = OnceLock::new();
    Z.get_or_init(|| {
        let mut z = Zones { base: HashMap::new(), english: Vec::new(), utc_gmt: Vec::new(), locales: HashMap::new() };
        let four = |f: &[&'static str]| [f[0], f[1], f[2], f[3]];
        for line in include_str!("zones.tsv").lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            match f[0] {
                "N" => {
                    z.base.insert(f[1], four(&f[2..]));
                }
                "D" => {
                    let p: usize = f[1].parse().unwrap_or(0);
                    while z.english.len() <= p {
                        z.english.push(HashMap::new());
                    }
                    z.english[p].insert(f[2], four(&f[3..]));
                }
                "U" => z.utc_gmt.push([f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9]]),
                "L" => {
                    z.locales.insert(f[1], (f[2] == "E", f[3].parse().unwrap_or(0)));
                }
                _ => {}
            }
        }
        z
    })
}

const UTC_ZONES: [&str; 8] = ["UTC", "Etc/UTC", "Etc/UCT", "UCT", "Etc/Universal", "Universal", "Etc/Zulu", "Zulu"];
const GMT_ZONES: [&str; 8] = ["GMT", "Etc/GMT", "Etc/GMT+0", "Etc/GMT-0", "Etc/GMT0", "GMT0", "Etc/Greenwich", "Greenwich"];

/// `TimeZone.getDisplayName` names of zone `id` in `locale`: long standard, long daylight,
/// short standard, short daylight. Locales outside English carry only their UTC and GMT
/// names; their other zones use the en_US names.
pub fn zone_names(locale: &str, id: &str) -> Option<[&'static str; 4]> {
    let z = zones();
    let key = resolve(locale, |l| z.locales.contains_key(l));
    let &(english, p) = z.locales.get(key)?;
    if english {
        if let Some(n) = z.english.get(p).and_then(|m| m.get(id)) {
            return Some(*n);
        }
    } else if let Some(u) = z.utc_gmt.get(p) {
        if UTC_ZONES.contains(&id) {
            return Some([u[0], u[1], u[2], u[3]]);
        }
        if GMT_ZONES.contains(&id) {
            return Some([u[4], u[5], u[6], u[7]]);
        }
    }
    z.base.get(id).copied()
}

/// The locale key data is stored under: exact, then without script/extensions, language and
/// country, language, root.
fn resolve(locale: &str, has: impl Fn(&str) -> bool) -> &str {
    if has(locale) {
        return locale;
    }
    if let Some(i) = locale.find("_#")
        && has(&locale[..i])
    {
        return &locale[..i];
    }
    let parts: Vec<&str> = locale.splitn(3, '_').collect();
    if parts.len() > 2 {
        let n = parts[0].len() + 1 + parts[1].len();
        if has(&locale[..n]) {
            return &locale[..n];
        }
    }
    if has(parts[0]) {
        return &locale[..parts[0].len()];
    }
    ""
}


// ------------------------------------------------------------------------------ day periods

struct DayPeriods {
    /// Per profile and style (FULL, SHORT, NARROW): 26 names, see [`period_index`].
    profiles: Vec<[Vec<&'static str>; 3]>,
    locales: HashMap<&'static str, usize>,
}

fn day_periods() -> &'static DayPeriods {
    static D: OnceLock<DayPeriods> = OnceLock::new();
    D.get_or_init(|| {
        let mut d = DayPeriods { profiles: Vec::new(), locales: HashMap::new() };
        let expand = |runs: &'static str| -> Vec<&'static str> {
            let mut out = Vec::with_capacity(26);
            for run in runs.split('|') {
                let (name, n) = run.rsplit_once('*').unwrap_or((run, "1"));
                for _ in 0..n.parse::<usize>().unwrap_or(1) {
                    out.push(name);
                }
            }
            out
        };
        for line in include_str!("dayperiods.tsv").lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            match f[0] {
                "P" => d.profiles.push([expand(f[2]), expand(f[3]), expand(f[4])]),
                "L" => {
                    d.locales.insert(f[1], f[2].parse().unwrap_or(0));
                }
                _ => {}
            }
        }
        d
    })
}

/// Slot of a minute of the day in the captured names: midnight, the rest of hour 0, hours
/// 1–11, noon, the rest of hour 12, hours 13–23 (the JDK's day periods change on the hour).
fn period_index(minute_of_day: u32) -> usize {
    let h = (minute_of_day / 60) as usize;
    match minute_of_day {
        0 => 0,
        1..=59 => 1,
        720 => 13,
        _ if h < 12 => h + 1,
        _ if h == 12 => 14,
        _ => h + 2,
    }
}

fn period_style(style: u8) -> usize {
    match style {
        0 | 1 => 0,
        2 | 3 => 1,
        _ => 2,
    }
}

/// The day period name (`B` patterns) of a minute of the day.
pub fn day_period(locale: &str, minute_of_day: u32, style: u8) -> &'static str {
    let d = day_periods();
    let key = resolve(locale, |l| d.locales.contains_key(l));
    let p = d.locales.get(key).copied().unwrap_or(0);
    d.profiles[p][period_style(style)].get(period_index(minute_of_day)).copied().unwrap_or("")
}

/// Every day period name of a style, for parsing.
pub fn day_period_names(locale: &str, style: u8) -> Vec<&'static str> {
    let d = day_periods();
    let key = resolve(locale, |l| d.locales.contains_key(l));
    let p = d.locales.get(key).copied().unwrap_or(0);
    let mut v = d.profiles[p][period_style(style)].clone();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_fallbacks() {
        assert_eq!(month("fr_FR", 3, 0), "mars");
        assert_eq!(month("fr", 3, 2), "mars");
        assert_eq!(month("de_DE", 3, 0), "März");
        assert_eq!(month("", 1, 0), "Jan");
        assert_eq!(month("en_US", 1, 0), "January");
        assert_eq!(month("en_US", 1, 4), "J");
        assert_eq!(day("it", 1, 0), "lunedì");
        assert_eq!(day("en_US", 7, 2), "Sun");
        assert_eq!(ampm("en_US", 1), "PM");
        assert_eq!(era("en_US", 1, 2), "AD");
        assert_eq!(quarter("en_US", 2, 0), "2nd quarter");
        // unknown regions fall back to the language, unknown languages to the root locale
        assert_eq!(month("fr_ZZ", 3, 0), "mars");
        assert_eq!(month("zz", 1, 0), "Jan");
        assert_eq!(week_def("fr_FR"), (1, 4));
        assert_eq!(week_def("en_US"), (7, 1));
        assert_eq!(localized_pattern("en_US", Some(3), None), "M/d/yy");
        assert_eq!(zone_names("en_US", "Europe/Paris"), Some(["Central European Standard Time", "Central European Summer Time", "CET", "CEST"]));
        assert_eq!(zone_names("fr_FR", "UTC").map(|n| n[0]), Some("temps universel coordonné"));
        assert_eq!(zone_names("fr_FR", "Europe/Paris").map(|n| n[2]), Some("CET"));
        assert_eq!(zone_names("en_US", "Etc/GMT+5").map(|n| n[0]), Some("GMT-05:00"));
        assert_eq!(day_period("en_US", 0, 0), "midnight");
        assert_eq!(day_period("en_US", 720, 0), "noon");
        assert_eq!(day_period("en_US", 721, 0), "in the afternoon");
        assert_eq!(day_period("en_US", 23 * 60, 0), "at night");
        assert_eq!(day_period("zh_TW", 15 * 60 + 4, 2), "下午");
    }
}
