#!/usr/bin/env python3
"""Compile the actual presenter; replace only Android API boundary calls."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile

p = argparse.ArgumentParser()
p.add_argument('--out', required=True, type=Path)
p.add_argument('--expect-red', action='store_true')
a = p.parse_args()
a.out.mkdir(parents=True, exist_ok=False)
here = Path(__file__).resolve().parent
root = here.parents[1]
inputs = ['surface_buffer_presenter.h', 'native_fifo_trace.h', 'consumer_release.h', 'display_timing.h',
          'scanout_presentation_geometry.h', 'scanout_geometry_abi.h']
result = {'production_sha256': {n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in inputs},
          'cases': [], 'boundary': 'Actual presenter with controlled Android API boundary; no hardware proof'}
with tempfile.TemporaryDirectory(prefix='.fifo-', dir=here) as tmp:
    tmp = Path(tmp)
    for name in inputs:
        shutil.copy2(root/name, tmp/name)
    shutil.copy2(here/'surface_control_dl.h', tmp/'surface_control_dl.h')
    binary = tmp/'fifo'
    compile_result = subprocess.run(['c++', '-std=c++17', '-O1', '-g', '-Wall', '-Wextra', '-Werror',
        '-pthread', '-fsanitize=address,undefined', '-fno-omit-frame-pointer', '-I'+str(tmp),
        '-I'+str(here/'stub'), str(here/'presenter_fifo_test.cpp'), '-o', str(binary)],
        capture_output=True, text=True)
    (a.out/'compile.stdout').write_text(compile_result.stdout)
    (a.out/'compile.stderr').write_text(compile_result.stderr)
    assert compile_result.returncode == 0, compile_result.stderr
    for name, args in [('fifo', []), ('unavailable', ['unavailable'])]:
        r = subprocess.run([str(binary), *args], capture_output=True, text=True, timeout=10,
                           env={**os.environ, 'ASAN_OPTIONS': 'detect_leaks=0'})
        (a.out/(name+'.stdout')).write_text(r.stdout)
        (a.out/(name+'.stderr')).write_text(r.stderr)
        expected = 1 if a.expect_red and name == 'fifo' else 0
        passed = r.returncode == expected and ('FAIL queued scanout buffer dropped' in r.stderr if expected else 'PASS ' in r.stdout)
        result['cases'].append({'name': name, 'exit_code': r.returncode, 'expected_exit': expected, 'passed': passed})
result['passed'] = all(r['passed'] for r in result['cases'])
(a.out/'result.json').write_text(json.dumps(result, indent=2)+'\n')
print(json.dumps(result))
raise SystemExit(0 if result['passed'] else 1)
