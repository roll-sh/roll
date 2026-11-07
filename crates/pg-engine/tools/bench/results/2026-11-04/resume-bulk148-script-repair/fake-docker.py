#!/usr/bin/env python3
import os, sys
a=sys.argv[1:]
if a[0]=='inspect':
    if os.environ.get('FAIL_DOCKER')=='1': sys.exit(29)
    print('fake')
elif a[0]=='exec':
    if 'psql' in a:
        sql=a[-1]
        if os.environ.get('FAIL_SQL') and os.environ['FAIL_SQL'] in sql:
            print('ERROR: deliberate fake SQL failure', file=sys.stderr); sys.exit(23)
        print('FAKE_SQL_OK')
    else: print('0')
else: sys.exit(91)
