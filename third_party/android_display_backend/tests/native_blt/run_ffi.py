#!/usr/bin/env python3
from pathlib import Path
import subprocess
import tempfile
here=Path(__file__).resolve().parent
root=here.parents[1]
def function(path, signature):
    text=path.read_text(); start=text.index(signature); begin=text.index('{',start); depth=1; end=begin+1
    while depth:
        depth += (text[end]=='{') - (text[end]=='}'); end+=1
    return text[start:end]
s=(here/'ffi_test.cpp').read_text()
s=s.replace('// INSERT_REAL_DESCRIPTION_HELPER',function(root/'shared_ahb_allocation.cpp','extern "C" const SharedAllocationDescription* crosvm_shared_ahb_get_description('))
s=s.replace('// INSERT_REAL_COPY_FFI',function(root/'crosvm_android_display_client.cpp','extern "C" int android_display_native_blt_copy('))
with tempfile.TemporaryDirectory(prefix='native-blt-ffi-') as temporary:
    p=Path(temporary); (p/'test.cpp').write_text(s)
    subprocess.run(['c++','-std=c++20','-Wall','-Wextra','-Werror','-fsanitize=address,undefined','-fno-omit-frame-pointer',
                    '-I',str(root),str(p/'test.cpp'),'-o',str(p/'test')],check=True)
    raise SystemExit(subprocess.run([str(p/'test')]).returncode)
