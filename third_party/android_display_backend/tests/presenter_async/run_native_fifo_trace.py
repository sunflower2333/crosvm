#!/usr/bin/env python3
"""Exercise production trace state and presenter/executor, replacing only OS/API boundaries."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
p=argparse.ArgumentParser(); p.add_argument('--out',required=True,type=Path); a=p.parse_args()
a.out.mkdir(parents=True,exist_ok=False)
here=Path(__file__).resolve().parent; root=here.parents[1]
headers=['surface_buffer_presenter.h','native_fifo_trace.h','consumer_release.h','display_timing.h','scanout_presentation_geometry.h','scanout_geometry_abi.h']
result={'boundary':'Actual production functions with controlled Android and trace IO boundaries; no runtime/hardware proof','production_sha256':{n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in headers},'cases':[]}
with tempfile.TemporaryDirectory(prefix='.nfifo-',dir=here) as temp:
    temp=Path(temp)
    for name in headers: shutil.copy2(root/name,temp/name)
    shutil.copy2(here/'surface_control_dl.h',temp/'surface_control_dl.h')
    def build(name):
        binary=temp/name
        r=subprocess.run(['c++','-std=c++17','-O1','-g','-Wall','-Wextra','-Werror','-pthread','-fsanitize=address,undefined','-fno-omit-frame-pointer','-I'+str(temp),'-I'+str(here/'stub'),str(here/'native_fifo_trace_test.cpp'),'-o',str(binary)],capture_output=True,text=True)
        (a.out/(name+'.compile.stdout')).write_text(r.stdout); (a.out/(name+'.compile.stderr')).write_text(r.stderr)
        assert r.returncode==0,r.stderr
        return binary
    def run(binary,case,red=False):
        name=binary.name+'-'+case
        r=subprocess.run([str(binary),case],capture_output=True,text=True,timeout=15,env={**os.environ,'ASAN_OPTIONS':'detect_leaks=0'})
        (a.out/(name+'.stdout')).write_text(r.stdout); (a.out/(name+'.stderr')).write_text(r.stderr)
        passed=(r.returncode!=0 and 'Assertion' in r.stderr) if red else (r.returncode==0 and 'PASS ' in r.stdout)
        result['cases'].append(dict(name=name,exit_code=r.returncode,expected_rejection=red,passed=passed))
    binary=build('trace')
    for case in ['chain','disabled','limits']: run(binary,case)
    mutations=[('missing-apply','surface_buffer_presenter.h','nativeFifoTraceEvent("apply_begin", trace, reinterpret_cast<uintptr_t>(transaction));','(void)trace;','chain'),('disabled-clock','native_fifo_trace.h','if (!enabled()) return {};','if (!enabled()) { (void)mIo.now(); return {}; }','disabled'),('event-cap','native_fifo_trace.h','if (mEvents >= mEventLimit) { stopLocked(now, 2); return; }','if (false) { stopLocked(now, 2); return; }','limits')]
    for name,file,old,new,case in mutations:
        original=(root/file).read_text(); assert original.count(old)==1,(file,old)
        (temp/file).write_text(original.replace(old,new))
        run(build(name),case,True)
        (temp/file).write_text(original)
result['passed']=all(c['passed'] for c in result['cases'])
(a.out/'result.json').write_text(json.dumps(result,indent=2)+'\n'); print(json.dumps(result))
raise SystemExit(0 if result['passed'] else 1)
