//! Protobuf codegen for the SFU wire protocol, and the static libvpx and
//! libopus.
//!
//! The RTC stack is core (no `webrtc` feature), so codegen always runs. We parse
//! the vendored `.proto` sources with the pure-Rust `protox` compiler and emit
//! types with `prost-build`'s `compile_fds`, which consumes the descriptor set
//! directly and never shells out to `protoc`.
//!
//! The published package carries prebuilt codec archives in `native/<target>`.
//! A git checkout has none, so the build downloads the pinned sources, checks
//! their SHA-256, and builds them in `OUT_DIR`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

fn main() {
    generate_sfu_protocol();
    link_codecs();
}

/// A pinned source archive. `name` is also its top-level directory.
struct Source {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
}

/// The archive of the v1.17.0 tag, commit 6df3ec34557879fff673706f4a1d9fbd0f3a6f0e.
const LIBVPX: Source = Source {
    name: "libvpx-1.17.0",
    url: "https://github.com/webmproject/libvpx/archive/refs/tags/v1.17.0.tar.gz",
    sha256: "1020f184046187baa2985dbde38e0691f49c44088bca7a1842b0236c6081dc0a",
};

/// The SHA-256 is the one in downloads.xiph.org/releases/opus/SHA256SUMS.txt.
const OPUS: Source = Source {
    name: "opus-1.6.1",
    url: "https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz",
    sha256: "6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1",
};

/// Links the static libvpx and libopus: the prebuilt archives of the published
/// package, or archives built from source in a git checkout.
fn link_codecs() {
    // docs.rs has no network, and rustdoc does not link.
    if std::env::var_os("DOCS_RS").is_some() {
        return;
    }
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    let prebuilt = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("native")
        .join(&target);
    let dir = if prebuilt.join("libvpx.a").is_file() && prebuilt.join("libopus.a").is_file() {
        prebuilt
    } else {
        build_codecs(&target)
    };

    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=vpx");
    println!("cargo:rustc-link-lib=static=opus");
    // A dependent build script reads this as `DEP_VPX_LICENSES`.
    println!("cargo:licenses={}", dir.display());
}

/// Builds libvpx and libopus from the pinned sources into
/// `OUT_DIR/native/<target>`, with the same files as a prebuilt directory.
fn build_codecs(target: &str) -> PathBuf {
    let vpx_target = match target {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => "x86_64-linux-gcc",
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => "arm64-linux-gcc",
        // darwin20 is macOS 11, the first macOS release on arm64.
        "aarch64-apple-darwin" => "arm64-darwin20-gcc",
        _ => panic!("getstream cannot build libvpx for {target}"),
    };
    if vpx_target.starts_with("x86_64") && Command::new("nasm").arg("-v").output().is_err() {
        panic!(
            "libvpx needs nasm on x86_64. Install it, for example with `apt install nasm`, \
             `dnf install nasm`, `apk add nasm` or `brew install nasm`."
        );
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let tools = Tools::new(target);

    let vpx = Cache::new(&out_dir, &LIBVPX);
    let vpx_target = format!("--target={vpx_target}");
    let vpx_build = vpx.configure_and_make(
        &[
            &vpx_target,
            "--enable-static",
            "--disable-shared",
            "--enable-pic",
            "--enable-runtime-cpu-detect",
            // The SDK encodes only with VPX_DL_REALTIME. The size limit stops a
            // remote participant from making the VP9 decoder allocate a very
            // large frame.
            "--enable-realtime-only",
            "--size-limit=4096x4096",
            "--disable-examples",
            "--disable-tools",
            "--disable-docs",
            "--disable-unit-tests",
            "--disable-install-bins",
        ],
        // The default target also builds the C++ libvpxrc, which the SDK
        // does not use.
        &["libvpx.a"],
        &tools,
    );

    let opus = Cache::new(&out_dir, &OPUS);
    let opus_build = opus.configure_and_make(
        &[
            "--enable-static",
            "--disable-shared",
            "--with-pic",
            "--disable-doc",
            "--disable-extra-programs",
            // Without CFLAGS, autotools adds -g.
            "CFLAGS=-O2",
        ],
        &[],
        &tools,
    );

    let dir = out_dir.join("native").join(target);
    fs::create_dir_all(&dir).expect("create the codec output directory");
    for (from, to) in [
        (vpx_build.join("libvpx.a"), "libvpx.a"),
        (vpx.tree().join("LICENSE"), "LICENSE-libvpx"),
        (vpx.tree().join("PATENTS"), "PATENTS-libvpx"),
        (opus_build.join(".libs").join("libopus.a"), "libopus.a"),
        (opus.tree().join("COPYING"), "COPYING-opus"),
    ] {
        fs::copy(&from, dir.join(to))
            .unwrap_or_else(|e| panic!("cannot copy {}: {e}", from.display()));
    }
    dir
}

/// The target toolchain that `configure` and `make` use.
struct Tools {
    cc: PathBuf,
    ar: OsString,
    jobs: String,
    /// `MACOSX_DEPLOYMENT_TARGET` and the value that rustc links with.
    deployment_target: Option<(String, String)>,
}

impl Tools {
    fn new(target: &str) -> Self {
        let build = cc::Build::new();
        let deployment_target = target.ends_with("-apple-darwin").then(|| {
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let output = Command::new(rustc)
                .args(["--print", "deployment-target", "--target", target])
                .output()
                .expect("run rustc --print deployment-target");
            let line = String::from_utf8(output.stdout).expect("rustc prints UTF-8");
            let (name, value) = line
                .trim()
                .split_once('=')
                .expect("rustc prints NAME=VERSION");
            (name.to_owned(), value.to_owned())
        });
        Self {
            cc: build.get_compiler().path().to_owned(),
            ar: build.get_archiver().get_program().to_owned(),
            jobs: std::env::var("NUM_JOBS").unwrap_or_else(|_| "1".to_owned()),
            deployment_target,
        }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env("CC", &self.cc).env("AR", &self.ar);
        if let Some((name, value)) = &self.deployment_target {
            command.env(name, value);
        }
        command
    }
}

/// The cache directory of one pinned source in `OUT_DIR`. Its name holds the
/// SHA-256, so a new version or checksum gets a new directory.
struct Cache<'a> {
    source: &'a Source,
    dir: PathBuf,
}

