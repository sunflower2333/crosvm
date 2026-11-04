#!/usr/bin/env python3
"""Real-header RED/GREEN regression; retain predecessor and successor provenance."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import subprocess
import tempfile
p=argparse.ArgumentParser();p.add_argument('--out',required=True,type=Path);p.add_argument('--baseline',required=True,type=Path);a=p.parse_args()
here=Path(__file__).resolve().parent;root=here.parents[1];a.out.mkdir(parents=True,exist_ok=False)
sha=lambda f:hashlib.sha256(f.read_bytes()).hexdigest()
result={'boundary':'Local production-header tests with controlled trace IO; no device/runtime proof','fixture_sha256':sha(here/'clock_zero_test.cpp'),'headers':{},'cases':[]}
with tempfile.TemporaryDirectory(prefix='.clock-zero-',dir=a.out) as temp:
    for name,headers in [('predecessor',a.baseline.resolve()),('successor',root)]:
        result['headers'][name]={'path':str(headers/'native_fifo_trace.h'),'sha256':sha(headers/'native_fifo_trace.h')}
        binary=Path(temp)/name
        command=['c++','-std=c++17','-O1','-g','-Wall','-Wextra','-Werror','-pthread','-fsanitize=address,undefined','-fno-omit-frame-pointer','-I'+str(headers),str(here/'clock_zero_test.cpp'),'-o',str(binary)]
        r=subprocess.run(command,capture_output=True,text=True)
        (a.out/(name+'.compile.stderr')).write_text(r.stderr)
        assert r.returncode==0,r.stderr
        for case in ['armed-begin','unarmed-begin','armed-event']:
            key=name+'-'+case;red=name=='predecessor' and case=='armed-begin'
            r=subprocess.run([str(binary),case],capture_output=True,text=True,timeout=10,env={**os.environ,'ASAN_OPTIONS':'detect_leaks=0'})
            (a.out/(key+'.stdout')).write_text(r.stdout);(a.out/(key+'.stderr')).write_text(r.stderr)
            passed=(r.returncode!=0 and 'Assertion' in r.stderr and '!state.enabled() && lines.size() == 3' in r.stderr) if red else (r.returncode==0 and 'PASS '+case in r.stdout)
            result['cases'].append(dict(name=key,expected_red=red,exit_code=r.returncode,passed=passed))
result['passed']=all(c['passed'] for c in result['cases']);(a.out/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
raise SystemExit(0 if result['passed'] else 1)
