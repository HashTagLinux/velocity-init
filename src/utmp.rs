use std::io;

use log::{debug, warn};
use nix::unistd::getpid;

const UT_RUN_LVL: i16 = 1;

/// `BOOT_TIME` in `utmpx.h`: the "system booted" record.
const UT_BOOT_TIME: i16 = 2;

const UT_ID_SYSV: &[u8] = b"~~";

const PREV_NONE: u8 = b'N';

pub const DEFAULT_UTMP_PATH: &str = "/var/run/utmp";

/// The width of the record's tail fields (`ut_session`, `ut_tv`).
///
/// glibc's utmp record comes in two shapes and the file must match the
/// shape the *system's* tools use: the 32-bit-compat one (`int32` tail,
/// 384-byte records) on x86 and x86_64, and the time64 one (64-bit tail,
/// 400-byte records) on arm64 and armhf — armhf since its time64
/// transition (`_TIME_BITS=64`, as current distro builds ship it).
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
type UtmpTail = i32;

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
type UtmpTail = i64;

#[cfg(not(any(
    target_arch = "x86",
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "arm"
)))]
compile_error!(
    "no utmp record layout is pinned for this target; check glibc's \
     bits/utmp.h, then add the arch to the UtmpTail cfgs in src/utmp.rs"
);

/// One record of the on-disk Linux utmp database (`utmpx` flavour).
///
/// velocity-init writes `/var/run/utmp` for the *system's* tools
/// (`runlevel`, `who -r`), so the bytes must match what those tools use —
/// glibc's layout — whatever libc this binary was built against.
/// `libc::utmpx` cannot promise that portably: glibc itself has two
/// record shapes (384 bytes with the compat tail on x86/x86_64, 400 with
/// the time64 tail on arm64 and friends), and musl models only the time64
/// one — 400 bytes on x86_64, where Devuan's tools read 384. So the
/// record is spelled out here for the arches Devuan ships, taken from
/// glibc's `bits/utmp.h`, and pinned by the size/offset tests below; a
/// build whose layout drifts fails loudly instead of writing a database
/// nothing can read.
#[allow(dead_code)] // pins the byte layout; some fields are never read
#[repr(C)]
#[derive(Clone, Copy)]
struct UtmpRecord {
    ut_type: i16,
    /// Padding that keeps `ut_pid` 4-aligned (part of both layouts).
    _ut_pad: i16,
    ut_pid: i32,
    ut_line: [u8; 32],
    ut_id: [u8; 4],
    ut_user: [u8; 32],
    ut_host: [u8; 256],
    /// `struct exit_status` (never inspected).
    _ut_exit: [i16; 2],
    ut_session: UtmpTail,
    ut_tv_sec: UtmpTail,
    _ut_tv_usec: UtmpTail,
    _ut_addr_v6: [u8; 16],
    _ut_reserved: [u8; 20],
}

/// The record is the file format: it must be exactly one of glibc's
/// two sizes, or every read/write is off the rails.
const _: () = assert!(
    std::mem::size_of::<UtmpRecord>() == 384 || std::mem::size_of::<UtmpRecord>() == 400
);

impl UtmpRecord {
    /// The record as it appears in the file.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `UtmpRecord` is `repr(C)` plain-old-data with no
        // references; every byte is part of the on-disk format.
        unsafe {
            std::slice::from_raw_parts(
                (self as *const UtmpRecord) as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }

    /// Decode one record from a buffer holding at least its size.
    fn from_bytes(bytes: &[u8]) -> Self {
        // SAFETY: plain-old-data, and callers only pass per-record
        // chunks (`size_of::<Self>()` bytes; see `read_runlevel` and
        // `find_record_offset`), so the unaligned read stays in bounds.
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const UtmpRecord) }
    }
}

pub fn utmp_path() -> String {
    utmp_path_with(|key| std::env::var(key).ok())
}

pub fn utmp_path_with(lookup: impl Fn(&str) -> Option<String>) -> String {
    lookup("VELOCITY_UTMP")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_UTMP_PATH.to_string())
}

pub fn writes_allowed() -> bool {
    writes_allowed_with(
        |key| std::env::var(key).ok(),
        getpid().as_raw() as u32,
    )
}

