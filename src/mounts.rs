use std::io;
use std::path::Path;

use log::{debug, info, warn};
use nix::errno::Errno;
use nix::mount::{mount, MsFlags};

pub const DEFAULT_FSTAB: &str = "/etc/fstab";

fn pseudo_flags() -> MsFlags {
    MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {

    pub source: String,

    pub target: String,

    pub fstype: String,

    pub options: String,
}

pub fn parse_fstab(path: &str) -> io::Result<Vec<MountEntry>> {
    let text = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            warn!("ignoring malformed fstab line: {}", line);
            continue;
        }
        entries.push(MountEntry {
            source: decode_escapes(fields[0]),
            target: decode_escapes(fields[1]),
            fstype: fields[2].to_string(),
            options: fields[3].to_string(),
        });
    }
    Ok(entries)
}

pub fn has_option(options: &str, needle: &str) -> bool {
    options.split(',').any(|o| o.trim() == needle)
}

pub fn fstab_flags(options: &str) -> MsFlags {
    let mut flags = MsFlags::empty();
    for opt in options.split(',') {
        match opt.trim() {
            "ro" => flags |= MsFlags::MS_RDONLY,
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "noatime" => flags |= MsFlags::MS_NOATIME,
            "relatime" => flags |= MsFlags::MS_RELATIME,
            "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
            "strictatime" => flags |= MsFlags::MS_STRICTATIME,
            "lazytime" => flags |= MsFlags::MS_LAZYTIME,
            "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
            "dirsync" => flags |= MsFlags::MS_DIRSYNC,
            "silent" => flags |= MsFlags::MS_SILENT,
            "mand" => flags |= MsFlags::MS_MANDLOCK,
            _ => {}
        }
    }
    flags
}

pub fn mount_data(options: &str) -> String {
    options
        .split(',')
        .map(|o| o.trim())
        .filter(|o| !o.is_empty())
        .filter(|o| !is_mount_marker_or_flag(o))
        .collect::<Vec<_>>()
        .join(",")
}

fn is_mount_marker_or_flag(opt: &str) -> bool {
    if opt.starts_with("x-") || opt.starts_with("comment=") {
        return true;
    }

    if opt == "x-mount.mkdir" {
        return true;
    }
    matches!(
        opt,
        "defaults"
            | "ro"
            | "rw"
            | "nosuid"
            | "nodev"
            | "noexec"
            | "noatime"
            | "relatime"
            | "nodiratime"
            | "strictatime"
            | "lazytime"
            | "sync"
            | "dirsync"
            | "silent"
            | "mand"
            | "noauto"
            | "nofail"
            | "_netdev"
            | "auto"
            | "bind"
            | "rbind"
    )
}

pub fn mount_data_dir(options: &str) -> Option<String> {
    options.split(',').find_map(|o| {
        let o = o.trim();
        o.strip_prefix("x-mount.mkdir=")
            .map(|d| d.trim().trim_matches('"').trim().to_string())
            .filter(|d| !d.is_empty())
    })
}

pub fn is_network_fs(fstype: &str, options: &str) -> bool {
    if has_option(options, "_netdev") {
        return true;
    }
    let fstype = fstype.to_ascii_lowercase();

    matches!(
        fstype.as_str(),
        "nfs"
            | "nfs4"
            | "cifs"
            | "smbfs"
            | "smb3"
            | "ftpfs"
            | "sshfs"
            | "ncpfs"
            | "coda"
            | "9p"
            | "glusterfs"
            | "fuse.glusterfs"
            | "fuse.sshfs"
            | "fuse.cifs"
    )
}

pub fn is_mounted(target: &str) -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    is_mounted_in(&text, target)
}

fn is_mounted_in(mountinfo: &str, target: &str) -> bool {
    mountinfo.lines().any(|l| {

        let mp = l.split_whitespace().nth(4);
        mp.map(decode_escapes).as_deref() == Some(target)
    })
}

