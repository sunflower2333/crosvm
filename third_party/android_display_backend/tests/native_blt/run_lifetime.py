#!/usr/bin/env python3
from pathlib import Path
import re
import subprocess
import tempfile
here=Path(__file__).resolve().parent; root=here.parents[1]
s=(root/'crosvm_android_display_client.cpp').read_text()
a=re.search(r'extern "C" (?:void|int) android_display_native_blt_destroy\(',s).start(); b=s.index('{',a); depth=1; e=b+1
while depth:
 depth+=(s[e]=='{')-(s[e]=='}'); e+=1
fixture=(here/'lifetime_test.cpp').read_text().replace('// INSERT_REAL_DESTROY',s[a:e])
a=s.index('    bool prepareNativeDestruction()'); b=s.index('{',a); depth=1; e=b+1
while depth:
 depth+=(s[e]=='{')-(s[e]=='}'); e+=1
fixture=fixture.replace('// INSERT_REAL_DRAIN',s[a:e])
with tempfile.TemporaryDirectory(prefix='native-blt-close-') as tmp:
 p=Path(tmp); (p/'test.cpp').write_text(fixture)
 subprocess.run(['c++','-std=c++17','-Wall','-Wextra','-Werror','-fsanitize=address,undefined','-I',str(root),str(p/'test.cpp'),'-o',str(p/'test')],check=True)
 raise SystemExit(subprocess.run([str(p/'test')]).returncode)
