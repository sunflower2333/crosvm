#!/usr/bin/env node
// Build the two actual production sources with generated Soong flags and
// existing platform link stubs. Do not rebuild/replace Android's libc or libui.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';

const root = '/home/sunf/droidvm-repos';
const aosp = path.join(root, '.artifacts/native-shared-allocation-aosp');
const output = path.resolve(process.argv[2] ??
    path.join(root, '.artifacts/native-shared-ahb-probe-20260922-v2'));
const baseline = JSON.parse(fs.readFileSync(path.join(root,
    '.artifacts/hdr-pq-quantized-20260911/manifest.json')));
const source = path.resolve(import.meta.dirname, '../..');
const compiler = baseline.compiler;
const mode = process.argv[3] ?? 'allocator';
if (!['allocator', 'paging'].includes(mode)) throw new Error('Expected allocator or paging');
const probeSource = `tests/shared_allocation/${mode}_probe.cpp`;
const ninja = execFileSync('prebuilts/build-tools/linux-x86/bin/ninja',
    ['-f', 'out/combined-aosp_arm64.ninja', '-t', 'commands', 'crosvm_shared_ahb_probe'],
    {cwd: aosp, encoding: 'utf8', maxBuffer: 64*1024*1024});
const lines = ninja.split('\n');
const compiled = [];
if (fs.existsSync(path.join(output, 'manifest.json'))) throw new Error('Frozen manifest exists');
fs.mkdirSync(output, {recursive: true});
for (const relative of ['shared_ahb_allocation.cpp', probeSource]) {
    const templateSource = relative === probeSource ? 'tests/shared_allocation/allocator_probe.cpp' : relative;
    const needle = `packages/modules/Virtualization/libs/android_display_backend/${templateSource}`;
    const matches = lines.filter(line => line.endsWith(` ${needle}`) && line.includes('clang++ -c '));
    if (matches.length !== 1) throw new Error(`Expected one actual compile command for ${relative}`);
    const command = matches[0].replace(/^PWD=\/proc\/self\/cwd /, '');
    // These generated compile commands need no shell expansion. Refuse to
    // interpret any quoting/metacharacter instead of evaluating shell text.
    if (/[;|&$"'`\\]/.test(command)) throw new Error('Unexpected shell syntax in compile command');
    const words = command.trim().split(/\s+/);
    if (words.shift() !== compiler) throw new Error('Compiler mismatch');
    // The generated command belongs to a separate AOSP checkout. Compile the
    // selected worktree's public headers before any stale module-local -I.
    words.unshift('-I' + source);
    const object = path.join(output, path.basename(relative, '.cpp') + '.o');
    words[words.indexOf('-o')+1] = object;
    words[words.indexOf('-MF')+1] = object + '.d';
    words[words.length-1] = path.join(source, relative);
    execFileSync(path.join(aosp, compiler), words, {cwd: aosp, stdio: 'inherit'});
    compiled.push({relative, object, args: words});
}
const args = baseline.linkArgs.flatMap(arg => arg.endsWith('/hdr_precision_probe_main.o')
    ? compiled.map(value => value.object) : [arg]);
const executable = path.join(output, 'crosvm_shared_ahb_probe');
args[args.indexOf('-o')+1] = executable;
const libui = 'out/soong/.intermediates/frameworks/native/libs/ui/libui/android_arm64_armv8-a_shared/libui.so';
args.splice(args.indexOf('-o'), 0, libui, '-Wl,-z,common-page-size=16384');
execFileSync(path.join(aosp, compiler), args, {cwd: aosp, stdio: 'inherit'});
const hash = filename => crypto.createHash('sha256').update(fs.readFileSync(filename)).digest('hex');
const sourceFiles = ['shared_ahb_allocation.cpp', 'shared_ahb_allocation.h', 'shared_ahb_layout.h',
    probeSource];
const linkedInputs = args.filter(arg => arg.endsWith('.so') || arg.endsWith('.a') || arg.endsWith('.o'));
const manifest = {purpose: mode === 'paging' ? 'Explicit paging byte operations on authentic AHB only'
    : 'Authentic AHB allocation and bidirectional DMA alias probe only', mode,
    aosp, source, compiler, compiled, linkArgs: args,
    sources: Object.fromEntries(sourceFiles.map(name => [name, hash(path.join(source,name))])),
    linkedInputs: Object.fromEntries(linkedInputs.map(name => [name, hash(path.resolve(aosp,name))])),
    artifact: {path: executable, bytes: fs.statSync(executable).size, sha256: hash(executable)},
    requiresPlatformLibui: true, guestMapping: false, zeroCopyEnabled: false,
    systemLibcRebuilt: false};
fs.writeFileSync(path.join(output,'manifest.json'), JSON.stringify(manifest,null,2)+'\n');
console.log(JSON.stringify(manifest.artifact));
