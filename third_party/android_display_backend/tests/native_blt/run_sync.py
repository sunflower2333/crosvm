#!/usr/bin/env python3
from pathlib import Path
import subprocess,tempfile
here=Path(__file__).resolve().parent
root=here.parents[1]
s=(root/'crosvm_android_display_client.cpp').read_text()
signature='extern "C" int android_display_native_blt_copy_wait('
assert signature in s,'RED: bounded native copy completion ABI missing'
start=s.index(signature);end=s.index('{',start)+1;depth=1
while depth:
 depth+=(s[end]=='{')-(s[end]=='}');end+=1
fixture=(here/'sync_test.cpp').read_text().replace('// INSERT_REAL_SYNC',s[start:end])
with tempfile.TemporaryDirectory(prefix='native-blt-sync-') as d:
 p=Path(d);(p/'test.cpp').write_text(fixture)
 subprocess.run(['c++','-std=c++20','-Wall','-Wextra','-Werror','-fsanitize=address,undefined','-I',str(root),str(p/'test.cpp'),'-o',str(p/'test')],check=True)
 raise SystemExit(subprocess.run([str(p/'test')]).returncode)
