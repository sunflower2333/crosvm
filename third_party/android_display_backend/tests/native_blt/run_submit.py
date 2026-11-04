#!/usr/bin/env python3
from pathlib import Path
import subprocess,tempfile
here=Path(__file__).resolve().parent;root=here.parents[1]
s=(root/'crosvm_android_display_client.cpp').read_text()
a=s.index('        VkResult result;',s.index('Result<int> blit('));b=s.index('        if (!mAsyncBlitEnabled) {',a)
fixture=(here/'submit_test.cpp').read_text().replace('// INSERT_SUBMIT',s[a:b])
a=s.index('    bool prepareNativeDestruction()');b=s.index('{',a)+1;depth=1
while depth:
 depth+=(s[b]=='{')-(s[b]=='}');b+=1
fixture=fixture.replace('// INSERT_DRAIN',s[a:b])
with tempfile.TemporaryDirectory(prefix='native-blt-submit-') as d:
 p=Path(d);(p/'test.cpp').write_text(fixture)
 subprocess.run(['c++','-std=c++17','-Wall','-Wextra','-Werror','-fsanitize=address,undefined',str(p/'test.cpp'),'-o',str(p/'test')],check=True)
 raise SystemExit(subprocess.run([str(p/'test')]).returncode)
