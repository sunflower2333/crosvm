#!/usr/bin/env node
// Bounded Android functional fixture using exact Soong C++ flags/platform stubs.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';

const root = '/home/sunf/droidvm-repos';
const aosp = path.join(root, '.artifacts/native-shared-allocation-aosp');
const output = path.resolve(process.argv[2] ??
    path.join(root, '.artifacts/native-shared-import-probe-20260922-v2'));
const gpuWrite = process.argv[3] === 'gpu-write';
if (process.argv[3] && !gpuWrite) throw new Error('Expected optional gpu-write fixture selector');
const fixture = gpuWrite ? 'gpu_write_probe' : 'import_probe';
const binary = gpuWrite ? 'crosvm_shared_gpu_write_probe' : 'crosvm_shared_import_probe';
const source = path.resolve(import.meta.dirname, '../..');
const virglSource = path.join(root, 'reference/codes/virgl-native-shared-allocation-20260912');
const baseline = JSON.parse(fs.readFileSync(path.join(root,
    '.artifacts/hdr-pq-quantized-20260911/manifest.json')));
const compiler = baseline.compiler;
if (fs.existsSync(path.join(output, 'manifest.json'))) throw new Error('Frozen manifest exists');
fs.mkdirSync(output, {recursive: true});
const commands = execFileSync('prebuilts/build-tools/linux-x86/bin/ninja',
    ['-f', 'out/combined-aosp_arm64.ninja', '-t', 'commands', 'crosvm_shared_ahb_probe'],
    {cwd: aosp, encoding: 'utf8', maxBuffer: 64*1024*1024}).split('\n');
const compiled = [];
for (const relative of ['shared_ahb_allocation.cpp', 'shared_ahb_dl.cpp',
                        `tests/shared_allocation/${fixture}.cpp`]) {
    const template = relative === 'shared_ahb_allocation.cpp' ? relative :
        'tests/shared_allocation/allocator_probe.cpp';
    const needle = `packages/modules/Virtualization/libs/android_display_backend/${template}`;
    const matches = commands.filter(line => line.endsWith(` ${needle}`) && line.includes('clang++ -c '));
    if (matches.length !== 1) throw new Error(`Expected one generated command for ${template}`);
    const command = matches[0].replace(/^PWD=\/proc\/self\/cwd /, '');
    if (/[;|&$"'`\\]/.test(command)) throw new Error('Unexpected generated shell syntax');
    const args = command.trim().split(/\s+/);
    if (args.shift() !== compiler) throw new Error('Compiler mismatch');
    const object = path.join(output, path.basename(relative, '.cpp') + '.o');
    args[args.indexOf('-o')+1] = object;
    args[args.indexOf('-MF')+1] = object + '.d';
    args[args.length-1] = path.join(source, relative);
    args.unshift('-I' + source, '-I' + path.join(virglSource, 'src'),
        '-I' + path.join(virglSource, 'src/drm'),
        '-I' + path.join(virglSource, 'src/compat'));
    args.push('-fPIC');
    execFileSync(path.join(aosp, compiler), args, {cwd: aosp, stdio: 'inherit'});
    compiled.push({relative, object, args});
}
const libui = 'out/soong/.intermediates/frameworks/native/libs/ui/libui/android_arm64_armv8-a_shared/libui.so';
const virgl = 'out/soong/.intermediates/external/virglrenderer/libvirglrenderer/android_arm64_armv8-a_shared/libvirglrenderer.so';
const links = [];
function link(filename, objects, libraries, shared) {
    let args = baseline.linkArgs.flatMap(arg => arg.endsWith('/hdr_precision_probe_main.o')
        ? objects : [arg]);
    if (shared) args = args.flatMap(arg => {
        if (arg === '-pie') return ['-shared'];
        if (arg.startsWith('-Wl,-dynamic-linker,')) return [];
        return [arg.replaceAll('crtbegin_dynamic', 'crtbegin_so').replaceAll('crtend_android', 'crtend_so')];
    });
    args[args.indexOf('-o')+1] = path.join(output, filename);
    args.splice(args.indexOf('-o'), 0, ...libraries, '-Wl,-z,common-page-size=16384');
    if (shared) args.push('-Wl,-soname,' + filename);
    execFileSync(path.join(aosp, compiler), args, {cwd: aosp, stdio: 'inherit'});
    links.push({filename,args});
}
link('libcrosvm_shared_ahb_allocation.so', [compiled[0].object], [libui], true);
link(binary, compiled.slice(1).map(item => item.object),
    [virgl, path.join(output, 'libcrosvm_shared_ahb_allocation.so')], false);
fs.copyFileSync(path.join(aosp, virgl), path.join(output, 'libvirglrenderer.so'));
const hash = filename => crypto.createHash('sha256').update(fs.readFileSync(filename)).digest('hex');
const artifacts = [binary, 'libcrosvm_shared_ahb_allocation.so', 'libvirglrenderer.so']
    .map(filename => ({filename, bytes: fs.statSync(path.join(output,filename)).size,
        sha256: hash(path.join(output,filename))}));
const tracked = ['shared_ahb_allocation.cpp','shared_ahb_allocation.h','shared_ahb_layout.h',
    'shared_ahb_dl.cpp',`tests/shared_allocation/${fixture}.cpp`,
    'tests/shared_allocation/build-import-probe.mjs'];
const dependencies = [...new Set(links.flatMap(item => item.args)
    .filter(arg => !arg.startsWith('-') && /\.(so|a|o)$/.test(arg)))];
fs.writeFileSync(path.join(output,'manifest.json'), JSON.stringify({
    purpose:gpuWrite ? 'Actual native AHB Virgl/KGSL CP_MEM_WRITE, retirement and pixel identity fixture' :
        'Actual optional AHB helper, Virgl DMA import and checked KGSL identity/cleanup fixture',
    aosp, source, virglSource, compiler, compiled, links,
    sources:Object.fromEntries(tracked.map(name => [name,hash(path.join(source,name))])),
    linkedInputs:Object.fromEntries(dependencies.map(name => [name,hash(path.resolve(aosp,name))])),
    artifacts, gpuSubmissions:gpuWrite ? 'runtime-observed' : 0,
    guestMapping:false, zeroCopyEnabled:false, shaderRendering:false, surfaceControl:false,
    systemLibrariesPackaged:false, legacyAllocatorAbiPreserved:true,
}, null, 2)+'\n');
console.log(JSON.stringify(artifacts));
