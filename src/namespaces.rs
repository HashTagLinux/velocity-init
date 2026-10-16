//! Runtime-environment probes that decide whether `reboot(2)` may be
//! issued at all.
//!
//! As PID 1 on a real machine, velocity-init owns the final step of a
//! shutdown: power off, halt, or reboot. Inside a *nested* runtime — a
//! container, or `scripts/sandbox.sh` — it is not the machine's init,
//! and the kernel turns a `reboot(2)` request from a nested PID
//! namespace into a signal to that namespace's init (ourselves): issuing
//! it kills us with SIGINT instead of achieving anything.
//!
//! `NSpid` in `/proc/self/status` (`pid_ns_depth` in `main.rs`) is the
//! primary probe, but it only sees the nesting when `/proc` is the outer
//! namespace's. A private `/proc` (mounted by `unshare --mount-proc`
//! and by container runtimes) lists a single, namespace-local value, so
//! the depth reads as 1 even though we are nested — the sandbox boot hit
//! exactly this and died of a rebooting SIGINT (exit 130). The probes
//! here cover that case:
//!
//! - [`in_mapped_user_namespace`]: `/proc/self/uid_map` shows a mapping
//!   other than the initial namespace's identity mapping. Every sandbox
//!   made with `unshare --user --map-root-user` has one, and so does
//!   any container with a uid map. A real PID 1 never tests positive:
//!   the initial user namespace is always `0 0 4294967295`.
//! - [`sandbox_requested`]: `VELOCITY_SANDBOX=1`, exported by
//!   `scripts/sandbox.sh`, as an explicit harness override.
//!
//! Known gap: a root-created *full-identity* user namespace (mapping
//! `0 0 4294967295`) is indistinguishable from the initial namespace by
//! these probes and escapes detection.
//!
//! Both probes follow the PID-1 rule — never panic — and on any doubt
//! answer "not nested", which keeps a real machine's reboot path intact.

/// `/proc` file describing this process's user-namespace uid mapping.
const UID_MAP_PATH: &str = "/proc/self/uid_map";

/// Environment variable `scripts/sandbox.sh` sets to `1`.
pub const SANDBOX_ENV: &str = "VELOCITY_SANDBOX";

/// The initial user namespace's mapping, as the kernel prints it.
const IDENTITY_MAPPING: (u64, u64, u64) = (0, 0, u32::MAX as u64);

/// First `(inside, outside, length)` mapping of a `uid_map` file.
///
/// Only the first line is considered: the map is sorted by inside id,
/// so the identity mapping is the whole file exactly when the first
/// line is the full range. A line with the wrong field count or a
/// non-numeric field yields `None`.
fn first_uid_mapping(content: &str) -> Option<(u64, u64, u64)> {
    let mut fields = content.lines().next()?.split_whitespace();
    let inside = fields.next()?.parse().ok()?;
    let outside = fields.next()?.parse().ok()?;
    let length = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((inside, outside, length))
}

/// True for a *positively identified* non-identity mapping.
///
/// A missing or malformed map is `false`: the doubt is resolved toward
/// `reboot(2)`, the value a real PID 1 needs.
fn mapped_from(content: Option<&str>) -> bool {
    matches!(
        content.and_then(first_uid_mapping),
        Some(mapping) if mapping != IDENTITY_MAPPING
    )
}

/// True when this process runs under a uid mapping other than the
/// initial namespace's identity mapping.
pub fn in_mapped_user_namespace() -> bool {
    mapped_from(std::fs::read_to_string(UID_MAP_PATH).ok().as_deref())
}

fn sandbox_flag(value: Option<&str>) -> bool {
    value == Some("1")
}

/// True when `VELOCITY_SANDBOX=1` demands sandbox semantics: treat this
/// PID 1 as nested even if the namespace probes missed the nesting.
pub fn sandbox_requested() -> bool {
    sandbox_flag(std::env::var(SANDBOX_ENV).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_mapping_is_recognized() {
        assert_eq!(first_uid_mapping("0 0 4294967295\n"), Some(IDENTITY_MAPPING));
        assert!(!mapped_from(Some("0 0 4294967295\n")));
        // /proc pads the fields with spaces.
        assert!(!mapped_from(Some("         0          0 4294967295\n")));
    }

    #[test]
    fn test_mapped_namespace_needs_more_than_the_root_uid() {
        // unshare --user --map-root-user: uid 0 in, 1000 out.
        assert!(mapped_from(Some("         0       1000          1\n")));
        // Root-created namespace mapping only uid 0 to itself.
        assert!(mapped_from(Some("0 0 1\n")));
    }

    #[test]
    fn test_full_identity_namespace_is_the_documented_gap() {
        // Root-created userns with the identity mapping: deliberately
        // reported as "not mapped" — see the module docs.
        assert!(!mapped_from(Some("0 0 4294967295\n")));
    }

    #[test]
    fn test_malformed_uid_map_defaults_to_not_mapped() {
        for bad in [
            None,
            Some(""),
            Some("garbage\n"),
            Some("0\n"),
            Some("0 1000\n"),
            Some("0 1000 1 2\n"),
            Some("-1 0 1\n"),
            Some("0x0 0 1\n"),
        ] {
            assert!(
                !mapped_from(bad),
                "malformed uid_map {bad:?} must not claim a mapping"
            );
        }
    }

    #[test]
    fn test_sandbox_flag_requires_exactly_one() {
        assert!(sandbox_flag(Some("1")));
        for other in [None, Some(""), Some("0"), Some("true"), Some(" 1")] {
            assert!(!sandbox_flag(other), "sandbox flag {other:?} must not arm");
        }
    }
}
