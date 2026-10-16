use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{warn, Level, LevelFilter, Metadata, Record};

pub const DEFAULT_JOURNAL_PATH: &str = crate::paths::DEFAULT_JOURNAL_PATH;

pub const MAGIC: &[u8; 8] = b"VELOJRN1";

pub const MAX_JOURNAL_BYTES: u64 = 32 * 1024 * 1024;

pub const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;

const SEG_ACTIVE: &str = ".0";
const SEG_PREV: &str = ".1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {

    pub micros: i64,

    pub level: u8,

    pub source: String,

    pub message: String,
}

impl Entry {

    pub fn from_record(record: &Record) -> Entry {
        Entry {
            micros: unix_micros(),
            level: record.level() as u8,
            source: record.target().to_string(),
            message: record.args().to_string(),
        }
    }

    pub fn level(&self) -> Level {
        match self.level {
            1 => Level::Error,
            2 => Level::Warn,
            3 => Level::Info,
            4 => Level::Debug,
            5 => Level::Trace,
            _ => Level::Info,
        }
    }

    pub fn display(&self) -> String {
        format!(
            "[{} {:<5} {}] {}",
            format_time(self.micros),
            self.level().to_string(),
            self.source,
            self.message
        )
    }
}

pub struct Journal {
    path: PathBuf,
    file: Mutex<File>,
}

pub(crate) fn seg_paths(base: &Path) -> (PathBuf, PathBuf) {
    let mut active = base.as_os_str().to_owned();
    active.push(SEG_ACTIVE);
    let mut prev = base.as_os_str().to_owned();
    prev.push(SEG_PREV);
    (PathBuf::from(active), PathBuf::from(prev))
}

impl Journal {

    pub fn open_first_available(candidates: &[PathBuf]) -> io::Result<(Journal, PathBuf)> {
        let mut last: Option<io::Error> = None;
        for path in candidates {
            match Self::open(&path.to_string_lossy()) {
                Ok(j) => return Ok((j, path.clone())),
                Err(e) => {
                    warn!("journal unavailable at {}: {}", path.display(), e);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
    }

    pub fn open(path: &str) -> io::Result<Journal> {
        let p = PathBuf::from(path);

        if std::fs::symlink_metadata(&p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "journal path is a symlink",
            ));
        }
        if p.is_dir() {

            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "journal path is a directory",
            ));
        }
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let (seg0, _) = seg_paths(&p);
        if !seg0.exists() && p.is_file() {

            let _ = std::fs::rename(&p, &seg0);
        }
        let file = Self::open_segment(&seg0)?;
        Ok(Journal {
            path: p,
            file: Mutex::new(file),
        })
    }

    fn open_segment(seg: &Path) -> io::Result<File> {
        if let Some(parent) = seg.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(seg)?;
        let len = file.metadata()?.len();
        if len == 0 {
            file.write_all(MAGIC)?;
            file.flush()?;
        } else if !is_valid_header(seg)? {

            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(MAGIC)?;
            file.flush()?;
        } else {

            truncate_torn_tail(&mut file)?;
        }
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &Entry) -> io::Result<()> {

        let mut source = entry.source.as_bytes();
        if source.len() > u16::MAX as usize {
            source = &source[..u16::MAX as usize];
        }
        let mut message = entry.message.as_bytes();
        let max_payload = u32::MAX as usize - source.len();
        if message.len() > max_payload {
            message = &message[..max_payload];
        }
        let payload_len = source.len() + message.len();
        let mut buf = Vec::with_capacity(24 + payload_len);
        buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
        buf.extend_from_slice(&entry.micros.to_le_bytes());
        buf.push(entry.level);
        buf.extend_from_slice(&(source.len() as u16).to_le_bytes());
        buf.extend_from_slice(source);
        buf.extend_from_slice(message);

        let rotation_error = {
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());

            #[cfg(test)]
            let _reentrancy = lock_probe::Guard::enter();

            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            if size > SEGMENT_BYTES {
                roll_segment(&self.path, &mut f).err()
            } else {
                None
            }
        };
        if let Some(e) = rotation_error {

            warn!("journal rotation failed ({}); continuing", e);
        }
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        f.write_all(&buf)?;
        f.flush()
    }

    pub fn read(&self) -> io::Result<Vec<Entry>> {
        read_file(&self.path)
    }
}

