#!/usr/bin/env node
// Independent copy of the accepted matched build cache: never hardlink or
// mutate previous source/output artifacts. Only tracked owned sources overlay.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync, spawnSync} from 'node:child_process';
import crypto from 'node:crypto';

const workspace = '/home/sunf/droidvm-repos';
const source = path.resolve(import.meta.dirname, '../../..');
const input = path.join(workspace, '.artifacts/native-shared-allocation-aosp');
const output = path.join(workspace, '.artifacts/native-owner-recovery-aosp-20260912');
const evidence = path.join(workspace, '.artifacts/native-owner-recovery-20260912');
if (fs.lstatSync(output, {throwIfNoEntry:false})?.isSymbolicLink()) throw new Error('Symlinked output');
if (!fs.existsSync(output)) {
    execFileSync('cp', ['-a', '--reflink=auto', input, output], {stdio:'inherit'});
}
const sha256 = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const files = execFileSync('git', ['ls-files', '-z'], {cwd:source, encoding:'utf8'}).split('\0').filter(Boolean);
const manifest = {};
for (const relative of files) {
    const from = path.join(source, relative), to = path.join(output, 'external/crosvm', relative);
    if (!fs.lstatSync(from).isFile()) continue;
    const digest = sha256(from);
    manifest[relative] = digest;
    if (fs.existsSync(to) && fs.lstatSync(to).isFile() && sha256(to) === digest) continue;
    if (fs.lstatSync(to, {throwIfNoEntry:false})?.isSymbolicLink()) throw new Error('Linked overlay: '+to);
    fs.mkdirSync(path.dirname(to), {recursive:true});
    fs.copyFileSync(from, to, fs.constants.COPYFILE_FICLONE);
}
fs.mkdirSync(evidence, {recursive:true});
fs.writeFileSync(path.join(evidence, 'android-inputs.json'), JSON.stringify({
    source, input, output, commit:execFileSync('git',['rev-parse','HEAD'],{cwd:source,encoding:'utf8'}).trim(),
    files:manifest,
},null,2)+'\n');
const result = spawnSync('build/soong/soong_ui.bash', ['--make-mode','crosvm_device_only','-j12'], {
    cwd:output, stdio:'inherit', env:{
        ...process.env, PWF_PLAN_ROOT:path.join(workspace,'.planning/native-display-owner-recovery-20260912'),
        PATH:path.join(workspace,'.artifacts/large-backing-tools/root/usr/bin')+':'+process.env.PATH,
        TARGET_PRODUCT:'aosp_arm64', TARGET_RELEASE:'trunk_staging', TARGET_BUILD_VARIANT:'eng',
        ALLOW_MISSING_DEPENDENCIES:'true', TARGET_BUILD_UNBUNDLED:'true',
        ALLOW_BP_UNDER_SYMLINKS:'false', OUT_DIR:'out',
    },
});
if (result.status !== 0) process.exit(result.status ?? 1);
console.log('NATIVE_OWNER_RECOVERY_ANDROID_BUILD_PASS');
