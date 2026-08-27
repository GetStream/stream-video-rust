import { createRequire } from 'node:module';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const bindingDirectory = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const binding = require(bindingDirectory);

const version = binding.bindingApiVersion();
if (version !== 1) {
  throw new Error(`unexpected binding API ${version}`);
}

const client = new binding.NativeStreamClient('key', 'secret');
const call = client.call('default', 'node-binding-smoke');

const state = JSON.parse(await call.stateJson());
if (state.callingState !== 'idle') {
  throw new Error(`unexpected initial state ${state.callingState}`);
}
if (state.sessionId != null) {
  throw new Error(`unexpected session id before join: ${state.sessionId}`);
}
if (state.participantCount !== 0 || state.participants.length !== 0) {
  throw new Error('expected an empty participant roster before join');
}

// Local tracks must construct without a system libvpx.
binding.NativeLocalAudioTrack.opus();
for (const codec of ['vp8', 'vp9', 'h264']) {
  binding.NativeLocalVideoTrack[codec](JSON.stringify({ layering: { mode: 'single' } }));
}

// Errors cross the boundary as encoded RTC codes, not opaque strings.
let code;
try {
  await call.muteTrack('not-a-track-type');
} catch (error) {
  code = JSON.parse(error.message).code;
}
if (code !== 'RTC_MEDIA') {
  throw new Error(`expected a structured RTC error, got ${code}`);
}

console.log('Node RTC binding smoke test passed');