pub fn writes_allowed_with(
    lookup: impl Fn(&str) -> Option<String>,
    pid: u32,
) -> bool {
    pid == 1
        || lookup("VELOCITY_UTMP")
            .map(|p| !p.is_empty())
            .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLevel {

    pub previous: char,

    pub current: char,
}

impl Default for RunLevel {
    fn default() -> Self {
        RunLevel {
            previous: PREV_NONE as char,
            current: '?',
        }
    }
}

pub fn read_runlevel(path: &str) -> Option<RunLevel> {
    let data = std::fs::read(path).ok()?;
    let entry_size = std::mem::size_of::<UtmpRecord>();
    let mut last: Option<RunLevel> = None;
    let mut i = 0;
    while i + entry_size <= data.len() {
        let chunk = &data[i..i + entry_size];
        let u = UtmpRecord::from_bytes(chunk);
        i += entry_size;
        if u.ut_type == UT_RUN_LVL {
            if let Some(rl) = decode_runlevel(&u) {
                last = Some(rl);
            }
        }
    }
    last
}

fn decode_runlevel(u: &UtmpRecord) -> Option<RunLevel> {

    let is_sysv = u.ut_user[0] == b'r' && u.ut_user[1] == b'u' && u.ut_user[2] == b'n';
    if is_sysv {
        let pid = u.ut_pid;
        if pid <= 0 {
            return None;
        }
        let prev = (pid / 256) as u8;
        let curr = (pid % 256) as u8;
        if curr == 0 {
            return None;
        }
        return Some(RunLevel {
            previous: if prev == 0 { 'N' } else { prev as char },
            current: curr as char,
        });
    }

    let prev = u.ut_user[0];
    let curr = u.ut_host[0];
    if curr == 0 {
        return None;
    }
    Some(RunLevel {
        previous: if prev == 0 { 'N' } else { prev as char },
        current: curr as char,
    })
}

pub fn write_runlevel_record(previous: Option<u8>, next: u8) {
    if !writes_allowed() {
        debug!(
            "not writing RUN_LVL record to {} (pid {} is not PID 1; set VELOCITY_UTMP to use a \
             private database)",
            utmp_path(),
            getpid()
        );
        return;
    }
    if let Err(e) = write_record(previous, next) {
        warn!("could not write runlevel record to utmp: {}", e,);
    }
    write_runlevel_file_if_init(next);
}

fn write_runlevel_file_if_init(next: u8) {
    let lookup = |key: &str| std::env::var(key).ok();
    if !runlevel_write_allowed_with(&lookup, getpid().as_raw() as u32) {
        debug!(
            "not writing {} (pid {} is not PID 1; set VELOCITY_RUNLEVEL_FILE to override)",
            runlevel_path(),
            getpid()
        );
        return;
    }
    if let Err(e) = write_runlevel_file(next) {
        warn!("could not write {}: {}", runlevel_path(), e);
    }
}

pub fn runlevel_write_allowed_with(
    lookup: impl Fn(&str) -> Option<String>,
    pid: u32,
) -> bool {
    let pinned = lookup("VELOCITY_RUNLEVEL_FILE")
        .map(|p| !p.is_empty())
        .unwrap_or(false);
    pid == 1 || pinned
}

pub fn write_runlevel_file(next: u8) -> io::Result<()> {
    let path = runlevel_path();
    write_runlevel_file_to(std::path::Path::new(&path), next)
}

pub fn write_runlevel_file_to(path: &std::path::Path, next: u8) -> io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(&[next])?;
    Ok(())
}

pub fn read_runlevel_file(path: &str) -> Option<char> {
    let data = std::fs::read(path).ok()?;
    data.first().map(|b| *b as char)
}

pub fn runlevel_path() -> String {
    runlevel_path_with(|key| std::env::var(key).ok())
}

pub fn runlevel_path_with(lookup: impl Fn(&str) -> Option<String>) -> String {
    lookup("VELOCITY_RUNLEVEL_FILE")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_RUNLEVEL_PATH.to_string())
}

pub const DEFAULT_RUNLEVEL_PATH: &str = "/var/run/runlevel";

pub fn write_boot_time() {
    if !writes_allowed() {
        return;
    }
    if let Err(e) = write_boot_time_record() {
        warn!("could not write BOOT_TIME record to utmp: {}", e);
    }
}

fn write_boot_time_record() -> io::Result<()> {
    let path = utmp_path();
    write_boot_time_record_to(std::path::Path::new(&path))
}

