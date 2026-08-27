# Local Node RTC binding

This directory is an unpublished NAPI-RS binding used by the
`feat/rust-rtc-bindings` branch of `stream-node`.

```bash
npm run build:local
export STREAM_NODE_RTC_NATIVE_PATH="$PWD/stream-node-rtc.node"
```

Applications consume the public types from `@stream-io/node-sdk`; this package
is an internal bridge and is not published by the branch-local prototype.
