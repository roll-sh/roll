#!/usr/bin/env python3
"""Builds `estests.json`: Painless scripts exercised by Elasticsearch's own language tests, with
the answers Elasticsearch 9.5.5 gives through `POST /_scripts/painless/_execute`.

usage: capture_estests.py <dir of lang-painless test sources> <out.json> [es_url]

Only the script strings are taken from the tests (`exec("...")` calls with a single literal
argument, concatenations of literals allowed); what they evaluate to comes from the running
Elasticsearch, never from the tests' assertions. Each case runs once on a fresh source (a script
Elasticsearch has cached keeps its linked call sites, which changes some `def` error messages),
so re-capture against a node that has not run these sources, or append a space to each source.

Scripts that would take the node down (`new int[Integer.MAX_VALUE - 1]`) and non-deterministic
ones (`UUID.randomUUID()`) are skipped.
"""
import json, os, re, sys, time, urllib.request


def java_str(s, i):
    """Decodes the Java string literal (or text block) starting at `s[i]`."""
    if s.startswith('"""', i):
        j = s.index('"""', i + 3)
        lines = s[i + 3:j].split('\n')[1:]
        ind = min((len(l) - len(l.lstrip()) for l in lines if l.strip()), default=0)
        return '\n'.join(l[ind:] for l in lines), j + 3
    out, i = [], i + 1
    esc = {'n': '\n', 't': '\t', 'r': '\r', '"': '"', "'": "'", '\\': '\\', 'b': '\b', 'f': '\f', '0': '\0'}
    while s[i] != '"':
        if s[i] == '\\':
            if s[i + 1] == 'u':
                out.append(chr(int(s[i + 2:i + 6], 16)))
                i += 6
            else:
                out.append(esc.get(s[i + 1], s[i + 1]))
                i += 2
        else:
            out.append(s[i])
            i += 1
    return ''.join(out), i + 1


def scripts(text):
    for m in re.finditer(r'\bexec\(\s*', text):
        i = m.end()
        if i >= len(text) or text[i] != '"':
            continue
        parts = []
        try:
            while True:
                v, i = java_str(text, i)
                parts.append(v)
                while text[i] in ' \t\r\n':
                    i += 1
                if text[i] != '+':
                    break
                i += 1
                while text[i] in ' \t\r\n':
                    i += 1
                if text[i] != '"':
                    parts = None
                    break
        except (ValueError, IndexError):
            continue
        if parts is not None and text[i] == ')':
            yield ''.join(parts)


def main():
    src_dir, out = sys.argv[1], sys.argv[2]
    url = (sys.argv[3] if len(sys.argv) > 3 else 'http://127.0.0.1:19200') + '/_scripts/painless/_execute'
    seen, cases = set(), []
    for f in sorted(os.listdir(src_dir)):
        for s in scripts(open(os.path.join(src_dir, f), encoding='utf-8').read()):
            if s in seen or 'Integer.MAX_VALUE - 1]' in s or 'randomUUID' in s:
                continue
            seen.add(s)
            cases.append((f.removesuffix('.java'), s))
    rows = []
    for name, src in cases:
        body = {'script': {'source': src, 'lang': 'painless'}}
        while True:
            req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={'content-type': 'application/json'})
            try:
                with urllib.request.urlopen(req) as r:
                    status, resp = r.status, json.load(r)
            except urllib.error.HTTPError as e:
                status, resp = e.code, json.load(e)
            if 'Too many dynamic script compilations' in json.dumps(resp):
                time.sleep(10)
                continue
            break
        rows.append({'name': name, 'request': body, 'status': status, 'response': resp})
    with open(out, 'w', encoding='utf-8') as f:
        f.write('[\n' + ',\n'.join(json.dumps(r, ensure_ascii=False, separators=(',', ':')) for r in rows) + '\n]\n')


main()