fn ensure_mounted(
    source: &str,
    target: &str,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
    label: &str,
) -> io::Result<()> {
    if is_mounted(target) {
        info!("{} already mounted at {} — skipping", label, target);
        return Ok(());
    }
    if let Err(e) = std::fs::create_dir_all(target) {
        warn!("cannot create mountpoint {}: {}", target, e);
    }
    match mount(Some(source), target, Some(fstype), flags, data) {
        Ok(()) => {
            info!("mounted {} at {}", label, target);
            Ok(())
        }
        Err(Errno::EBUSY) => {
            debug!("{} at {} already mounted (EBUSY)", label, target);
            Ok(())
        }
        Err(e) => {
            warn!("cannot mount {} at {}: {}", label, target, e);
            Err(io::Error::from_raw_os_error(e as i32))
        }
    }
}

pub fn setup_base_filesystems() -> io::Result<()> {
    ensure_mounted(
        "proc",
        "/proc",
        "proc",
        pseudo_flags(),
        None::<&str>,
        "proc",
    )?;
    ensure_mounted(
        "sysfs",
        "/sys",
        "sysfs",
        pseudo_flags(),
        None::<&str>,
        "sysfs",
    )?;

    let _ = ensure_mounted(
        "devtmpfs",
        "/dev",
        "devtmpfs",
        MsFlags::MS_NOSUID,
        None,
        "dev",
    );

    let _ = ensure_mounted(
        "tmpfs",
        "/run",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=0755"),
        "run",
    );
    setup_cgroup2();
    info!("base filesystems ready: /proc /sys /dev /run");
    Ok(())
}

fn mount_depth(target: &str) -> usize {
    target.split('/').filter(|c| !c.is_empty()).count()
}

fn resolve_swap_source(source: &str) -> Option<String> {
    if source.starts_with('/') {
        return Some(source.to_string());
    }
    if let Some(id) = source.strip_prefix("UUID=") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(format!("/dev/disk/by-uuid/{id}"));
        }
        return None;
    }
    if let Some(label) = source.strip_prefix("LABEL=") {
        let label = label.trim();
        if !label.is_empty() {
            return Some(format!("/dev/disk/by-label/{label}"));
        }
        return None;
    }
    None
}

