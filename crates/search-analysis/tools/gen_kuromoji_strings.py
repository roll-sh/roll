#!/usr/bin/env python3
"""Generates src/kuromoji/strings.rs from Lucene's ToStringUtil.java (Apache-2.0, Lucene 10.5.1):
the English translations of IPADIC's part-of-speech, inflection type and inflected form tags,
and `getRomanization` (modified Hepburn), transpiled from its switch statements.

usage: python3 -I tools/gen_kuromoji_strings.py <ToStringUtil.java> > src/kuromoji/strings.rs
"""
import re, sys

src = open(sys.argv[1], encoding='utf-8').read()


def table(field):
    start = src.index('private static final Map<String, String> %s;' % field)
    end = src.index('%s = Collections.unmodifiableMap' % field, start)
    pairs = re.findall(r'translations\.put\("((?:[^"\\]|\\.)*)", "((?:[^"\\]|\\.)*)"\);', src[start:end])
    d = {}
    for k, v in pairs:
        d[k] = v  # HashMap.put: the last one wins
    return sorted(d.items(), key=lambda kv: kv[0].encode('utf-8'))


def lit(c):
    return '0x%04X' % ord(c)


body = src[src.index('public static void getRomanization(Appendable builder'):]
lines = body.split('\n')
start = next(i for i, l in enumerate(lines) if l.strip() == 'switch (ch) {')
out = []
stack = []  # per switch: {'indent': n, 'arm_open': bool, 'labels': [], 'stmts': 0, 'last': str, 'default': bool}
i = start


def emit(indent, s):
    out.append('    ' * indent + s)


depth = 3
def close_arm(sw, at_end=False):
    global depth
    if sw['arm_open']:
        if sw['stmts'] and not at_end and sw['last'] not in ('break', 'break main'):
            raise SystemExit('fallthrough after %r' % sw['labels'])
        sw['all_main'] = sw.get('all_main', True) and sw['last'] == 'break main'
        depth -= 1
        emit(depth, '}')
        sw['arm_open'] = False


for line in lines[start:]:
    t = line.strip()
    if not t or t.startswith('//'):
        continue
    m = re.match(r"switch \((ch\d?)\) \{$", t)
    if m:
        emit(depth, 'match %s {' % m.group(1))
        depth += 1
        stack.append({'arm_open': False, 'labels': [], 'stmts': 0, 'last': '', 'default': False})
        continue
    m = re.match(r"case '(.)':$", t)
    if m or t == 'default:':
        sw = stack[-1]
        label = lit(m.group(1)) if m else '_'
        if m is None:
            sw['default'] = True
        if sw['arm_open'] and sw['stmts'] == 0:
            # label grouping: rewrite the open arm's head
            sw['labels'].append(label)
            out[-1] = '    ' * (depth - 1) + ' | '.join(sw['labels']) + ' => {'
            continue
        close_arm(sw)
        sw['labels'] = [label]
        sw['stmts'] = 0
        sw['last'] = ''
        emit(depth, label + ' => {')
        depth += 1
        sw['arm_open'] = True
        continue
    if t == '}' and stack and depth > 0:
        # closes an if/else block or a switch
        sw = stack[-1]
        if sw.get('if_depth', 0) > 0:
            sw['if_depth'] -= 1
            depth -= 1
            emit(depth, '}')
            continue
        close_arm(sw, at_end=True)
        if not sw['default']:
            emit(depth, '_ => {}')
        depth -= 1
        emit(depth, '}')
        stack.pop()
        if not stack:
            break
        stack[-1]['stmts'] += 1
        # a switch whose every arm leaves the loop body cannot fall through
        stack[-1]['last'] = 'break main' if sw['default'] and sw.get('all_main') else 'switch'
        continue
    sw = stack[-1]
    m = re.match(r"(\} else )?if \((.*)\) \{$", t)
    if m:
        cond = re.sub(r"'(.)'", lambda mm: lit(mm.group(1)), m.group(2))
        if m.group(1):
            depth -= 1
            emit(depth, '} else if %s {' % cond)
        else:
            emit(depth, 'if %s {' % cond)
            sw['if_depth'] = sw.get('if_depth', 0) + 1
            sw['stmts'] += 1
            sw['last'] = 'if'
        depth += 1
        continue
    if t == '} else {':
        depth -= 1
        emit(depth, '} else {')
        depth += 1
        continue
    if t == 'break;':
        if sw.get('if_depth', 0):
            raise SystemExit('break inside an if')
        if sw['stmts'] == 0:
            # `case 'ー': break;`: an arm that does nothing
            sw['stmts'] = 1
        sw['last'] = 'break'
        continue
    sw['stmts'] += 1
    if t == 'break main;':
        sw['last'] = 'break main'
        emit(depth, "break 'main;")
        continue
    sw['last'] = 'stmt'
    m = re.match(r'builder\.append\("(.*)"\);$', t)
    if m:
        emit(depth, 'push(out, "%s");' % m.group(1))
        continue
    m = re.match(r"builder\.append\('(.)'\);$", t)
    if m:
        emit(depth, 'out.push(%s);' % lit(m.group(1)))
        continue
    if t == 'builder.append(ch);':
        emit(depth, 'out.push(ch);')
        continue
    if t == 'i++;':
        emit(depth, 'i += 1;')
        continue
    if t == 'i += 2;':
        emit(depth, 'i += 2;')
        continue
    raise SystemExit('unhandled: %r' % t)


def rs_table(name, items):
    s = 'pub(crate) static %s: &[(&str, &str)] = &[\n' % name
    for k, v in items:
        s += '    ("%s", "%s"),\n' % (k, v)
    return s + '];\n'


print('''//! Generated by tools/gen_kuromoji_strings.py from Lucene's `ToStringUtil` (Apache-2.0, Lucene
//! 10.5.1): English names of IPADIC's tags (the `(en)` attributes of `_analyze` explain) and the
//! modified Hepburn romanization of `kuromoji_readingform`'s `use_romaji`. Do not edit.

''')
print(rs_table('POS_TRANSLATIONS', table('posTranslations')))
print(rs_table('INFL_TYPE_TRANSLATIONS', table('inflTypeTranslations')))
print(rs_table('INFL_FORM_TRANSLATIONS', table('inflFormTranslations')))
print('''/// Looks a tag up in one of the tables.
pub(crate) fn translate(table: &[(&str, &'static str)], s: &str) -> Option<&'static str> {
    table
        .binary_search_by(|(k, _)| k.as_bytes().cmp(s.as_bytes()))
        .ok()
        .map(|i| table[i].1)
}

fn push(out: &mut Vec<u16>, s: &str) {
    out.extend(s.encode_utf16());
}

/// `ToStringUtil.getRomanization`: katakana to modified Hepburn, over UTF-16 units.
pub(crate) fn romanize(out: &mut Vec<u16>, s: &[u16]) {
    let len = s.len();
    let mut i = 0;
    while i < len {
        // maximum lookahead: 3
        let ch = s[i];
        let ch2 = if i + 1 < len { s[i + 1] } else { 0 };
        let ch3 = if i + 2 < len { s[i + 2] } else { 0 };
        #[allow(clippy::if_same_then_else)]
        'main: {''')
print('\n'.join(out))
print('''        }
        i += 1;
    }
}''')