fn is_valid_header(path: &Path) -> io::Result<bool> {
    let mut f = OpenOptions::new().read(true).open(path)?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    Ok(magic == *MAGIC)
}

fn last_complete_record(data: &[u8]) -> Option<usize> {
    if data.len() < MAGIC.len() || &data[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mut off = MAGIC.len();
    while off + 15 <= data.len() {
        let payload_len = u32::from_le_bytes(data[off..off + 4].try_into().ok()?) as usize;
        let src_len = u16::from_le_bytes(data[off + 13..off + 15].try_into().ok()?) as usize;
        let payload_start = off + 15;
        if payload_len > data.len().saturating_sub(payload_start) {
            break;
        }
        if src_len > payload_len {
            break;
        }
        off = payload_start + payload_len;
    }
    Some(off)
}

fn truncate_torn_tail(f: &mut File) -> io::Result<()> {
    use std::io::Read;
    let len = f.metadata()?.len();
    if len <= MAGIC.len() as u64 {
        return Ok(());
    }

    if len > SEGMENT_BYTES + 4096 {
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(MAGIC)?;
        return f.flush();
    }
    f.seek(SeekFrom::Start(0))?;
    let mut all = Vec::new();
    (&*f).take(SEGMENT_BYTES + 4096).read_to_end(&mut all)?;
    let Some(valid_end) = last_complete_record(&all) else {
        return Ok(());
    };
    if (valid_end as u64) < len {
        f.set_len(valid_end as u64)?;
    }

    Ok(())
}

fn roll_segment(base: &Path, f: &mut File) -> io::Result<()> {
    f.flush()?;
    let (seg0, seg1) = seg_paths(base);
    std::fs::rename(&seg0, &seg1)?;
    *f = Journal::open_segment(&seg0)?;
    Ok(())
}

fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= cap {
        let mut data = Vec::with_capacity(len as usize);
        f.read_to_end(&mut data)?;
        return Ok(data);
    }

    let start = len - cap;
    f.seek(SeekFrom::Start(start))?;
    let mut data = Vec::with_capacity(cap as usize);
    f.read_to_end(&mut data)?;

    let mut framed = Vec::with_capacity(data.len() + MAGIC.len());
    framed.extend_from_slice(MAGIC);
    framed.extend_from_slice(&data);
    Ok(framed)
}

pub fn read_file(path: &Path) -> io::Result<Vec<Entry>> {
    let (seg0, seg1) = seg_paths(path);
    if !seg0.exists() && !seg1.exists() && path.is_file() {

        return read_single(path, MAX_JOURNAL_BYTES);
    }

    let mut entries = read_single(&seg1, SEGMENT_BYTES).unwrap_or_default();
    entries.extend(read_single(&seg0, SEGMENT_BYTES)?);
    Ok(entries)
}

fn read_single(path: &Path, cap: u64) -> io::Result<Vec<Entry>> {
    let data = read_capped(path, cap)?;
    if data.len() < MAGIC.len() || &data[..MAGIC.len()] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a velocity-init journal file",
        ));
    }
    let mut entries = Vec::new();

    let mut off = MAGIC.len();
    while off + 15 <= data.len() {
        let payload_len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let micros = i64::from_le_bytes(data[off + 4..off + 12].try_into().unwrap());
        let level = data[off + 12];
        let src_len = u16::from_le_bytes(data[off + 13..off + 15].try_into().unwrap()) as usize;
        let payload_start = off + 15;
        if payload_len > data.len().saturating_sub(payload_start) {
            break;
        }
        if src_len > payload_len {
            break;
        }
        let payload = &data[payload_start..payload_start + payload_len];
        let source = String::from_utf8_lossy(&payload[..src_len]).into_owned();
        let message = String::from_utf8_lossy(&payload[src_len..]).into_owned();
        entries.push(Entry {
            micros,
            level,
            source,
            message,
        });
        off = payload_start + payload_len;
    }
    Ok(entries)
}

