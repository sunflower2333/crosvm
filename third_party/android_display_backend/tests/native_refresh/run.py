#!/usr/bin/env python3
"""Compile actual attach/event/generation/present methods with presenter double."""
from pathlib import Path
import os
import re
import shutil
import subprocess
import tempfile

here = Path(__file__).resolve().parent
root = here.parents[1]
source = (root / 'crosvm_android_display_client.cpp').read_text()

def method(name):
    match = re.search(r'^    [^;\n]*\b' + name + r'\([^;]*?\)\s*\{', source, re.M)
    if not match:
        raise RuntimeError('production method missing: ' + name)
    end = source.index('{', match.start()) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[match.start():end]

methods = '\n'.join(method(name) for name in ['setNativeSurface', 'setNativeRefreshEvent',
    'nativeRefreshGeneration', 'signalNativeRefreshLocked', 'presentShared'])
fixture = (here / 'refresh_test.cpp').read_text()
variants = [('production', methods),
    ('stale-submit', methods.replace('expectedGeneration != mNativeSurfaceGeneration', 'false')),
    ('missing-attach-notify', methods.replace('            signalNativeRefreshLocked();', '            ;', 1)),
    ('repeat-front-refresh', methods.replace('mDirectPresentedGeneration != mNativeSurfaceGeneration', 'true'))]
with tempfile.TemporaryDirectory(prefix='.native-refresh-', dir=here) as temp:
    out = Path(temp)
    for name in ['surface_buffer_presenter.h', 'consumer_release.h', 'surface_attachment_lease.h']:
        shutil.copy(root / name, out)
    shutil.copy(root / 'tests/presenter_async/surface_control_dl.h', out)
    for name, body in variants:
        unit = out / (name + '.cpp')
        unit.write_text(fixture.replace('// INSERT_PRODUCTION', body))
        binary = out / name
        subprocess.run(['c++', '-std=c++17', '-O1', '-g', '-Wall', '-Wextra', '-Werror', '-pthread',
            '-fsanitize=address,undefined', '-I' + str(out), '-I' + str(root / 'tests/presenter_async/stub'),
            str(unit), '-o', str(binary)], check=True)
        result = subprocess.run([str(binary)], capture_output=True, text=True,
            env={**os.environ, 'ASAN_OPTIONS': 'detect_leaks=0'}, cwd=out)
        if (result.returncode == 0) != (name == 'production'):
            raise SystemExit(name + ': unexpected result\n' + result.stdout + result.stderr)
        print('PASS native refresh ' + name + (' rejected' if name != 'production' else ''))
