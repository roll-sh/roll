use super::*;

#[test]
fn epoch_day_round_trips() {
    for ed in [-1_000_000i64, -719_528, -1, 0, 1, 18_262, 2_932_896, 365_241_780_471] {
        let d = Date::from_epoch_day(ed);
        assert_eq!(d.epoch_day(), ed, "{d:?}");
    }
    assert_eq!(Date { y: 2020, m: 1, d: 1 }.epoch_day(), 18262);
    assert_eq!(Date { y: 2020, m: 1, d: 1 }.day_of_week(), 3);
}

#[test]
fn to_string_layouts() {
    let d = Date { y: 2020, m: 1, d: 1 };
    assert_eq!(TimeVal::Zoned { d, t: Time(0), offset: 0, zone: Zone::Offset(0) }.to_java_string(), "2020-01-01T00:00Z");
    assert_eq!(TimeVal::Instant { secs: 0, nanos: 0 }.to_java_string(), "1970-01-01T00:00:00Z");
    assert_eq!(TimeVal::Instant { secs: 0, nanos: 500_000_000 }.to_java_string(), "1970-01-01T00:00:00.500Z");
    assert_eq!(TimeVal::Duration { secs: 3, nanos: 0 }.to_java_string(), "PT3S");
    assert_eq!(TimeVal::Duration { secs: 29_172, nanos: 345_000_000 }.to_java_string(), "PT8H6M12.345S");
    assert_eq!(TimeVal::Duration { secs: -1, nanos: 500_000_000 }.to_java_string(), "PT-0.5S");
    assert_eq!(TimeVal::Period { y: 1, m: 2, d: 3 }.to_java_string(), "P1Y2M3D");
    assert_eq!(TimeVal::LocalTime(Time::of(10, 15, 30, 0).ok().unwrap()).to_java_string(), "10:15:30");
    assert_eq!(TimeVal::LocalTime(Time::of(10, 15, 0, 1000).ok().unwrap()).to_java_string(), "10:15:00.000001");
    assert_eq!(format_date(Date { y: 10000, m: 1, d: 1 }), "+10000-01-01");
    assert_eq!(format_date(Date { y: -1, m: 1, d: 1 }), "-0001-01-01");
}

#[test]
fn month_arithmetic_clamps() {
    let d = Date { y: 2020, m: 1, d: 31 };
    assert_eq!(d.plus_months(1).ok().unwrap(), Date { y: 2020, m: 2, d: 29 });
    assert_eq!(d.plus_months(13).ok().unwrap(), Date { y: 2021, m: 2, d: 28 });
    assert_eq!(Date { y: 2020, m: 2, d: 29 }.plus_years(1).ok().unwrap(), Date { y: 2021, m: 2, d: 28 });
}

#[test]
fn zones_resolve_gaps_and_overlaps() {
    let z = zone_of("Europe/Paris").ok().unwrap();
    // 2021-03-28 02:30 does not exist in Paris: shifted to 03:30 +02:00
    let (d, t, o) = resolve_local(Date { y: 2021, m: 3, d: 28 }, Time::of(2, 30, 0, 0).ok().unwrap(), &z, None);
    assert_eq!((d, t.hour(), t.minute(), o), (Date { y: 2021, m: 3, d: 28 }, 3, 30, 7200));
    // 2021-10-31 02:30 happens twice: the earlier offset (+02:00) wins
    let (_, _, o) = resolve_local(Date { y: 2021, m: 10, d: 31 }, Time::of(2, 30, 0, 0).ok().unwrap(), &z, None);
    assert_eq!(o, 7200);
    assert_eq!(zone_of("UTC").ok().unwrap().id(), "UTC");
    assert_eq!(zone_of("Z").ok().unwrap().id(), "Z");
    assert_eq!(zone_of("+05:30").ok().unwrap().id(), "+05:30");
    assert!(zone_of("Mars/Base").is_err());
}

#[test]
fn iso_parsing() {
    assert_eq!(parse_local_date("2020-02-29").ok().unwrap(), Date { y: 2020, m: 2, d: 29 });
    assert!(parse_local_date("2021-02-29").is_err());
    assert_eq!(parse_duration("PT1H30M").ok().unwrap(), (5400, 0));
    assert_eq!(parse_duration("-PT0.5S").ok().unwrap(), (-1, 500_000_000));
    assert_eq!(parse_period("P1Y2M3W4D").ok().unwrap(), (1, 2, 25));
    assert_eq!(parse_instant("2020-01-01T00:00:00Z").ok().unwrap(), (1_577_836_800, 0));
}

#[test]
fn formatter_patterns() {
    let f = format::Formatter::of_pattern("yyyy-MM-dd HH:mm:ss.SSS EEE MMM a", "en_US").ok().unwrap();
    let t = TimeVal::LocalDateTime(Date { y: 2020, m: 1, d: 2 }, Time::of(15, 4, 5, 678_000_000).ok().unwrap());
    assert_eq!(f.format(&t).ok().unwrap(), "2020-01-02 15:04:05.678 Thu Jan PM");
    let p = format::Formatter::of_pattern("dd/MM/yyyy", "en_US").ok().unwrap();
    let r = p.parse_to("31/12/1999", cls::LOCAL_DATE).ok().unwrap();
    assert_eq!(r.to_java_string(), "1999-12-31");
}
