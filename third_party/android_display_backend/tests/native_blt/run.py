#!/usr/bin/env python3
"""Portable region contract; hardware pixels are tested separately."""
from pathlib import Path
import subprocess
import tempfile

here = Path(__file__).resolve().parent
with tempfile.TemporaryDirectory(prefix='native-blt-') as temporary:
    binary = Path(temporary) / 'geometry'
    subprocess.run(['c++', '-std=c++17', '-Wall', '-Wextra', '-Werror',
                    '-fsanitize=address,undefined', '-fno-omit-frame-pointer',
                    '-I', str(here.parents[1]), str(here / 'geometry_test.cpp'),
                    '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