pub fn write_boot_time_record_to(utmp: &std::path::Path) -> io::Result<()> {
    if let Some(parent) = utmp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut u: UtmpRecord = unsafe { std::mem::zeroed() };
    u.ut_type = UT_BOOT_TIME;
    u.ut_pid = 0;

    for (i, b) in b"~~".iter().enumerate() {
        u.ut_id[i] = *b;
    }
    u.ut_line[0] = b'~';
    for (i, b) in b"reboot".iter().enumerate() {
        u.ut_user[i] = *b;
    }
    fill_ut_host(&mut u);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // The compat layout carries a 32-bit `tv_sec` (the y2038 wrap is
    // part of that on-disk format); the time64 layout carries 64.
    u.ut_tv_sec = now as UtmpTail;
    put_or_append(utmp, u, UT_BOOT_TIME, UT_ID_SYSV)
}

fn fill_ut_host(u: &mut UtmpRecord) {

    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };

    if unsafe { libc::uname(&mut uts) } != 0 {
        return;
    }

    let release = unsafe {
        std::ffi::CStr::from_ptr(uts.release.as_ptr())
    };
    for (i, b) in release.to_bytes().iter().enumerate().take(u.ut_host.len()) {
        u.ut_host[i] = *b;
    }
}

fn write_record(previous: Option<u8>, next: u8) -> io::Result<()> {
    let path = utmp_path();
    write_record_to(std::path::Path::new(&path), previous, next)
}

pub fn write_record_to(
    utmp: &std::path::Path,
    previous: Option<u8>,
    next: u8,
) -> io::Result<()> {

    if let Some(parent) = utmp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let prev = previous.unwrap_or(PREV_NONE);
    let mut u: UtmpRecord = unsafe { std::mem::zeroed() };
    u.ut_type = UT_RUN_LVL;
    u.ut_pid = next as i32 + 256 * prev as i32;

    for (i, b) in UT_ID_SYSV.iter().enumerate() {
        u.ut_id[i] = *b;
    }
    u.ut_line[0] = b'~';
    for (i, b) in b"runlevel".iter().enumerate() {
        u.ut_user[i] = *b;
    }
    fill_ut_host(&mut u);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    u.ut_tv_sec = now as UtmpTail;

    put_or_append(utmp, u, UT_RUN_LVL, UT_ID_SYSV)
}

fn put_or_append(
    path: &std::path::Path,
    record: UtmpRecord,
    ut_type: i16,
    id: &[u8],
) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    let entry_size = std::mem::size_of::<UtmpRecord>() as u64;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;

    let offset = find_record_offset(&mut f, ut_type, id, entry_size)?;

    let bytes = record.as_bytes();
    match offset {
        Some(off) => f.seek(SeekFrom::Start(off))?,

        None => f.seek(SeekFrom::End(0))?,
    };
    f.write_all(bytes)?;

    let _ = unsafe { libc::fsync(f.as_raw_fd()) };
    Ok(())
}

fn find_record_offset(
    f: &mut std::fs::File,
    ut_type: i16,
    id: &[u8],
    entry_size: u64,
) -> io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut buf = vec![0u8; entry_size as usize];
    let mut off = 0u64;
    f.seek(SeekFrom::Start(0))?;
    loop {
        match f.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }

        let u = UtmpRecord::from_bytes(&buf);
        if u.ut_type == ut_type && id_matches(&u.ut_id, id) {
            return Ok(Some(off));
        }
        off += entry_size;
    }
}