pub fn tail(path: &Path, n: usize) -> io::Result<Vec<Entry>> {
    let all = read_file(path)?;
    let start = all.len().saturating_sub(n);
    Ok(all.into_iter().skip(start).collect())
}

fn unix_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64) * 1_000_000 + i64::from(d.subsec_micros()),
        Err(e) => -((e.duration().as_secs() as i64) * 1_000_000),
    }
}

pub fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let sub_millis = ((micros.rem_euclid(1_000_000)) / 1_000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3_600, (rem / 60) % 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, m, d, hh, mm, ss, sub_millis
    )
}

pub struct JournalLogger {
    journal: Mutex<Journal>,
    filter: LevelFilter,
}

impl JournalLogger {
    pub fn new(journal: Journal) -> JournalLogger {
        let filter = std::env::var("VELOCITY_JOURNAL_LEVEL")
            .ok()
            .and_then(|l| l.parse().ok())
            .unwrap_or(LevelFilter::Info);
        JournalLogger {
            journal: Mutex::new(journal),
            filter,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.journal.lock().unwrap_or_else(|e| e.into_inner()).path().to_path_buf()
    }

    pub fn reopen(&self, path: &str) -> io::Result<()> {
        let j = Journal::open(path)?;
        *self.journal.lock().unwrap_or_else(|e| e.into_inner()) = j;
        Ok(())
    }
}

impl log::Log for JournalLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.filter
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Poison-tolerant on purpose: after a panic elsewhere, losing
        // one record would be a shame, but panicking *inside the
        // logger* while PID 1 handles that panic would be fatal — a
        // torn last record is trimmed by the reader.
        let _ = self
            .journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(&Entry::from_record(record));
    }

    fn flush(&self) {}
}

pub struct DualLogger {
    console: crate::console::ConsoleLogger,
    journal: Mutex<Option<JournalLogger>>,
}

impl DualLogger {
    pub fn new(
        console: crate::console::ConsoleLogger,
        journal: Option<JournalLogger>,
    ) -> DualLogger {
        DualLogger {
            console,
            journal: Mutex::new(journal),
        }
    }

    pub fn reopen_journal(&self, path: &str) -> io::Result<()> {
        match self.journal.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(j) => j.reopen(path),
            None => Ok(()),
        }
    }
}

impl log::Log for DualLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.console.enabled(metadata)
            || self
                .journal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|j| j.enabled(metadata))
    }

    fn log(&self, record: &Record) {
        self.console.log(record);
        if let Some(j) = self.journal.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            j.log(record);
        }
    }

    fn flush(&self) {
        self.console.flush();
        if let Some(j) = self.journal.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            j.flush();
        }
    }
}

#[cfg(test)]
mod lock_probe {
    use std::cell::Cell;

    thread_local! {
        static DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    pub struct Guard;

    impl Guard {
        pub fn enter() -> Self {
            DEPTH.with(|d| d.set(d.get() + 1));
            Guard
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }

    pub fn held_by_this_thread() -> bool {
        DEPTH.with(|d| d.get() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("velocity-journal-{}-{}", name, std::process::id()));
        cleanup(&p);
        p
    }

    fn cleanup(base: &Path) {
        let (seg0, seg1) = seg_paths(base);
        let _ = std::fs::remove_file(base);
        let _ = std::fs::remove_file(&seg0);
        let _ = std::fs::remove_file(&seg1);
    }

    fn total_len(base: &Path) -> u64 {
        let (seg0, seg1) = seg_paths(base);
        std::fs::metadata(&seg0).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(&seg1).map(|m| m.len()).unwrap_or(0)
    }

    fn mk_entry(level: u8, source: &str, message: &str) -> Entry {
        Entry {
            micros: 1_752_900_000_000_000 + level as i64,
            level,
            source: source.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn round_trip_keeps_order() {
        let p = temp_journal("roundtrip");
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        let e1 = mk_entry(3, "velocity_init", "boot start");
        let e2 = mk_entry(
            2,
            "velocity_init::supervisor",
            "spawned getty@console (pid 42)",
        );
        j.append(&e1).unwrap();
        j.append(&e2).unwrap();
        let entries = j.read().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], e1);
        assert_eq!(entries[1], e2);
        assert_eq!(entries[1].level(), Level::Warn);
        assert_eq!(entries[0].level(), Level::Info);
        cleanup(&p);
    }

    #[test]
    fn tail_returns_last_n() {
        let p = temp_journal("tail");
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        for i in 0..10 {
            j.append(&mk_entry(3, "s", &format!("m{i}"))).unwrap();
        }
        let t = tail(&p, 3).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].message, "m7");
        assert_eq!(t[2].message, "m9");
        cleanup(&p);
    }

