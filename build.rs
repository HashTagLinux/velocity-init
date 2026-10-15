// build.rs — enforce the glibc floor the artifact is allowed to assume.
//
// velocity-init is a dynamically linked PID 1. Unlike an ordinary
// program it cannot be started at all on a machine with an older libc:
// there is no "error message", there is a kernel refusing to map
// `/lib64/ld-linux-x86-64.so.2`. Silently building such a binary on a
// newer host and shipping it is the classic way to produce an init that
// only fails on the machine it was meant to rescue.
//
// So the floor is checked here, at build time, rather than discovered by
// an operator at boot. Anything below the floor fails the build loudly.
//
// Only glibc targets are checked: a musl build has no GNU libc at all,
// and a `-C target-feature=+crt-static` build embeds its own libc, so
// there is no system runtime to be compatible with.

use std::process::Command;

/// The oldest glibc velocity-init supports. Devuan 7 (freia/ceres)
/// ships 2.43, and that is the distribution this init replaces.
const MINIMUM: (u32, u32) = (2, 43);

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VELOCITY_MIN_GLIBC");

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") {
        return; // musl (or another libc): nothing to check against
    }

    let minimum = std::env::var("VELOCITY_MIN_GLIBC")
        .ok()
        .and_then(|s| parse_version(&s))
        .unwrap_or(MINIMUM);

    let Some((major, minor)) = detect_glibc() else {
        // Fail open, but never quietly: a build machine without glibc
        // tooling is unusual, and silently skipping the check is exactly
        // how this class of bug comes back.
        println!(
            "cargo:warning=velocity-init: could not detect the build glibc; the >= {}.{} \
             floor was NOT checked",
            minimum.0, minimum.1
        );
        return;
    };

    if (major, minor) < minimum {
        panic!(
            "velocity-init requires glibc >= {}.{}, but this build targets glibc {}.{}.\n\
             Building against an older libc produces a PID 1 that cannot start on the target.\n\
             Build inside the target distribution (docker/Dockerfile.freia), or override the \
             floor with VELOCITY_MIN_GLIBC=x.y if you know the artifact will run elsewhere.",
            minimum.0, minimum.1, major, minor
        );
    }
}

/// Parse `2.43`, `2.43-7`, `GLIBC_2.43` → `(2, 43)`.
///
/// The token must contain a dot: `ldd --version` also prints "Copyright
/// (C) 2024", and a bare year must not be mistaken for major version
/// 2024 — that would wave every old build through.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let text = text.trim();
    if !text.contains('.') || !text.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut parts = text.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Ask the build machine what glibc it has. `getconf` is the cheapest
/// and does not depend on the linker's output format; `ldd` is the
/// fallback for images without libc-bin.
fn detect_glibc() -> Option<(u32, u32)> {
    for (program, args) in [
        ("getconf", vec!["GNU_LIBC_VERSION"]),
        ("ldd", vec!["--version"]),
    ] {
        let Ok(out) = Command::new(program).args(args).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        // `getconf` prints "glibc 2.43"; `ldd --version` prints several
        // lines ending in the bare version, so check every token.
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            for token in line.split_whitespace() {
                if let Some(v) = parse_version(token) {
                    return Some(v);
                }
            }
        }
    }
    None
}