fn id_matches(field: &[u8], id: &[u8]) -> bool {
    field
        .iter()
        .zip(id)
        .all(|(c, b)| c == b)
        && field.get(id.len()).is_some_and(|c| *c == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a synthetic record to a unique per-test path and return it.
    ///
    /// The path is unique (test name + pid), so a stale file from an
    /// earlier run — possibly owned by another user — can never make the
    /// test fail with `PermissionDenied`, and parallel test binaries
    /// cannot collide. Callers remove the file when done.
    fn write_fake_record(u: &UtmpRecord, name: &str) -> std::path::PathBuf {
        let path = scratch(name);
        std::fs::write(&path, u.as_bytes()).unwrap();
        path
    }

    fn fake_record(prev: u8, cur: u8, name: &str) -> std::path::PathBuf {
        let mut u: UtmpRecord = unsafe { std::mem::zeroed() };
        u.ut_type = UT_RUN_LVL;
        u.ut_pid = cur as i32 + 256 * prev as i32;
        u.ut_id[0] = b'~';
        u.ut_id[1] = b'~';
        u.ut_line[0] = b'~';
        for (i, b) in b"runlevel".iter().enumerate() {
            u.ut_user[i] = *b;
        }
        write_fake_record(&u, name)
    }

    fn fake_legacy_record(prev: u8, cur: u8, name: &str) -> std::path::PathBuf {
        let mut u: UtmpRecord = unsafe { std::mem::zeroed() };
        u.ut_type = UT_RUN_LVL;
        u.ut_pid = 1;
        u.ut_id[0] = b'r';
        u.ut_id[1] = b'l';
        u.ut_user[0] = prev;
        u.ut_user[1] = 0;
        u.ut_host[0] = cur;
        u.ut_host[1] = 0;
        write_fake_record(&u, name)
    }

    #[test]
    fn read_runlevel_finds_last_record() {
        let p1 = fake_record(b'N', b'2', "last-a");
        let rl = read_runlevel(p1.to_str().unwrap()).unwrap();
        assert_eq!(
            rl,
            RunLevel {
                previous: 'N',
                current: '2'
            }
        );

        let p2 = fake_record(b'2', b'3', "last-b");
        let rl2 = read_runlevel(p2.to_str().unwrap()).unwrap();
        assert_eq!(
            rl2,
            RunLevel {
                previous: '2',
                current: '3'
            }
        );

        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn read_runlevel_missing_file_is_none() {
        assert!(read_runlevel("/nonexistent/utmp").is_none());
    }

    use std::collections::HashMap;

    fn lookup_of(map: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        |key: &str| map.get(key).cloned()
    }

    fn with_vars(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "velocity-utmp-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn utmp_path_honours_the_override() {
        let map = with_vars(&[("VELOCITY_UTMP", "/tmp/somewhere-utmp")]);
        assert_eq!(utmp_path_with(lookup_of(&map)), "/tmp/somewhere-utmp");
        let empty: HashMap<String, String> = HashMap::new();
        assert_eq!(utmp_path_with(lookup_of(&empty)), DEFAULT_UTMP_PATH);
        let blank = with_vars(&[("VELOCITY_UTMP", "")]);
        assert_eq!(utmp_path_with(lookup_of(&blank)), DEFAULT_UTMP_PATH);
    }

    #[test]
    fn writes_are_refused_without_an_explicit_database() {

        let empty: HashMap<String, String> = HashMap::new();
        assert!(
            !writes_allowed_with(lookup_of(&empty), 4242),
            "a non-PID-1 run must not write the machine's utmp"
        );

        assert!(writes_allowed_with(lookup_of(&empty), 1));

        let map = with_vars(&[("VELOCITY_UTMP", "/tmp/somewhere-utmp")]);
        assert!(writes_allowed_with(lookup_of(&map), 4242));

        let blank = with_vars(&[("VELOCITY_UTMP", "")]);
        assert!(!writes_allowed_with(lookup_of(&blank), 4242));
    }

    #[test]
    fn runlevel_publish_gate_needs_pid1_or_an_override() {
        let empty: HashMap<String, String> = HashMap::new();
        assert!(!runlevel_write_allowed_with(lookup_of(&empty), 4242));
        assert!(runlevel_write_allowed_with(lookup_of(&empty), 1));
        let map = with_vars(&[("VELOCITY_RUNLEVEL_FILE", "/tmp/runlevel")]);
        assert!(runlevel_write_allowed_with(lookup_of(&map), 4242));
        assert_eq!(
            runlevel_path_with(lookup_of(&map)),
            "/tmp/runlevel"
        );
        assert_eq!(
            runlevel_path_with(lookup_of(&empty)),
            DEFAULT_RUNLEVEL_PATH
        );
    }

    #[test]
    fn a_record_written_to_a_pinned_database_reads_back() {

        let dir = scratch("rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("utmp");

        write_record_to(&path, Some(b'N'), b'3').unwrap();
        let rl = read_runlevel(path.to_str().unwrap()).expect("record written");
        assert_eq!(rl, RunLevel { previous: 'N', current: '3' });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_runlevel_record_replaces_the_first() {

        let dir = scratch("rep");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("utmp");
        let rlevel = dir.join("runlevel");

        write_record_to(&path, Some(b'N'), b'2').unwrap();
        write_record_to(&path, Some(b'2'), b'3').unwrap();
        write_record_to(&path, Some(b'3'), b'5').unwrap();

        let size = std::mem::size_of::<UtmpRecord>();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            len,
            size as u64,
            "three level changes must leave one RUN_LVL record, not three"
        );
        assert_eq!(
            read_runlevel(path.to_str().unwrap()),
            Some(RunLevel {
                previous: '3',
                current: '5'
            }),
            "a front-to-back reader (runlevel/who -r) sees the current level"
        );

        write_runlevel_file_to(&rlevel, b'5').unwrap();
        assert_eq!(std::fs::read(&rlevel).unwrap(), vec![b'5']);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_time_record_is_written() {
        let dir = scratch("boot");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("utmp");

        write_boot_time_record_to(&path).unwrap();
        write_boot_time_record_to(&path).unwrap();

        let size = std::mem::size_of::<UtmpRecord>();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size as u64);
        let data = std::fs::read(&path).unwrap();
        let u = UtmpRecord::from_bytes(&data);
        assert_eq!(u.ut_type, UT_BOOT_TIME);
        assert!(u.ut_tv_sec > 1_700_000_000, "a real timestamp");

        assert_eq!(&u.ut_id[..2], b"~~");
        assert_eq!(u.ut_line[0], b'~');
        assert_eq!(&u.ut_user[..6], b"reboot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runlevel_record_uses_the_sysvinit_pid_encoding() {

        let dir = scratch("pidenc");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("utmp");
        write_record_to(&path, Some(b'N'), b'2').unwrap();
        let data = std::fs::read(&path).unwrap();
        let u = UtmpRecord::from_bytes(&data);
        assert_eq!(u.ut_pid, b'2' as i32 + 256 * b'N' as i32);
        assert_eq!(&u.ut_id[..2], b"~~");
        assert_eq!(u.ut_line[0], b'~');
        assert_eq!(&u.ut_user[..8], b"runlevel");
        assert!(u.ut_tv_sec > 1_700_000_000, "RUN_LVL carries a timestamp");

        assert_eq!(
            read_runlevel(path.to_str().unwrap()),
            Some(RunLevel { previous: 'N', current: '2' })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_user_host_records_still_decode() {

        let p = fake_legacy_record(b'2', b'3', "legacy-compat");
        assert_eq!(
            read_runlevel(p.to_str().unwrap()),
            Some(RunLevel { previous: '2', current: '3' })
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zero_levels_decode_sensibly() {

        let p = fake_record(0, b'2', "zero-prev");
        assert_eq!(
            read_runlevel(p.to_str().unwrap()),
            Some(RunLevel { previous: 'N', current: '2' })
        );
        assert_eq!(RunLevel::default().previous, 'N');
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn runlevel_file_round_trips_a_single_byte() {
        let dir = scratch("rlfile");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runlevel");
        write_runlevel_file_to(&path, b'3').unwrap();
        assert_eq!(read_runlevel_file(path.to_str().unwrap()), Some('3'));
        assert!(read_runlevel_file("/nonexistent/runlevel-velocity-test").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_record_is_written_to_an_unpinned_database() {

        let empty: HashMap<String, String> = HashMap::new();
        assert!(!writes_allowed_with(lookup_of(&empty), 4242));
        assert!(!runlevel_write_allowed_with(lookup_of(&empty), 4242));
    }

    #[test]
    fn record_layout_matches_the_glibc_utmp_file_format() {
        use std::mem::{offset_of, size_of};

        // Everything up to `ut_session` is shared by glibc's two layouts.
        assert_eq!(offset_of!(UtmpRecord, ut_type), 0);
        assert_eq!(offset_of!(UtmpRecord, ut_pid), 4);
        assert_eq!(offset_of!(UtmpRecord, ut_line), 8);
        assert_eq!(offset_of!(UtmpRecord, ut_id), 40);
        assert_eq!(offset_of!(UtmpRecord, ut_user), 44);
        assert_eq!(offset_of!(UtmpRecord, ut_host), 76);
        assert_eq!(offset_of!(UtmpRecord, _ut_exit), 332);

        // The tail is where the layouts diverge — same arch split as the
        // `UtmpTail` alias.
        const TIME64: bool = cfg!(any(target_arch = "aarch64", target_arch = "arm"));
        if TIME64 {
            assert_eq!(size_of::<UtmpRecord>(), 400, "time64 layout");
            assert_eq!(offset_of!(UtmpRecord, ut_tv_sec), 344);
            assert_eq!(offset_of!(UtmpRecord, _ut_addr_v6), 360);
        } else {
            assert_eq!(size_of::<UtmpRecord>(), 384, "32-bit-compat layout");
            assert_eq!(offset_of!(UtmpRecord, ut_tv_sec), 340);
            assert_eq!(offset_of!(UtmpRecord, _ut_addr_v6), 348);
        }
    }
}
