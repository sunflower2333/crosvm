#!/usr/bin/env node
// Own build tree, copied dependency/output cache; frozen inputs are read-only.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
const root = '/home/sunf/droidvm-repos';
const base = path.join(root, '.artifacts/native-display-release-aosp');
const output = path.join(root, '.artifacts/native-shared-allocation-aosp');
const refreshOnly = process.argv.includes('--refresh-sources');
const invalidateSources = process.argv.includes('--invalidate-sources');
const resume = process.argv.includes('--resume') || refreshOnly;
if (fs.existsSync(output) && (!resume || !fs.lstatSync(output).isDirectory() ||
        fs.lstatSync(output).isSymbolicLink())) {
    throw new Error('Existing owned build directory requires explicit --resume');
}
const replacements = new Map([
    ['external/crosvm', ['crosvm-native-shared-allocation-20260912', '14d5bb5e']],
    ['external/virglrenderer', ['virgl-native-shared-allocation-20260912', 'c30846b']],
    ['packages/modules/Virtualization', ['virtualization-native-shared-allocation-20260912', 'f0fa645']],
    ['build/soong', ['soong-16kb-20260912', '965ff70']],
    ['external/minigbm', ['minigbm-16kb-20260912', 'e87417c']],
]);
const provenance = {};
for (const [relative, [name, baseline]] of replacements) {
    const source = path.join(root, 'reference/codes', name);
    execFileSync('git', ['merge-base', '--is-ancestor', baseline, 'HEAD'], {cwd: source});
    provenance[relative] = {
        source, baseline,
        head: execFileSync('git', ['rev-parse', 'HEAD'], {cwd: source, encoding: 'utf8'}).trim(),
        status: execFileSync('git', ['status', '--short'], {cwd: source, encoding: 'utf8'}),
    };
}
let files = 0, bytes = 0;
function copy(relative, input = path.join(base, relative), refresh = false) {
    if (relative === 'out/.ninja_fifo' || relative === 'out/.path_interposer_log') return;
    if (provenance[relative]) { input = provenance[relative].source; refresh = true; }
    const target = path.join(output, relative);
    const stat = fs.lstatSync(input);
    if (stat.isSymbolicLink()) {
        if (fs.existsSync(target) || (() => { try { return fs.lstatSync(target).isSymbolicLink(); } catch { return false; } })()) {
            if (!fs.lstatSync(target).isSymbolicLink() || fs.readlinkSync(target) !== fs.readlinkSync(input)) {
                throw new Error(`Mismatched resumed symlink: ${target}`);
            }
            return;
        }
        fs.symlinkSync(fs.readlinkSync(input), target);
    } else if (stat.isDirectory()) {
        fs.mkdirSync(target, {recursive: true});
        for (const child of fs.readdirSync(input)) {
            if (child === '.git' || child === '.repo') continue;
            copy(path.join(relative, child), path.join(input, child), refresh);
        }
        fs.chmodSync(target, stat.mode & 0o777);
        fs.utimesSync(target, stat.atime, stat.mtime);
    } else if (stat.isFile()) {
        if (refresh && fs.existsSync(target)) {
            const previous = fs.lstatSync(target);
            if (!previous.isFile()) throw new Error(`Mismatched source file: ${target}`);
            if (previous.size === stat.size && fs.readFileSync(input).equals(fs.readFileSync(target))) {
                if (invalidateSources) fs.utimesSync(target, new Date(), new Date());
                return;
            }
        }
        if (!refresh && fs.existsSync(target)) {
            const previous = fs.lstatSync(target);
            if (!previous.isFile()) throw new Error(`Mismatched resumed file: ${target}`);
            if (previous.size === stat.size && Math.abs(previous.mtimeMs - stat.mtimeMs) < 1) return;
        }
        // Never hard-link mutable build outputs. Reflink falls back to copy.
        fs.copyFileSync(input, target, fs.constants.COPYFILE_FICLONE);
        fs.chmodSync(target, stat.mode & 0o777);
        // A checkout can change while the independent tree builds. Its source
        // mtime can precede an object compiled from the old copied content;
        // changed source must get its refresh time, not the checkout timestamp.
        fs.utimesSync(target, stat.atime, refresh ? new Date() : stat.mtime);
        ++files;
        bytes += stat.size;
    } else {
        throw new Error(`Unexpected special file in frozen build: ${input}`);
    }
}
if (refreshOnly) {
    if (!fs.existsSync(path.join(output, 'native-display-source-manifest.json')))
        throw new Error('Source refresh requires a completed owned assembly');
    for (const relative of replacements.keys()) copy(relative);
} else {
    copy('');
}
// Generated boot input is unchanged and is not part of shallow source clones.
const shimInput = path.join(base, 'external/crosvm/hypervisor/src/gunyah/shim.bin');
const shimOutput = path.join(output, 'external/crosvm/hypervisor/src/gunyah/shim.bin');
if (!fs.existsSync(shimOutput) || !fs.readFileSync(shimInput).equals(fs.readFileSync(shimOutput)))
    fs.copyFileSync(shimInput, shimOutput);
fs.writeFileSync(path.join(output, 'native-display-source-manifest.json'),
    JSON.stringify({base, output, files, bytes, provenance}, null, 2) + '\n');
console.log(JSON.stringify({output, files, bytes}));
