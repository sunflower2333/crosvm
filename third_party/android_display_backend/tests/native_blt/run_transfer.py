#!/usr/bin/env python3
from pathlib import Path
import subprocess
import tempfile
here=Path(__file__).resolve().parent
with tempfile.TemporaryDirectory(prefix='native-blt-transfer-') as directory:
    binary=Path(directory)/'test'
    subprocess.run(['c++','-std=c++20','-Wall','-Wextra','-Werror','-fsanitize=address,undefined',
                    '-fno-omit-frame-pointer','-I',str(here.parents[1]),str(here/'transfer_test.cpp'),
                    '-o',str(binary)],check=True)
    raise SystemExit(subprocess.run([str(binary)]).returncode)
