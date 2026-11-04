import pathlib as P,json,re
r=P.Path(__file__).resolve().parent
def rows(n):return [json.loads(x) for x in (r/n).read_text().splitlines()] if (r/n).exists() else []
results=rows('results.jsonl');commands=rows('commands.jsonl');windows=rows('windows.jsonl')
print(json.dumps(dict(timed=len([x for x in results if re.fullmatch('(copy|pgbench|index)-[0-4]-(baseline|candidate)',x['tag'])]),profiles=len([x for x in results if x.get('profile')]),correctness_checks=len(rows('correctness.jsonl')),footprint_windows=len([x for x in windows if x['tag'].startswith('footprint-')]),close_fixtures=len(rows('close-fixtures.jsonl')),latest_command={k:commands[-1].get(k) for k in ['tag','exit','wall']} if commands else None,final_exit=json.loads((r/'actual-resume1-exit.json').read_text()) if (r/'actual-resume1-exit.json').exists() else None,failures=[dict(tag=x['tag'],exit=x['exit']) for x in commands if x['exit']!=0 and not x['tag'].startswith('negative-')]),indent=2))
