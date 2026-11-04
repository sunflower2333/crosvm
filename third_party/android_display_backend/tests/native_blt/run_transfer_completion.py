#!/usr/bin/env python3
from pathlib import Path
import subprocess
import tempfile
here=Path(__file__).resolve().parent
root=here.parents[1]
s=(root/'crosvm_android_display_client.cpp').read_text()
start=s.index('    Result<void> transferNative(')
begin=s.index('{',start); end=begin+1; depth=1
while depth:
    depth+=(s[end]=='{')-(s[end]=='}'); end+=1
tail=s[s.index('        // Conservatively retain command/image/staging objects',begin):end-1]
fixture=(here/'transfer_completion_test.cpp').read_text().replace('// INSERT_REAL_COMPLETION',tail)
with tempfile.TemporaryDirectory(prefix='native-blt-transfer-completion-') as directory:
    p=Path(directory); (p/'test.cpp').write_text(fixture)
    subprocess.run(['c++','-std=c++20','-Wall','-Wextra','-Werror','-fsanitize=address,undefined',
        '-fno-omit-frame-pointer','-I',str(root),str(p/'test.cpp'),'-o',str(p/'test')],check=True)
    raise SystemExit(subprocess.run([str(p/'test')]).returncode)
