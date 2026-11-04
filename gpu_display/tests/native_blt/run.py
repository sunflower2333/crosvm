#!/usr/bin/env python3
from pathlib import Path
import subprocess,tempfile
here=Path(__file__).resolve().parent
with tempfile.TemporaryDirectory(prefix='native-blt-rust-') as temporary:
 out=Path(temporary)/'test'
 subprocess.run(['rustc','--edition=2021','--test','--cfg','feature="android_display"',str(here/'bridge_test.rs'),'-o',str(out)],check=True)
 raise SystemExit(subprocess.run([str(out)]).returncode)
