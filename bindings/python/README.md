# getstream-rtc-core

Python bindings for the Stream Video Rust SFU participant stack. The wheel
exposes `RtcSession` over pre-fetched coordinator credentials; Python keeps
location discovery, coordinator join, and coordinator watch.

## Build a local wheel

From this directory, with libvpx, cmake, and a C compiler installed:

```bash
pip install maturin
maturin build --release
```

The wheel is written to `target/wheels/` at the repository root (Cargo
workspace target). Install it with:

```bash
pip install --force-reinstall ../../target/wheels/getstream_rtc_core-*.whl
```

For an editable install while developing:

```bash
maturin develop --release
```

Requires Python 3.10+ (abi3).