impl<'a> Cache<'a> {
    fn new(out_dir: &Path, source: &'a Source) -> Self {
        let dir = out_dir.join(format!("{}-{}", source.name, &source.sha256[..16]));
        Self { source, dir }
    }

    /// Returns the extracted source tree. Downloads only when the archive is
    /// missing or does not match its SHA-256.
    fn tree(&self) -> PathBuf {
        let tree = self.dir.join(self.source.name);
        if tree.is_dir() {
            return tree;
        }
        fs::create_dir_all(&self.dir).expect("create the cache directory");
        let archive = self.dir.join("source.tar.gz");
        if !archive.is_file() || sha256(&archive) != self.source.sha256 {
            self.download(&archive);
        }

        // The tree appears only after a complete extraction.
        let staging = self.dir.join("extracting");
        let _ = fs::remove_dir_all(&staging);
        let file = File::open(&archive).expect("open the source archive");
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(&staging)
            .unwrap_or_else(|e| panic!("cannot extract {}: {e}", archive.display()));
        fs::rename(staging.join(self.source.name), &tree).expect("move the source tree");
        let _ = fs::remove_dir_all(&staging);
        tree
    }

    /// Downloads the archive to a temporary file and keeps it only when its
    /// SHA-256 matches.
    fn download(&self, archive: &Path) {
        let url = self.source.url;
        let partial = self.dir.join("source.tar.gz.part");
        let response = ureq::get(url)
            .call()
            .unwrap_or_else(|e| panic!("cannot download {url}: {e}"));
        let mut file = File::create(&partial).expect("create the download file");
        io::copy(&mut response.into_body().into_reader(), &mut file)
            .unwrap_or_else(|e| panic!("cannot download {url}: {e}"));
        let actual = sha256(&partial);
        if actual != self.source.sha256 {
            let _ = fs::remove_file(&partial);
            panic!(
                "{url} has SHA-256 {actual}, expected {}",
                self.source.sha256
            );
        }
        fs::rename(&partial, archive).expect("move the download into the cache");
    }

    /// Runs `configure` with `args` and then `make` for `targets` in a new
    /// build directory, unless a build with the same arguments finished before.
    fn configure_and_make(&self, args: &[&str], targets: &[&str], tools: &Tools) -> PathBuf {
        let build = self.dir.join("build");
        let stamp = self.dir.join("built");
        let key = [args, targets].concat().join(" ");
        if fs::read_to_string(&stamp).is_ok_and(|built| built == key) {
            return build;
        }

        let tree = self.tree();
        let _ = fs::remove_file(&stamp);
        let _ = fs::remove_dir_all(&build);
        fs::create_dir_all(&build).expect("create the build directory");
        run(tools
            .command("sh")
            .arg(tree.join("configure"))
            .args(args)
            .current_dir(&build));
        run(tools
            .command("make")
            .arg(format!("-j{}", tools.jobs))
            .args(targets)
            .current_dir(&build));
        fs::write(&stamp, key).expect("write the build stamp");
        build
    }
}

fn sha256(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Runs `command` and stops the build when it fails.
fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("cannot run {command:?}: {e}"));
    assert!(status.success(), "{command:?} failed with {status}");
}

fn generate_sfu_protocol() {
    // Paths are relative to the `proto/` include root, matching the upstream
    // `import "video/sfu/..."` statements.
    const INCLUDE_ROOT: &str = "proto";
    const PROTOS: [&str; 3] = [
        "video/sfu/models/models.proto",
        "video/sfu/signal_rpc/signal.proto",
        "video/sfu/event/events.proto",
    ];

    println!("cargo:rerun-if-changed={INCLUDE_ROOT}");
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={INCLUDE_ROOT}/{proto}");
    }

    // protox parses and links the .proto files (well-known google.protobuf.*
    // types are bundled) into a FileDescriptorSet, entirely in Rust.
    let file_descriptors = protox::compile(PROTOS, [INCLUDE_ROOT])
        .expect("protox failed to compile the vendored SFU .proto sources");

    // Emit a single include file that reconstructs the package module tree, so
    // cross-package references (signal -> models, event -> models/signal) resolve.
    let mut config = prost_build::Config::new();
    config.include_file("_sfu_proto.rs");
    // `JoinRequest` holds the SFU token; `src/rtc/proto.rs` has its redacting `Debug`.
    config.skip_debug([".stream.video.sfu.event.JoinRequest"]);

    config
        .compile_fds(file_descriptors)
        .expect("prost-build failed to generate Rust types from the SFU descriptors");
}