    #[test]
    fn torn_tail_record_is_dropped() {
        let p = temp_journal("torn");
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        j.append(&mk_entry(3, "a", "keep me")).unwrap();

        let (seg0, _) = seg_paths(&p);
        let mut f = OpenOptions::new().append(true).open(&seg0).unwrap();
        f.write_all(&[0u8; 10]).unwrap();
        let entries = j.read().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "keep me");
        cleanup(&p);
    }

    #[test]
    fn foreign_file_is_replaced() {
        let p = temp_journal("foreign");
        std::fs::write(&p, b"not a journal").unwrap();
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        assert_eq!(j.read().unwrap().len(), 0);
        cleanup(&p);
    }

    #[test]
    fn concurrent_appenders_do_not_corrupt() {
        let p = temp_journal("concurrent");
        let j = std::sync::Arc::new(Journal::open(p.to_str().unwrap()).unwrap());
        let mut handles = Vec::new();
        for t in 0..4 {
            let jj = j.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    jj.append(&mk_entry(3, "t", &format!("{t}-{i}"))).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let entries = j.read().unwrap();
        assert_eq!(entries.len(), 100);
        let mut messages: Vec<String> = entries.iter().map(|e| e.message.clone()).collect();
        messages.sort();
        messages.dedup();
        assert_eq!(messages.len(), 100, "no torn/duplicated records");
        cleanup(&p);
    }

    #[test]
    fn format_time_is_iso8601() {

        let s = format_time(1_784_133_296_789_000);
        assert_eq!(s, "2026-07-15T16:34:56.789Z");
        let epoch = format_time(0);
        assert_eq!(epoch, "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn display_line_shape() {
        let e = Entry {
            micros: 1_784_133_296_789_000,
            level: 3,
            source: "velocity_init".to_string(),
            message: "hello".to_string(),
        };
        assert_eq!(
            e.display(),
            "[2026-07-15T16:34:56.789Z INFO  velocity_init] hello"
        );
    }

    fn fill(name: &str, n: usize, size: usize) -> (PathBuf, Vec<String>) {
        let p = temp_journal(name);
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        let mut msgs = Vec::with_capacity(n);
        for i in 0..n {
            let m = format!("{:0width$}", i, width = size);
            j.append(&mk_entry(3, "velocity_init", &m)).unwrap();
            msgs.push(m);
        }
        (p, msgs)
    }

    #[test]
    fn rotation_keeps_the_newest_records() {

        let size = 4096;
        let n = (MAX_JOURNAL_BYTES as usize / size) + 64;
        let (p, _) = fill("rotate", n, size);
        let entries = read_file(&p).unwrap();

        assert!(
            entries.len() > 1,
            "rotation must not empty the journal (got {} records)",
            entries.len()
        );
        let last = entries.last().unwrap();
        assert_eq!(
            last.message,
            format!("{:0width$}", n - 1, width = size),
            "the newest record must survive rotation"
        );
        assert!(
            !entries.iter().any(|e| e.message == format!("{:0width$}", 0, width = size)),
            "the evicted generation must be gone"
        );
        assert!(
            total_len(&p) <= MAX_JOURNAL_BYTES + size as u64,
            "rotation must bound both segments"
        );
        cleanup(&p);
    }

    #[test]
    fn reads_span_both_generations_oldest_first() {

        let size = 1024;
        let n = (SEGMENT_BYTES as usize / size) + 16;
        let (p, _) = fill("span", n, size);
        let (seg0, seg1) = seg_paths(&p);
        assert!(
            std::fs::metadata(&seg1).is_ok(),
            "filling past one segment must leave a previous generation"
        );
        assert!(
            std::fs::metadata(&seg0).is_ok(),
            "the active segment must exist"
        );
        let entries = read_file(&p).unwrap();
        let first = entries.first().unwrap();
        let last = entries.last().unwrap();
        assert_eq!(
            last.message,
            format!("{:0width$}", n - 1, width = size),
            "newest record last"
        );
        assert!(
            first.message < last.message,
            "records must read oldest-first across the roll"
        );
        cleanup(&p);
    }

    #[test]
    fn append_does_not_deadlock_when_rotation_fails() {

        let big = temp_journal("rotate-fail");
        let w = Journal::open(big.to_str().unwrap()).unwrap();
        w.append(&mk_entry(3, "velocity_init", "before the failure"))
            .unwrap();
        drop(w);
        let (seg0, seg1) = seg_paths(&big);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&seg0)
            .unwrap()
            .set_len(SEGMENT_BYTES + 4096)
            .unwrap();
        std::fs::create_dir_all(&seg1).unwrap();

        let j = Journal {
            path: big.clone(),
            file: Mutex::new(File::open(&seg0).unwrap()),
        };

        assert!(
            log::set_boxed_logger(Box::new(Probe)).is_ok(),
            "another unit test already installed a global logger"
        );
        log::set_max_level(LevelFilter::Info);

        j.append(&mk_entry(3, "velocity_init", "after a failed rotation"))
            .ok();

        assert!(
            PROBE_CALLS.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "rotation never failed, so the re-entrancy path was not tested"
        );
        let _ = std::fs::remove_dir_all(&seg1);
        cleanup(&big);
    }

    static PROBE_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    struct Probe;
    impl log::Log for Probe {
        fn enabled(&self, _m: &Metadata) -> bool {
            true
        }
        fn log(&self, record: &Record) {
            PROBE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(
                !lock_probe::held_by_this_thread(),
                "PID 1 logged while holding the journal lock: {}",
                record.args()
            );
        }
        fn flush(&self) {}
    }

    #[test]
    fn a_symlinked_journal_path_is_refused() {

        let dir = std::env::temp_dir().join(format!("velocity-journal-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("victim");
        std::fs::write(&target, b"important\n").unwrap();
        let link = dir.join("journal");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(
            Journal::open(link.to_str().unwrap()).is_err(),
            "a symlinked journal path must be refused, not followed"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"important\n",
            "the symlink target must be untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlong_source_is_truncated_not_corrupting_later_records() {

        let p = temp_journal("truncate-src");
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        let big_source = "s".repeat(u16::MAX as usize + 100);
        j.append(&Entry {
            micros: 1,
            level: 3,
            source: big_source,
            message: "m".to_string(),
        })
        .unwrap();
        j.append(&mk_entry(3, "velocity_init", "after")).unwrap();
        let entries = j.read().unwrap();
        assert_eq!(entries.len(), 2, "both records must survive: {entries:?}");
        assert_eq!(entries[0].source.len(), u16::MAX as usize);
        assert_eq!(entries[1].message, "after");
        cleanup(&p);
    }

    #[test]
    fn oversized_journal_is_reset_instead_of_read_into_memory() {

        let (p, _) = fill("oversized", 4, 32);
        let (seg0, _) = seg_paths(&p);
        let mut f = OpenOptions::new().append(true).open(&seg0).unwrap();
        f.write_all(&vec![b'x'; 4096]).unwrap();
        f.set_len(SEGMENT_BYTES + 8192).unwrap();
        drop(f);
        let j = Journal::open(p.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::metadata(&seg0).unwrap().len(), MAGIC.len() as u64);
        j.append(&mk_entry(3, "velocity_init", "fresh")).unwrap();
        assert_eq!(j.read().unwrap().len(), 1);
        cleanup(&p);
    }
}