fn activate_swap(entries: &[MountEntry]) {
    for e in entries {
        if !e.fstype.eq_ignore_ascii_case("swap") || has_option(&e.options, "noauto") {
            continue;
        }
        let Some(path) = resolve_swap_source(&e.source) else {
            debug!("swap entry {} is not a usable source — skipping", e.source);
            continue;
        };

        let c = match std::ffi::CString::new(path.as_str()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rc = unsafe { libc::swapon(c.as_ptr(), 0) };
        if rc == 0 {
            info!("swap on {}", path);
        } else {
            warn!("swapon {} failed: {}", path, Errno::last());
        }
    }
}

fn order_local_entries(entries: &[MountEntry]) -> Vec<&MountEntry> {
    let mut local: Vec<&MountEntry> = entries.iter().collect();
    local.retain(|e| !e.fstype.eq_ignore_ascii_case("swap"));
    local.sort_by_key(|e| mount_depth(&e.target));
    local
}

pub fn mount_fstab_entries(entries: &[MountEntry]) {
    activate_swap(entries);

    for e in order_local_entries(entries) {
        if has_option(&e.options, "noauto") {
            debug!("{} has noauto — skipping", e.target);
            continue;
        }

        if is_network_fs(&e.fstype, &e.options) {
            info!(
                "skipping network filesystem {} on {} ({} / _netdev)",
                e.source, e.target, e.fstype
            );
            continue;
        }
        if is_mounted(&e.target) {
            debug!("{} already mounted — skipping", e.target);
            continue;
        }

        let is_bind = has_option(&e.options, "bind") || has_option(&e.options, "rbind");
        if !is_bind {
            let want = mount_data_dir(&e.options).unwrap_or_else(|| e.target.clone());
            if !Path::new(&want).is_dir() {
                match std::fs::create_dir_all(&want) {
                    Ok(()) => debug!("created mountpoint {}", want),
                    Err(err) => warn!(
                        "could not create mountpoint {} for {}: {} — skipping entry",
                        want, e.source, err
                    ),
                }
            }
        }

        let is_rbind = has_option(&e.options, "rbind");
        let extra_flags = fstab_flags(&e.options);
        let flags = if is_bind {
            let mut f = MsFlags::MS_BIND;
            if is_rbind {
                f |= MsFlags::MS_REC;
            }
            f
        } else {
            extra_flags
        };

        let data_str = mount_data(&e.options);
        let data: Option<&str> = if is_bind || data_str.is_empty() {
            None
        } else {
            Some(&data_str)
        };
        let fstype = if is_bind {
            None
        } else {
            Some(e.fstype.as_str())
        };
        match mount(
            Some(e.source.as_str()),
            e.target.as_str(),
            fstype,
            flags,
            data,
        ) {
            Ok(()) => {
                info!("mounted {} on {} ({})", e.source, e.target, e.fstype);
                if is_bind && !extra_flags.is_empty() {

                    let mut remount = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | extra_flags;
                    if is_rbind {
                        remount |= MsFlags::MS_REC;
                    }
                    if let Err(err) = mount(
                        None::<&str>,
                        e.target.as_str(),
                        None::<&str>,
                        remount,
                        None::<&str>,
                    ) {
                        warn!(
                            "bind mount {} on {} succeeded but remount with {:?} failed: {}",
                            e.source, e.target, extra_flags, err
                        );
                    }
                }
            }
            Err(Errno::EBUSY) => debug!("{} already mounted (EBUSY)", e.target),
            Err(err) => {
                if has_option(&e.options, "nofail") {
                    warn!(
                        "could not mount {} on {}: {} (nofail)",
                        e.source, e.target, err
                    );
                } else {
                    warn!(
                        "could not mount {} on {}: {} — continuing boot",
                        e.source, e.target, err
                    );
                }
            }
        }
    }
}

fn setup_cgroup2() {
    let cg = std::path::PathBuf::from("/sys/fs/cgroup");
    if cg.join("cgroup.controllers").exists() {
        debug!("cgroup v2 already mounted");
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&cg) {
        warn!("cannot create {}: {}", cg.display(), e);
    }

    match mount(
        Some("cgroup2"),
        cg.as_path(),
        Some("cgroup2"),
        pseudo_flags(),
        None::<&str>,
    ) {
        Ok(()) => info!("mounted cgroup2 at {}", cg.display()),
        Err(e) => warn!("cannot mount cgroup2 at {}: {}", cg.display(), e),
    }
}

fn decode_escapes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 4 <= chars.len() {
            let digits: String = chars[i + 1..i + 4].iter().collect();
            if digits.bytes().all(|b| matches!(b, b'0'..=b'7')) {
                if let Ok(code) = u32::from_str_radix(&digits, 8) {
                    if let Some(c) = char::from_u32(code) {
                        out.push(c);
                        i += 4;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn parse_mount_targets(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.get(4).map(|t| decode_escapes(t))
        })
        .collect()
}

fn unmount_order(targets: Vec<String>) -> Vec<String> {
    fn is_critical(t: &str) -> bool {
        t == "/"
            || t == "/proc"
            || t.starts_with("/proc/")
            || t == "/sys"
            || t.starts_with("/sys/")
            || t == "/dev"
            || t.starts_with("/dev/")
    }
    fn sort_key(t: &str) -> (std::cmp::Reverse<usize>, std::cmp::Reverse<usize>) {
        let depth = t.split('/').filter(|c| !c.is_empty()).count();
        (std::cmp::Reverse(depth), std::cmp::Reverse(t.len()))
    }
    let (mut normal, mut critical): (Vec<String>, Vec<String>) =
        targets.into_iter().partition(|t| !is_critical(t));

    normal.sort_by_key(|t| sort_key(t));
    critical.sort_by_key(|t| sort_key(t));
    normal.extend(critical);
    normal
}

pub fn teardown_filesystems() {

    if let Err(e) = nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        nix::mount::MsFlags::MS_REC | nix::mount::MsFlags::MS_PRIVATE,
        None::<&str>,
    ) {
        warn!("could not make / private before teardown: {}", e);
    }
    let mountinfo = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(t) => t,
        Err(e) => {
            warn!("cannot read /proc/self/mountinfo for teardown: {}", e);

            nix::unistd::sync();
            return;
        }
    };
    let targets = unmount_order(parse_mount_targets(&mountinfo));

    for target in &targets {
        if target == "/" {
            continue;
        }
        match nix::mount::umount2(target.as_str(), nix::mount::MntFlags::empty()) {
            Ok(()) => info!("unmounted {}", target),
            Err(Errno::EBUSY) => {

                warn!("{} busy — detaching lazily", target);
                let _ = nix::mount::umount2(target.as_str(), nix::mount::MntFlags::MNT_DETACH);
            }
            Err(e) => warn!("cannot unmount {}: {}", target, e),
        }
    }

    info!("remounting / read-only");
    match mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        Ok(()) => info!("root filesystem remounted read-only"),
        Err(e) => warn!("cannot remount / read-only: {}", e),
    }

    info!("syncing filesystems");
    nix::unistd::sync();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fstab_fixture() -> std::path::PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data_fstab/fstab")
    }

    #[test]
    fn test_parse_fstab_fixture() {
        let entries = parse_fstab(fstab_fixture().to_str().unwrap()).expect("fixture parses");
        assert_eq!(entries.len(), 3, "comment/blank lines are skipped");

        let root = entries
            .iter()
            .find(|e| e.target == "/")
            .expect("root entry");
        assert_eq!(root.fstype, "ext4");
        assert_eq!(root.source, "UUID=8f4a1c2e-…");

        let home = entries.iter().find(|e| e.target == "/home").expect("home");
        assert_eq!(home.options, "defaults,noatime,nofail");
        assert!(has_option(&home.options, "nofail"));
        assert!(has_option(&home.options, "noatime"));

        let swap = entries.iter().find(|e| e.target == "swap").expect("swap");
        assert_eq!(swap.fstype, "swap");
    }

    #[test]
    fn test_parse_fstab_missing_file_errors() {
        assert!(parse_fstab("/nonexistent/fstab").is_err());
    }

    #[test]
    fn test_fstab_flags() {
        assert!(fstab_flags("defaults").is_empty());
        let f = fstab_flags("ro,nosuid,nodev,noexec,noatime,relatime,nodiratime");
        assert!(f.contains(MsFlags::MS_RDONLY));
        assert!(f.contains(MsFlags::MS_NOSUID));
        assert!(f.contains(MsFlags::MS_NODEV));
        assert!(f.contains(MsFlags::MS_NOEXEC));
        assert!(f.contains(MsFlags::MS_NOATIME));
        assert!(f.contains(MsFlags::MS_RELATIME));
        assert!(f.contains(MsFlags::MS_NODIRATIME));
        assert!(!f.contains(MsFlags::MS_BIND));

        assert!(mount_data("defaults,noatime,relatime,nodiratime").is_empty());

        let g = fstab_flags("sync,dirsync,silent,strictatime,lazytime,mand");
        assert!(g.contains(MsFlags::MS_SYNCHRONOUS));
        assert!(g.contains(MsFlags::MS_DIRSYNC));
        assert!(g.contains(MsFlags::MS_SILENT));
        assert!(g.contains(MsFlags::MS_STRICTATIME));
        assert!(g.contains(MsFlags::MS_LAZYTIME));
        assert!(g.contains(MsFlags::MS_MANDLOCK));
        assert!(mount_data("sync,dirsync,silent,strictatime,lazytime,mand").is_empty());

        assert!(has_option("defaults, ro", "ro"));
        assert!(fstab_flags("ro, nosuid").contains(MsFlags::MS_NOSUID));
    }

    #[test]
    fn test_is_mounted_on_host() {

        assert!(is_mounted("/proc"), "/proc should be mounted");
        assert!(is_mounted("/sys"), "/sys should be mounted");
        assert!(
            !is_mounted("/definitely-not-a-mountpoint"),
            "no false positive"
        );
    }

    #[test]
    fn test_skip_swap_and_noauto() {

        let entries = vec![
            MountEntry {
                source: "/dev/sdb1".into(),
                target: "/mnt/data".into(),
                fstype: "ext4".into(),
                options: "defaults,noauto".into(),
            },
            MountEntry {
                source: "/dev/sdb2".into(),
                target: "swap".into(),
                fstype: "swap".into(),
                options: "defaults".into(),
            },

            MountEntry {
                source: "whatever".into(),
                target: "/proc".into(),
                fstype: "proc".into(),
                options: "defaults".into(),
            },
        ];
        mount_fstab_entries(&entries);
    }

    #[test]
    fn test_is_network_fs() {
        assert!(is_network_fs("nfs", ""));
        assert!(is_network_fs("nfs4", ""));
        assert!(is_network_fs("cifs", ""));
        assert!(is_network_fs("smbfs", ""));
        assert!(is_network_fs("sshfs", ""));
        assert!(is_network_fs("fuse.glusterfs", ""));

        assert!(is_network_fs("ext4", "_netdev"));
        assert!(is_network_fs("xfs", "defaults,_netdev"));

        assert!(!is_network_fs("ext4", "defaults"));
        assert!(!is_network_fs("xfs", "defaults,noatime"));
        assert!(!is_network_fs("vfat", "defaults"));
    }

    #[test]
    fn test_mount_data_passes_driver_options() {

        assert_eq!(mount_data("defaults"), "");
        assert_eq!(mount_data("ro,noexec,defaults"), "");
        assert_eq!(mount_data("defaults,noatime"), "");
        assert_eq!(mount_data("defaults,subtype=foo"), "subtype=foo");
        assert_eq!(mount_data("noatime,defaults,discard,offset=1024"), "discard,offset=1024");

        assert_eq!(mount_data("_netdev,defaults,x-mount.mkdir"), "");
        assert_eq!(mount_data("defaults,x-mount.mkdir=/foo"), "");
        assert_eq!(mount_data("defaults,x-systemd.device-timeout=10"), "");
        assert_eq!(mount_data("defaults,comment=foo"), "");
        assert_eq!(mount_data("defaults,sync"), "");

        assert_eq!(
            mount_data("defaults,x-mount.mkdir=/foo,discard"),
            "discard"
        );
    }

    #[test]
    fn test_skip_network_and_noauto() {

        let entries = vec![
            MountEntry {
                source: "host:/srv".into(),
                target: "/nfs/srv".into(),
                fstype: "nfs4".into(),
                options: "defaults,_netdev,auto".into(),
            },
            MountEntry {
                source: "//srv/share".into(),
                target: "/mnt/share".into(),
                fstype: "cifs".into(),
                options: "defaults,guest".into(),
            },

            MountEntry {
                source: "whatever".into(),
                target: "/proc".into(),
                fstype: "proc".into(),
                options: "defaults".into(),
            },
        ];
        mount_fstab_entries(&entries);
    }

    #[test]
    fn test_mount_depth_and_local_ordering() {
        assert_eq!(mount_depth("/"), 0);
        assert_eq!(mount_depth("/var"), 1);
        assert_eq!(mount_depth("/var/log"), 2);

        let mk = |source: &str, target: &str, fstype: &str| MountEntry {
            source: source.into(),
            target: target.into(),
            fstype: fstype.into(),
            options: "defaults".into(),
        };
        let entries = vec![
            mk("d", "/var/log/remote", "ext4"),
            mk("a", "/var/log", "ext4"),
            mk("s", "swap", "swap"),
            mk("b", "/var", "ext4"),
            mk("n", "host:/x", "nfs4"),
        ];

        let ordered = order_local_entries(&entries);
        let targets: Vec<&str> = ordered.iter().map(|e| e.target.as_str()).collect();
        assert_eq!(
            targets,
            vec!["/var", "/var/log", "host:/x", "/var/log/remote"]
        );
    }

    #[test]
    fn test_activate_swap_skips_non_path_sources() {

        let mk = |source: &str, noauto: bool| MountEntry {
            source: source.into(),
            target: "swap".into(),
            fstype: "swap".into(),
            options: if noauto { "noauto".into() } else { "defaults".into() },
        };
        let entries = vec![mk("swap", false), mk("/dev/sdb2", true)];
        activate_swap(&entries);
    }

    #[test]
    fn test_parse_mount_targets_and_escapes() {
        let fixture = "\
26 19 0:24 / /run tmpfs rw,nosuid,nodev,noexec,relatime,size=... - tmpfs tmpfs rw
27 19 0:25 / /mnt/with\\040space ext4 rw,relatime - ext4 /dev/sdb1 rw
31 1 0:30 / /proc proc rw,nosuid,nodev,noexec,relatime - proc proc rw
32 1 0:31 / /sys sysfs rw,nosuid,nodev,noexec,relatime - sysfs sysfs rw
";
        let targets = parse_mount_targets(fixture);
        assert_eq!(
            targets,
            vec![
                "/run".to_string(),
                "/mnt/with space".to_string(),
                "/proc".to_string(),
                "/sys".to_string(),
            ]
        );
    }

    #[test]
    fn test_unmount_order_children_first_critical_last() {

        let order = unmount_order(vec![
            "/".to_string(),
            "/sys".to_string(),
            "/proc".to_string(),
            "/mnt/data/sub".to_string(),
            "/run".to_string(),
            "/mnt/data".to_string(),
            "/dev".to_string(),
            "/proc/sys/kernel".to_string(),
        ]);
        assert_eq!(
            order,
            vec![
                "/mnt/data/sub",
                "/mnt/data",
                "/run",
                "/proc/sys/kernel",
                "/proc",
                "/sys",
                "/dev",
                "/",
            ]
        );
    }

    #[test]
    fn test_unmount_order_prefix_boundary_and_depth() {

        let order = unmount_order(vec![
            "/proc".to_string(),
            "/procfoo".to_string(),
            "/sysfoo".to_string(),
            "/devices".to_string(),
        ]);

        assert_eq!(
            order,
            vec![
                "/procfoo".to_string(),
                "/devices".to_string(),
                "/sysfoo".to_string(),
                "/proc".to_string(),
            ]
        );

        let order = unmount_order(vec![
            "/mnt/a-very-long-name".to_string(),
            "/a/b/c".to_string(),
        ]);
        assert_eq!(
            order,
            vec!["/a/b/c".to_string(), "/mnt/a-very-long-name".to_string(),]
        );
    }

    #[test]
    fn test_is_mounted_in_decodes_escapes() {
        let fixture = "27 19 0:25 / /mnt/with\\040space ext4 rw - ext4 /dev/sdb1 rw\n";
        assert!(is_mounted_in(fixture, "/mnt/with space"));
        assert!(!is_mounted_in(fixture, "/mnt/with\\040space"));
        assert!(!is_mounted_in(fixture, "/mnt/other"));
    }

    #[test]
    fn test_decode_escapes_edge_cases() {
        assert_eq!(decode_escapes("/plain"), "/plain");
        assert_eq!(decode_escapes("/a\\040b"), "/a b");
        assert_eq!(decode_escapes("/a\\011b"), "/a\tb");

        assert_eq!(decode_escapes("/a\\999b"), "/a\\999b");
        assert_eq!(decode_escapes("/a\\04"), "/a\\04");
        assert_eq!(decode_escapes("/a\\"), "/a\\");
        assert_eq!(decode_escapes("x-mount.mkdir=\"/my dir\""), "x-mount.mkdir=\"/my dir\"");
    }

    #[test]
    fn test_mount_data_dir_quotes() {
        assert_eq!(
            mount_data_dir("defaults,x-mount.mkdir=\"/my dir\""),
            Some("/my dir".to_string())
        );
        assert_eq!(mount_data_dir("defaults"), None);
        assert_eq!(mount_data_dir("x-mount.mkdir="), None);
    }

    #[test]
    fn test_resolve_swap_source() {
        assert_eq!(
            resolve_swap_source("/dev/sda2"),
            Some("/dev/sda2".to_string())
        );
        assert_eq!(
            resolve_swap_source("UUID=8f4a1c2e-1234"),
            Some("/dev/disk/by-uuid/8f4a1c2e-1234".to_string())
        );
        assert_eq!(
            resolve_swap_source("LABEL=swapdisk"),
            Some("/dev/disk/by-label/swapdisk".to_string())
        );
        assert_eq!(resolve_swap_source("swap"), None);
        assert_eq!(resolve_swap_source("none"), None);
        assert_eq!(resolve_swap_source("UUID="), None);
    }
}
