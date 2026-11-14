#!/usr/bin/env python3
"""Captures the java.time and java.util.Locale data of Elasticsearch 9.5.5's JDK (its CLDR
provider) by running Painless scripts through `_scripts/painless/_execute`, and writes the data
files the crate embeds: src/time/locales.tsv (month/day/quarter/am-pm/era names, localized
patterns, week definitions), src/time/dayperiods.tsv ('B' day periods), src/time/zones.tsv
(TimeZone display names) and src/locale_names.tsv (Locale display names and ISO codes).

usage: capture_dates.py <crate dir> [es_url]
"""
import json, os, sys, urllib.request

ES = (sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:19200") + "/_scripts/painless/_execute"

def run(src, params=None):
    body = {"script": {"source": src, "params": params or {}}}
    req = urllib.request.Request(ES, data=json.dumps(body).encode(), headers={"content-type": "application/json"})
    with urllib.request.urlopen(req) as r:
        return json.load(r)["result"]

DATES = "String S = String.valueOf((char) 1); String N = String.valueOf((char) 10);\nStringBuilder out = new StringBuilder();\nLocale[] all = Locale.getAvailableLocales();\nString[] mp = new String[] {'MMMM', 'MMM', 'MMMMM', 'LLLL', 'LLL', 'LLLLL'};\nString[] dp = new String[] {'EEEE', 'EEE', 'EEEEE', 'cccc', 'ccc', 'ccccc'};\nString[] qp = new String[] {'QQQQ', 'QQQ', 'QQQQQ', 'qqqq', 'qqq', 'qqqqq'};\nString[] gp = new String[] {'GGGG', 'G', 'GGGGG'};\nFormatStyle[] fs = new FormatStyle[] {FormatStyle.FULL, FormatStyle.LONG, FormatStyle.MEDIUM, FormatStyle.SHORT};\nfor (int li = params.from; li < params.to && li < all.length; li++) {\n  Locale l = all[li];\n  out.append(l.toString());\n  for (String p : mp) { DateTimeFormatter f = DateTimeFormatter.ofPattern(p, l); for (int m = 1; m <= 12; m++) { out.append(S).append(f.format(LocalDate.of(2020, m, 1))); } }\n  for (String p : dp) { DateTimeFormatter f = DateTimeFormatter.ofPattern(p, l); for (int d = 0; d < 7; d++) { out.append(S).append(f.format(LocalDate.of(2020, 3, 2 + d))); } }\n  for (String p : qp) { DateTimeFormatter f = DateTimeFormatter.ofPattern(p, l); for (int q = 0; q < 4; q++) { out.append(S).append(f.format(LocalDate.of(2020, 1 + 3 * q, 1))); } }\n  DateTimeFormatter a = DateTimeFormatter.ofPattern('a', l);\n  out.append(S).append(a.format(LocalTime.of(1, 0))).append(S).append(a.format(LocalTime.of(13, 0)));\n  for (String p : gp) { DateTimeFormatter f = DateTimeFormatter.ofPattern(p, l); out.append(S).append(f.format(LocalDate.of(-5, 1, 1))).append(S).append(f.format(LocalDate.of(2020, 1, 1))); }\n  for (FormatStyle d : fs) { out.append(S).append(DateTimeFormatterBuilder.getLocalizedDateTimePattern(d, null, IsoChronology.INSTANCE, l)); }\n  for (FormatStyle t : fs) { out.append(S).append(DateTimeFormatterBuilder.getLocalizedDateTimePattern(null, t, IsoChronology.INSTANCE, l)); }\n  for (FormatStyle d : fs) { for (FormatStyle t : fs) { out.append(S).append(DateTimeFormatterBuilder.getLocalizedDateTimePattern(d, t, IsoChronology.INSTANCE, l)); } }\n  WeekFields w = WeekFields.of(l);\n  out.append(S).append(w.getFirstDayOfWeek().getValue()).append(S).append(w.getMinimalDaysInFirstWeek());\n  out.append(N);\n}\nreturn out.toString();\n"
DAY_PERIODS = "String S = String.valueOf((char) 1); String N = String.valueOf((char) 10);\nStringBuilder out = new StringBuilder();\nList times = [LocalTime.of(0, 0), LocalTime.of(0, 1)];\nfor (int h = 1; h < 24; h++) { times.add(LocalTime.of(h, 0)); if (h == 12) times.add(LocalTime.of(12, 1)); }\nfor (Locale l : Locale.getAvailableLocales()) {\n  out.append(l.toString());\n  for (String p : ['B', 'BBBB', 'BBBBB']) { DateTimeFormatter f = DateTimeFormatter.ofPattern(p, l); for (LocalTime t : times) { out.append(S).append(f.format(t)); } }\n  out.append(N);\n}\nreturn out.toString();\n"
ZONES = 'String S = String.valueOf((char) 1); String N = String.valueOf((char) 10);\nStringBuilder out = new StringBuilder(); out.append(Locale.getAvailableLocales()[params.i].toString()).append(String.valueOf((char) 10));\nList ids = new ArrayList(ZoneId.getAvailableZoneIds()); Collections.sort(ids);\nLocale l = Locale.getAvailableLocales()[params.i];\nfor (String id : ids) {\n  TimeZone tz = TimeZone.getTimeZone(id);\n  out.append(id).append(S).append(tz.getDisplayName(false, TimeZone.LONG, l)).append(S).append(tz.getDisplayName(true, TimeZone.LONG, l)).append(S).append(tz.getDisplayName(false, TimeZone.SHORT, l)).append(S).append(tz.getDisplayName(true, TimeZone.SHORT, l)).append(N);\n}\nreturn out.toString();\n'
LOCALE_META = "String S = String.valueOf((char) 1); String N = String.valueOf((char) 10);\nStringBuilder out = new StringBuilder();\nfor (String c : Locale.getISOLanguages()) { Locale l = new Locale(c); String i3; try { i3 = l.getISO3Language() } catch (Exception e) { i3 = '!' } out.append('L').append(S).append(c).append(S).append(i3).append(S).append(l.getDisplayLanguage(Locale.US)).append(N); }\nfor (String c : Locale.getISOCountries()) { Locale l = new Locale('', c); String i3; try { i3 = l.getISO3Country() } catch (Exception e) { i3 = '!' } out.append('C').append(S).append(c).append(S).append(i3).append(S).append(l.getDisplayCountry(Locale.US)).append(N); }\nfor (Locale l : Locale.getAvailableLocales()) { out.append('A').append(S).append(l.toString()).append(S).append(l.toLanguageTag()).append(S).append(l.getDisplayName(Locale.US)).append(S).append(l.getScript()).append(S).append(l.getDisplayScript(Locale.US)).append(S).append(l.getDisplayLanguage(Locale.US)).append(S).append(l.getDisplayCountry(Locale.US)).append(S).append(l.getDisplayVariant(Locale.US)).append(N); }\nreturn out.toString();\n"

def dates_tsv(text, path):
    # Converts the date-symbol capture (one line per locale, \x01-separated) into src/time/locales.tsv:
    # deduplicated sets per kind, then one L line per locale referencing them.
    lines = text.split('\n')[:-1]
    rows = [l.split('\x01') for l in lines]
    segs = [('M', 1, 73), ('D', 73, 115), ('Q', 115, 139), ('A', 139, 141), ('E', 141, 147), ('P', 147, 171)]
    tables = {k: {} for k, _, _ in segs}
    out = ['# java.time locale data captured from Elasticsearch 9.5.5\'s JDK (CLDR provider) by tests/fixtures/capture_dates.py:',
           '# M months (MMMM MMM MMMMM LLLL LLL LLLLL x 12), D days Mon..Sun (EEEE EEE EEEEE cccc ccc ccccc), Q quarters',
           '# (QQQQ QQQ QQQQQ qqqq qqq qqqqq x 4), A am/pm, E eras BC/AD (GGGG G GGGGG), P localized patterns (date FULL..SHORT,',
           '# time FULL..SHORT, date x time), L locale -> set ids, first day of week (1 = Monday), minimal days in first week']
    body = []
    for r in rows:
        for f in r:
            assert '\t' not in f and '\n' not in f, f
        ids = []
        for k, a, b in segs:
            key = tuple(r[a:b])
            t = tables[k]
            if key not in t:
                t[key] = len(t)
                out.append('\t'.join([k, str(t[key])] + list(key)))
            ids.append(str(t[key]))
        body.append('\t'.join(['L', r[0]] + ids + r[171:173]))
    open(path, 'w', encoding='utf-8').write('\n'.join(out + body) + '\n')


def zones_tsv(text, path):
    # Builds src/time/zones.tsv from the per-locale TimeZone display-name capture.
    data=text.split('\x02\n')[:-1]
    blocks=[]
    for block in data:
        lines=block.split('\n')
        rows=[l.split('\x01') for l in lines[1:] if l]
        blocks.append((lines[0],rows))
    ids=[r[0] for r in blocks[0][1]]
    UTC=['UTC','Etc/UTC','Etc/UCT','UCT','Etc/Universal','Universal','Etc/Zulu','Zulu']
    GMT=['GMT','Etc/GMT','Etc/GMT+0','Etc/GMT-0','Etc/GMT0','GMT0','Etc/Greenwich','Greenwich']
    idx={z:i for i,z in enumerate(ids)}
    en_us=[b for b in blocks if b[0]=='en_US'][0][1]
    base={r[0]:tuple(r[1:]) for r in en_us}
    out=["# TimeZone display names captured from Elasticsearch 9.5.5's JDK (tests/fixtures/capture_zones.py): N = en_US",
         "# names (long standard, long daylight, short standard, short daylight); D = another English profile's",
         "# differences from en_US; U = a profile's UTC then GMT names (same four); L = locale -> E|U profile"]
    for z in ids: out.append('\t'.join(['N',z]+list(base[z])))
    eprof={}; uprof={}
    for loc,rows in blocks:
        lang=loc.split('_')[0]
        if lang in ('en',''):
            key=tuple(tuple(r[1:]) for r in rows)
            if key not in eprof:
                eprof[key]=len(eprof)
                for r in rows:
                    if tuple(r[1:])!=base[r[0]]:
                        out.append('\t'.join(['D',str(eprof[key]),r[0]]+r[1:]))
            out.append('\t'.join(['L',loc,'E',str(eprof[key])]))
        else:
            u={tuple(rows[idx[z]][1:]) for z in UTC}; g={tuple(rows[idx[z]][1:]) for z in GMT}
            assert len(u)==1 and len(g)==1, loc
            u=u.pop(); g=g.pop()
            key=u+g
            if key not in uprof:
                uprof[key]=len(uprof)
                out.append('\t'.join(['U',str(uprof[key])]+list(key)))
            out.append('\t'.join(['L',loc,'U',str(uprof[key])]))
    open(path,'w',encoding='utf-8').write('\n'.join(out)+'\n')


def day_periods_tsv(text, path):
    rows = [l.split('\x01') for l in text.split('\n') if l]
    prof = {}
    out = ["# Day period names ('B', 'BBBB', 'BBBBB') captured from Elasticsearch 9.5.5's JDK (tests/fixtures/capture_dates.py):",
           "# P id, then per style 26 names at 00:00, 00:01, 01:00 ... 11:00, 12:00, 12:01, 13:00 ... 23:00; L locale -> P id"]
    lines = []
    for r in rows:
        key = tuple(r[1:])
        if key not in prof:
            prof[key] = len(prof)
            segs = []
            for s in range(3):
                names = key[s * 26:(s + 1) * 26]
                runs, i = [], 0
                while i < 26:
                    j = i
                    while j < 26 and names[j] == names[i]:
                        j += 1
                    runs.append("%s*%d" % (names[i], j - i))
                    i = j
                segs.append('|'.join(runs))
            out.append('\t'.join(['P', str(prof[key])] + segs))
        lines.append('\t'.join(['L', r[0], str(prof[key])]))
    open(path, 'w', encoding='utf-8').write('\n'.join(out + lines) + '\n')

def locale_names_tsv(text, path):
    rows = [l.split('\x01') for l in text.split('\n') if l]
    langs, countries, scripts, variants = {}, {}, {}, {}
    for r in rows:
        if r[0] == 'L': langs[r[1]] = (r[2], r[3])
        elif r[0] == 'C': countries[r[1]] = (r[2], r[3])
    for r in rows:
        if r[0] != 'A': continue
        ts, tag, dn, sc, scn, ln, cn, vn = r[1:9]
        parts = ts.split('_')
        if parts[0] and parts[0] not in langs: langs[parts[0]] = ('', ln)
        if sc: scripts[sc] = scn
        if len(parts) > 2 and parts[2] and not parts[2].startswith('#'): variants[parts[2]] = vn
        if len(parts) > 1 and parts[1] and parts[1] not in countries: countries[parts[1]] = ('', cn)
    iso_l = [r[1] for r in rows if r[0] == 'L']; iso_c = [r[1] for r in rows if r[0] == 'C']
    out = ["# java.util.Locale data captured from Elasticsearch 9.5.5's JDK by tests/fixtures/capture_dates.py:",
           "# L language ISO3 English-name (I = in Locale.getISOLanguages()), C country ISO3 English-name, S script name, V variant name"]
    for k, (i3, n) in langs.items(): out.append('\t'.join(['L', k, i3, n, 'I' if k in iso_l else '']))
    for k, (i3, n) in countries.items(): out.append('\t'.join(['C', k, i3, n, 'I' if k in iso_c else '']))
    for k, n in scripts.items(): out.append('\t'.join(['S', k, n]))
    for k, n in variants.items(): out.append('\t'.join(['V', k, n]))
    open(path, 'w', encoding='utf-8').write('\n'.join(out) + '\n')

def main():
    crate = sys.argv[1]
    dates = "".join(run(DATES, {"from": i, "to": i + 200}) for i in range(0, 1400, 200))
    dates_tsv(dates, os.path.join(crate, "src/time/locales.tsv"))
    day_periods_tsv(run(DAY_PERIODS), os.path.join(crate, "src/time/dayperiods.tsv"))
    zones = "".join(run(ZONES, {"i": i}) + "\x02\n" for i in range(len(dates.split("\n")) - 1))
    zones_tsv(zones, os.path.join(crate, "src/time/zones.tsv"))
    locale_names_tsv(run(LOCALE_META), os.path.join(crate, "src/locale_names.tsv"))

main()
