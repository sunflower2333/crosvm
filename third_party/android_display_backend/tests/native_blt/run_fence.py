#!/usr/bin/env python3
from pathlib import Path
import subprocess
import tempfile
here=Path(__file__).resolve().parent; root=here.parents[1]
def function(path, signature):
 s=path.read_text(); a=s.index(signature); b=s.index('{',a); depth=1; e=b+1
 while depth:
  depth+=(s[e]=='{')-(s[e]=='}'); e+=1
 return s[a:e]
s='#include <initializer_list>\n'+(here/'fence_test.cpp').read_text()
s=s.replace('// INSERT_REAL_FENCE_STATUS',function(root/'consumer_release.h','inline int consumerFenceStatus('))
s=s.replace('// INSERT_REAL_ORACLE_WAIT','bool synchronousNativeCopy=false;\n'+function(here/'gpu_probe.cpp','void waitComplete('))
with tempfile.TemporaryDirectory(prefix='native-blt-fence-') as tmp:
 p=Path(tmp); (p/'test.cpp').write_text(s)
 subprocess.run(['c++','-std=c++17','-Wall','-Wextra','-Werror','-fsanitize=address,undefined',str(p/'test.cpp'),'-o',str(p/'test')],check=True)
 raise SystemExit(subprocess.run([str(p/'test')]).returncode)
