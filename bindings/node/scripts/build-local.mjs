// Build the host-platform addon and lay it out as an unpacked
// `@stream-io/node-rtc` package. Nothing here publishes: the output is meant to
// be consumed through STREAM_NODE_RTC_NATIVE_PATH.
import { copyFile, unlink } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const bindingDirectory = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(bindingDirectory, '..', '..');
const debug = process.argv.includes('--debug');
const profile = debug ? 'debug' : 'release';
const buildEnvironment = { ...process.env, VPX_STATIC: '1' };
const napi = resolve(bindingDirectory, 'node_modules', '.bin', 'napi');

execFileSync(
  napi,
  [
    'build',
    '--dts',
    'index.d.ts',
    '--output-dir',
    bindingDirectory,
    '--target-dir',
    resolve(repositoryRoot, 'target'),
    ...(debug ? [] : ['--release']),
  ],
  { cwd: bindingDirectory, env: buildEnvironment, stdio: 'inherit' },
);

const source = resolve(bindingDirectory, 'index.node');
const destination = resolve(bindingDirectory, 'stream-node-rtc.node');

await copyFile(source, destination);
await unlink(source);

const dependencies =
  process.platform === 'darwin'
    ? execFileSync('otool', ['-L', destination], { encoding: 'utf8' })
    : process.platform === 'linux'
      ? execFileSync('ldd', [destination], { encoding: 'utf8' })
      : '';

if (/libvpx[^\n]*(?:\.dylib|\.so)/i.test(dependencies)) {
  throw new Error(
    `local addon must link libvpx statically, but its dependencies include:\n${dependencies}`,
  );
}

console.log(`built ${profile} addon: ${destination}`);
console.log('verified: no runtime libvpx dependency');
console.log('');
console.log('Point the Node SDK at it with:');
console.log(`  export STREAM_NODE_RTC_NATIVE_PATH=${destination}`);
