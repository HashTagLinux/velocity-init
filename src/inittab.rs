use std::collections::HashMap;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {

    Sysinit,

    Boot,

    Bootwait,

    Wait,

    Once,

    Respawn,

    Askfirst,

    CtrlAltDel,

    PowerFail,

    PowerWait,

    PowerOkWait,

    PowerFailNow,

    Shutdown,

    Restart,

    InitDefault,

    Ondemand,

    Off,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "sysinit" => Self::Sysinit,
            "boot" => Self::Boot,
            "bootwait" => Self::Bootwait,
            "wait" => Self::Wait,
            "once" => Self::Once,
            "respawn" => Self::Respawn,
            "askfirst" => Self::Askfirst,
            "ctrlaltdel" => Self::CtrlAltDel,
            "powerfail" => Self::PowerFail,
            "powerwait" => Self::PowerWait,
            "powerokwait" => Self::PowerOkWait,
            "powerfailnow" => Self::PowerFailNow,
            "shutdown" => Self::Shutdown,
            "restart" => Self::Restart,
            "initdefault" => Self::InitDefault,
            "ondemand" => Self::Ondemand,
            "off" => Self::Off,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {

    pub id: String,

    pub runlevels: String,

    pub action: Action,

    pub process: String,
}

impl Line {

    pub fn matches(&self, level: &str) -> bool {
        if level.len() != 1 {
            return false;
        }
        if self.runlevels.is_empty() {
            return level.chars().next().is_some_and(|c| c.is_ascii_digit());
        }
        self.runlevels.contains(level)
    }

    pub fn matches_insensitive(&self, level: &str) -> bool {
        if level.len() != 1 {
            return false;
        }
        let level_lower = level.to_ascii_lowercase();
        if self.runlevels.is_empty() {
            return level_lower.chars().next().is_some_and(|c| c.is_ascii_digit());
        }
        self.runlevels.to_ascii_lowercase().contains(&level_lower)
    }

    pub fn cmd(&self) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            self.process.clone(),
        ]
    }
}

#[derive(Debug, Clone, Default)]
pub struct Inittab {
    lines: Vec<Line>,
}

impl Inittab {

    pub const DEFAULT_PATH: &'static str = "/etc/inittab";

    pub fn parse(path: &str) -> io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&bytes);
        Ok(Self::parse_str(&text))
    }

    pub fn parse_str(text: &str) -> Self {
        let mut lines = Vec::new();
        for raw in text.lines() {
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }

            let mut parts = raw.splitn(4, ':');
            let id = parts.next().unwrap_or("").trim().to_string();
            let runlevels = parts.next().unwrap_or("").trim().to_string();
            let action_s = parts.next().unwrap_or("").trim();
            let process = parts.next().unwrap_or("").trim().to_string();
            let Some(action) = Action::parse(action_s) else {
                log::debug!("inittab: skipping line with unknown action {:?}", action_s);
                continue;
            };
            if process.is_empty() && action != Action::InitDefault {
                log::debug!("inittab: skipping line {:?} with empty process", id);
                continue;
            }
            lines.push(Line {
                id,
                runlevels,
                action,
                process,
            });
        }
        Self { lines }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn default_level(&self) -> Option<String> {
        let l = self
            .lines
            .iter()
            .find(|l| l.action == Action::InitDefault)
            .map(|l| l.runlevels.clone())
            .filter(|l| !l.is_empty())?;

        let mut chars = l.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            log::warn!(
                "inittab: initdefault runlevel {:?} is not a single level — ignoring it",
                l
            );
            return None;
        }
        let upper = first.to_ascii_uppercase();
        if matches!(upper, '0'..='9' | 'S') {
            Some(upper.to_string())
        } else {
            log::warn!(
                "inittab: initdefault runlevel {:?} is not a boot runlevel (0-6, S) — ignoring it",
                l
            );
            None
        }
    }

    pub fn valid_level(level: &str) -> bool {
        level.len() == 1
            && matches!(
                level.chars().next(),
                Some('0'..='9' | 'S' | 's' | 'A'..='C' | 'a'..='c')
            )
    }

    pub fn is_ondemand(level: &str) -> bool {
        level.len() == 1 && matches!(level.chars().next(), Some('A'..='C' | 'a'..='c'))
    }

    pub fn normalize_level(level: &str) -> String {
        if level == "s" {
            "S".to_string()
        } else {
            level.to_string()
        }
    }

    fn by_action(&self, action: Action) -> Vec<&Line> {
        self.lines.iter().filter(|l| l.action == action).collect()
    }

    pub fn boot_lines(&self) -> Vec<&Line> {
        let mut out = self.by_action(Action::Sysinit);
        out.extend(
            self.lines
                .iter()
                .filter(|l| matches!(l.action, Action::Boot | Action::Bootwait)),
        );
        out
    }

    pub fn blocks_boot(&self, line: &Line) -> bool {
        !matches!(line.action, Action::Boot)
    }

    pub fn wait_lines_for(&self, level: &str) -> Vec<&Line> {
        self.lines
            .iter()
            .filter(|l| l.action == Action::Wait && l.matches(level))
            .collect()
    }

    pub fn once_lines_for(&self, level: &str) -> Vec<&Line> {
        self.lines
            .iter()
            .filter(|l| l.action == Action::Once && l.matches(level))
            .collect()
    }

    pub fn respawn_lines_for(&self, level: &str) -> Vec<&Line> {
        self.lines
            .iter()
            .filter(|l| {
                (l.action == Action::Respawn || l.action == Action::Askfirst) && l.matches(level)
            })
            .collect()
    }

    pub fn ondemand_lines_for(&self, level: &str) -> Vec<&Line> {
        self.lines
            .iter()
            .filter(|l| l.action == Action::Ondemand && l.matches_insensitive(level))
            .collect()
    }

    pub fn ctrlaltdel(&self) -> Option<&Line> {
        self.by_action(Action::CtrlAltDel).first().copied()
    }

    pub fn shutdown_lines(&self) -> Vec<&Line> {
        self.by_action(Action::Shutdown)
    }

    pub fn power_action(&self, action: Action) -> Option<&Line> {
        self.by_action(action).first().copied()
    }

    pub fn wait_line_for(&self, level: &str) -> Option<&Line> {
        self.wait_lines_for(level).first().copied()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SysvContext {

    pub level: String,

    pub boot_names: Vec<String>,

    pub boot_async_names: Vec<String>,

    pub wait_names: Vec<String>,

    pub once_names: Vec<String>,

    pub respawn_names: Vec<String>,

    pub respawn_levels: HashMap<String, String>,

    pub shutdown_names: Vec<String>,

    pub wait_for_level: HashMap<String, Vec<String>>,

    pub once_for_level: HashMap<String, Vec<String>>,

    pub ondemand_for_level: HashMap<String, Vec<String>>,

    pub commands: HashMap<String, Vec<String>>,
}

impl SysvContext {

    pub fn build(it: &Inittab, level: &str) -> Self {
        let boot_lines = it.boot_lines();
        let boot_names: Vec<String> = (0..boot_lines.len())
            .map(|i| format!("boot@{i}"))
            .collect();

        let boot_async_names: Vec<String> = boot_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !it.blocks_boot(l))
            .map(|(i, _)| format!("boot@{i}"))
            .collect();

        let mut wait_name_of_line: HashMap<usize, String> = HashMap::new();
        let mut wait_for_level: HashMap<String, Vec<String>> = HashMap::new();
        for (idx, l) in it.lines.iter().enumerate().filter(|(_, l)| l.action == Action::Wait) {
            let levels = levels_of(l);
            let first_tag = levels.first().copied().unwrap_or('0').to_string();

            let count = wait_for_level.get(&first_tag).map(Vec::len).unwrap_or(0);
            let name = if count == 0 {
                format!("wait@{first_tag}")
            } else {
                format!("wait@{first_tag}#{count}")
            };
            for lv in &levels {
                wait_for_level
                    .entry(lv.to_string())
                    .or_default()
                    .push(name.clone());
            }
            wait_name_of_line.insert(idx, name);
        }

        let wait_names = it
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.action == Action::Wait && l.matches(level))
            .filter_map(|(idx, _)| wait_name_of_line.get(&idx).cloned())
            .collect::<Vec<String>>();

        let mut once_name_of_line: HashMap<usize, String> = HashMap::new();
        let mut once_for_level: HashMap<String, Vec<String>> = HashMap::new();
        for (idx, l) in it.lines.iter().enumerate().filter(|(_, l)| l.action == Action::Once) {
            let levels = levels_of(l);
            let first_tag = levels.first().copied().unwrap_or('0').to_string();
            let count = once_for_level.get(&first_tag).map(Vec::len).unwrap_or(0);
            let name = if count == 0 {
                format!("once@{first_tag}")
            } else {
                format!("once@{first_tag}#{count}")
            };
            for lv in &levels {
                once_for_level
                    .entry(lv.to_string())
                    .or_default()
                    .push(name.clone());
            }
            once_name_of_line.insert(idx, name);
        }
        let once_names = it
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.action == Action::Once && l.matches(level))
            .filter_map(|(idx, _)| once_name_of_line.get(&idx).cloned())
            .collect::<Vec<String>>();

        let mut ondemand_name_of_line: HashMap<usize, String> = HashMap::new();
        let mut ondemand_for_level: HashMap<String, Vec<String>> = HashMap::new();
        for (idx, l) in it
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.action == Action::Ondemand)
        {
            let name = if l.id.is_empty() {
                format!("ondemand@{idx}")
            } else {
                format!("ondemand@{}", l.id)
            };

            let mut unique = name.clone();
            let mut suffix = 1;
            while ondemand_name_of_line.values().any(|n| n == &unique) {
                unique = format!("{name}#{suffix}");
                suffix += 1;
            }
            for lv in l.runlevels.to_ascii_lowercase().chars() {
                let entry = ondemand_for_level.entry(lv.to_string()).or_default();
                if !entry.contains(&unique) {
                    entry.push(unique.clone());
                }
            }

            ondemand_name_of_line.insert(idx, unique);
        }

        let mut respawn_names = Vec::new();
        for l in it.respawn_lines_for(level) {
            let name = respawn_name(&l.id, &l.process);
            if !respawn_names.contains(&name) {
                respawn_names.push(name.clone());
            }
        }

        let mut respawn_levels: HashMap<String, String> = HashMap::new();
        let mut commands: HashMap<String, Vec<String>> = HashMap::new();
        for l in it
            .lines
            .iter()
            .filter(|l| l.action == Action::Respawn || l.action == Action::Askfirst)
        {
            let name = respawn_name(&l.id, &l.process);
            match respawn_levels.get_mut(&name) {
                None => {
                    respawn_levels.insert(name.clone(), l.runlevels.clone());
                }
                Some(existing) => {

                    if l.runlevels.is_empty() {
                        existing.clear();
                    } else if !existing.is_empty() {
                        for c in l.runlevels.chars() {
                            if !existing.contains(c) {
                                existing.push(c);
                            }
                        }
                    }
                    log::debug!(
                        "inittab: duplicate respawn service {name:?} (id {:?}): levels merged",
                        l.id,
                    );
                }
            }
            let cmd = if l.action == Action::Askfirst {
                askfirst_cmd(&l.process)
            } else {
                l.cmd()
            };
            match commands.get(&name) {
                None => {
                    commands.insert(name.clone(), cmd);
                }
                Some(first) if first != &cmd => {
                    log::warn!(
                        "inittab: service {name:?} defined twice (id {:?} shadows another line) — keeping the first command",
                        l.id,
                    );
                }
                Some(_) => {}
            }
        }

        for (i, l) in it.boot_lines().iter().enumerate() {
            commands.insert(format!("boot@{i}"), l.cmd());
        }
        for (idx, l) in it.lines.iter().enumerate().filter(|(_, l)| l.action == Action::Wait) {
            if let Some(name) = wait_name_of_line.get(&idx) {
                commands.insert(name.clone(), l.cmd());
            }
        }
        for (idx, l) in it.lines.iter().enumerate().filter(|(_, l)| l.action == Action::Once) {
            if let Some(name) = once_name_of_line.get(&idx) {
                commands.insert(name.clone(), l.cmd());
            }
        }
        for (idx, l) in it
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.action == Action::Ondemand)
        {
            if let Some(name) = ondemand_name_of_line.get(&idx) {
                commands.insert(name.clone(), l.cmd());
            }
        }
        let shutdown_names: Vec<String> = (0..it.shutdown_lines().len())
            .map(|i| format!("shut@{i}"))
            .collect();
        for (i, l) in it.shutdown_lines().iter().enumerate() {
            commands.insert(format!("shut@{i}"), l.cmd());
        }

        Self {
            level: level.to_string(),
            boot_names,
            boot_async_names,
            wait_names,
            once_names,
            respawn_names,
            respawn_levels,
            shutdown_names,
            wait_for_level,
            once_for_level,
            ondemand_for_level,
            commands,
        }
    }

    pub fn name_command(&self, name: &str) -> Option<Vec<String>> {
        self.commands.get(name).cloned()
    }

    pub fn all_respawn_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.respawn_levels.keys().cloned().collect();
        out.sort();
        out
    }

    pub fn wait_services(&self, level: &str) -> &[String] {
        self.wait_for_level.get(level).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn respawn_active(&self, svc: &str, level: &str) -> bool {
        match self.respawn_levels.get(svc) {
            None => false,
            Some(rl) => {
                if level.len() != 1 {
                    return false;
                }
                if rl.is_empty() {
                    return level.chars().next().is_some_and(|c| c.is_ascii_digit());
                }
                rl.contains(level)
            }
        }
    }

    pub fn once_services(&self, level: &str) -> &[String] {
        self.once_for_level.get(level).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn ondemand_services(&self, level: &str) -> &[String] {
        self.ondemand_for_level
            .get(&level.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Levels a line applies to, in file order, without repetitions.
///
/// An empty runlevel field means 0-9 (inittab(5)). A repeated character
/// in the field (`rc:22:wait:...`) must not register the service name
/// twice: `wait_services`/`once_services` are consumed as start lists,
/// and a `wait` line would otherwise run its script once per repetition.
fn levels_of(l: &Line) -> Vec<char> {
    let raw = if l.runlevels.is_empty() {
        "0123456789"
    } else {
        l.runlevels.as_str()
    };
    let mut levels: Vec<char> = Vec::with_capacity(raw.len());
    for c in raw.chars() {
        if !levels.contains(&c) {
            levels.push(c);
        }
    }
    levels
}

fn respawn_name(id: &str, process: &str) -> String {

    let toks: Vec<&str> = process.split_whitespace().collect();
    let exe = toks
        .first()
        .copied()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    if exe.ends_with("getty") {

        let tty = toks
            .iter()
            .skip(1)
            .find(|t| t.starts_with("tty"))
            .or_else(|| toks.last())
            .copied()
            .unwrap_or("tty");
        return format!("getty@{tty}");
    }
    if id.is_empty() {
        format!("res@{}", exe)
    } else {
        format!("res@{}", id)
    }
}

pub fn askfirst_cmd(process: &str) -> Vec<String> {

    let script = format!(
        "{{ printf '\\nPlease press Enter to activate this console. '; \
         read -r _ && exec {process}; }} </dev/console >/dev/console 2>&1"
    );
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVUAN_LIKE: &str = "\
# /etc/inittab: noexec:0:ignore:/sbin/init
id:2:initdefault:

si::sysinit:/etc/init.d/rcS

l0:0:wait:/etc/init.d/rc 0
l1:1:wait:/etc/init.d/rc 1
l2:2:wait:/etc/init.d/rc 2
l3:3:wait:/etc/init.d/rc 3
l4:4:wait:/etc/init.d/rc 4
l5:5:wait:/etc/init.d/rc 5
l6:6:wait:/etc/init.d/rc 6

ca:12345:ctrlaltdel:/sbin/shutdown -t1 -a -r now
pf:12345:powerfail:/sbin/shutdown -f -h +0 \"Power Failure; System Shutting Down\"
pn:12345:powerokwait:/sbin/shutdown -c \"Power Restored; Shutdown Cancelled\"

1:2345:respawn:/sbin/getty 38400 tty1
2:23:respawn:/sbin/getty 38400 tty2
6:23:respawn:/sbin/getty 38400 tty6
";

    #[test]
    fn test_parse_devuan_like() {
        let it = Inittab::parse_str(DEVUAN_LIKE);
        assert!(!it.is_empty());
        assert_eq!(it.default_level().as_deref(), Some("2"));
        assert_eq!(it.boot_lines().len(), 1);
        assert_eq!(it.boot_lines()[0].process, "/etc/init.d/rcS");
        assert_eq!(it.wait_lines_for("2")[0].process, "/etc/init.d/rc 2");
        assert_eq!(it.wait_lines_for("S").len(), 0);
        assert!(it.ctrlaltdel().is_some());
        assert!(it.power_action(Action::PowerFail).is_some());
        assert!(it.power_action(Action::PowerOkWait).is_some());
        assert_eq!(it.respawn_lines_for("2").len(), 3);
        assert_eq!(it.respawn_lines_for("4").len(), 1);
        assert_eq!(it.respawn_lines_for("S").len(), 0);
    }

    #[test]
    fn test_parse_skips_garbage() {
        let it = Inittab::parse_str(
            "# comment\n\nid:2:initdefault:\nthis:line:is:short\nbad::nosuchaction:/bin/true\n",
        );
        assert_eq!(it.lines.len(), 1);
        assert_eq!(it.default_level().as_deref(), Some("2"));
    }

    #[test]
    fn test_process_keeps_colons() {
        let it = Inittab::parse_str("x::once:/usr/bin/foo --opt /a:b/c\n");
        assert_eq!(it.lines.len(), 1);
        assert_eq!(it.lines[0].process, "/usr/bin/foo --opt /a:b/c");
    }

    #[test]
    fn test_matches_semantics() {
        let line = Line {
            id: "1".into(),
            runlevels: "2345".into(),
            action: Action::Respawn,
            process: "/bin/true".into(),
        };
        assert!(line.matches("2"));
        assert!(line.matches("5"));
        assert!(!line.matches("1"));
        assert!(!line.matches("S"));
        assert!(!line.matches("6"));

        assert!(!line.matches("23"));
        assert!(!line.matches(""));

        let empty = Line {
            runlevels: String::new(),
            ..line.clone()
        };
        assert!(empty.matches("2"));
        assert!(empty.matches("0"));
        assert!(!empty.matches("S"));
        assert!(!empty.matches("a"));

        assert!(!line.matches("A"));
    }

    #[test]
    fn test_missing_file_is_empty() {
        let it = Inittab::parse("/nonexistent/inittab").expect("missing file is Ok");
        assert!(it.is_empty());
    }

    #[test]
    fn test_cmd_uses_sh_c() {
        let line = Line {
            id: "si".into(),
            runlevels: String::new(),
            action: Action::Sysinit,
            process: "/etc/init.d/rcS".into(),
        };
        assert_eq!(
            line.cmd(),
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "/etc/init.d/rcS".into()
            ]
        );
    }

    #[test]
    fn test_build_devuan_context() {
        let it = Inittab::parse_str(DEVUAN_LIKE);
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.level, "2");
        assert_eq!(ctx.boot_names, vec!["boot@0"]);
        assert_eq!(ctx.wait_names, vec!["wait@2"]);
        assert_eq!(ctx.once_names.len(), 0);

        assert!(ctx.respawn_names.contains(&"getty@tty1".to_string()));
        assert!(ctx.respawn_names.contains(&"getty@tty2".to_string()));
        assert_eq!(ctx.wait_services("0"), ["wait@0".to_string()]);
        assert_eq!(ctx.wait_services("6"), ["wait@6".to_string()]);
        assert!(ctx.respawn_active("getty@tty1", "3"));
        assert!(!ctx.respawn_active("getty@tty2", "5"));
        assert!(!ctx.respawn_active("getty@tty2", "S"));
    }

    #[test]
    fn test_boot_and_bootwait_lines_are_not_dropped() {

        let it = Inittab::parse_str(
            "si::sysinit:/etc/init.d/rcS\n\
             pr::bootwait:/etc/init.d/preseed\n\
             la::boot:/etc/init.d/late\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.boot_names, vec!["boot@0", "boot@1", "boot@2"]);

        assert_eq!(
            ctx.commands["boot@0"],
            vec!["/bin/sh", "-c", "/etc/init.d/rcS"]
        );

        assert_eq!(
            ctx.commands["boot@1"],
            vec!["/bin/sh", "-c", "/etc/init.d/preseed"]
        );
        assert_eq!(
            ctx.commands["boot@2"],
            vec!["/bin/sh", "-c", "/etc/init.d/late"]
        );

        assert!(it.blocks_boot(it.boot_lines()[0]), "sysinit waits");
        assert!(it.blocks_boot(it.boot_lines()[1]), "bootwait waits");
        assert!(!it.blocks_boot(it.boot_lines()[2]), "boot is background");

        assert_eq!(ctx.boot_async_names, vec!["boot@2".to_string()]);
    }

    #[test]
    fn test_only_sysinit_has_no_async_boot_lines() {

        let it = Inittab::parse_str(
            "si::sysinit:/etc/init.d/rcS\n\
             l2:2:wait:/etc/init.d/rc 2\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert!(ctx.boot_async_names.is_empty());
        assert_eq!(ctx.boot_names, vec!["boot@0".to_string()]);
    }

    #[test]
    fn test_two_wait_lines_at_one_level_both_run() {

        let it = Inittab::parse_str(
            "id:2:initdefault:\n\
             a:2:wait:/etc/init.d/first\n\
             b:2:wait:/etc/init.d/second\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.wait_names, vec!["wait@2", "wait@2#1"]);
        assert_eq!(
            ctx.commands["wait@2"],
            vec!["/bin/sh", "-c", "/etc/init.d/first"]
        );
        assert_eq!(
            ctx.commands["wait@2#1"],
            vec!["/bin/sh", "-c", "/etc/init.d/second"]
        );

        assert_eq!(
            ctx.wait_services("2"),
            ["wait@2".to_string(), "wait@2#1".to_string()]
        );
    }

    #[test]
    fn test_respawn_names() {
        assert_eq!(respawn_name("1", "/sbin/getty 38400 tty1"), "getty@tty1");
        assert_eq!(
            respawn_name("", "/usr/bin/simple-agent"),
            "res@simple-agent"
        );

        assert_eq!(
            respawn_name("s0", "/sbin/getty -L ttyS0 115200"),
            "getty@ttyS0"
        );
    }

    #[test]
    fn test_askfirst_cmd_prompts_before_exec() {
        let cmd = askfirst_cmd("/sbin/getty -L ttyS0 115200");
        assert_eq!(cmd[0], "/bin/sh");
        assert_eq!(cmd[1], "-c");
        let script = &cmd[2];
        assert!(script.contains("Please press Enter"), "got: {script}");

        assert!(script.contains("read -r _ && exec /sbin/getty -L ttyS0 115200"));
    }

    #[test]
    fn test_askfirst_lines_get_prompt_wrapping() {
        let it = Inittab::parse_str("s0:2345:askfirst:/sbin/getty -L ttyS0 115200\n");
        let ctx = SysvContext::build(&it, "2");
        let cmd = ctx.name_command("getty@ttyS0").expect("askfirst command");
        assert!(cmd[2].contains("Please press Enter"));

        let it = Inittab::parse_str("1:2345:respawn:/sbin/getty 38400 tty1\n");
        let ctx = SysvContext::build(&it, "2");
        let cmd = ctx.name_command("getty@tty1").expect("respawn command");
        assert!(!cmd[2].contains("Please press Enter"));
    }

    #[test]
    fn test_shutdown_lines_are_indexed() {
        let it = Inittab::parse_str(
            "x::sysinit:/etc/init.d/rcS\n\
             um::shutdown:/sbin/umount -a -r\n\
             st:0:shutdown:/sbin/halt\n\
             id:2:initdefault:\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(
            ctx.shutdown_names,
            vec!["shut@0".to_string(), "shut@1".to_string()]
        );

        assert_eq!(
            ctx.name_command("shut@0"),
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "/sbin/umount -a -r".to_string(),
            ])
        );

        assert_eq!(ctx.name_command("shut@2"), None);
    }

    #[test]
    fn test_respawn_indexed_across_levels_for_telinit() {
        let it = Inittab::parse_str(
            "1:2345:respawn:/sbin/getty 38400 tty1\n\
             2:23:respawn:/sbin/getty 38400 tty2\n",
        );
        let ctx = SysvContext::build(&it, "2");

        let all = ctx.all_respawn_names();
        assert!(all.contains(&"getty@tty1".to_string()));
        assert!(all.contains(&"getty@tty2".to_string()));
    }

    #[test]
    fn test_multi_level_wait_line_is_indexed_under_every_level() {

        let it = Inittab::parse_str(
            "id:2:initdefault:\n\
             rc:2345:wait:/etc/init.d/rc\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.wait_names, vec!["wait@2".to_string()]);
        assert_eq!(ctx.wait_services("3"), ["wait@2".to_string()]);
        assert_eq!(ctx.wait_services("4"), ["wait@2".to_string()]);
        assert_eq!(ctx.wait_services("5"), ["wait@2".to_string()]);
        assert!(ctx.wait_services("S").is_empty());

        assert_eq!(
            ctx.name_command("wait@2"),
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "/etc/init.d/rc".to_string(),
            ])
        );
    }

    #[test]
    fn test_duplicate_runlevels_do_not_duplicate_services() {

        let it = Inittab::parse_str(
            "id:2:initdefault:\n\
             rc:22:wait:/etc/init.d/rc 2\n\
             on:Aa:ondemand:/bin/echo on\n\
             thrice:33:once:/bin/echo thrice\n",
        );
        let ctx = SysvContext::build(&it, "2");

        assert_eq!(ctx.wait_services("2"), ["wait@2".to_string()]);

        assert_eq!(ctx.once_services("3"), ["once@3".to_string()]);

        assert_eq!(ctx.ondemand_services("a"), ["ondemand@on".to_string()]);
        assert_eq!(ctx.ondemand_services("A").len(), 1);
    }

    #[test]
    fn test_once_lines_are_indexed_per_level_for_telinit() {

        let it = Inittab::parse_str(
            "id:2:initdefault:\n\
             a:2:once:/bin/echo two\n\
             b:3:once:/bin/echo three\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.once_names.len(), 1);
        assert_eq!(ctx.once_services("2").len(), 1);
        assert_eq!(ctx.once_services("3").len(), 1);
        assert_ne!(ctx.once_services("2"), ctx.once_services("3"));
        assert!(ctx.name_command(&ctx.once_services("3")[0]).is_some());
    }

    #[test]
    fn test_ondemand_lines_parse_and_index() {
        let it = Inittab::parse_str(
            "id:2:initdefault:\n\
             x:a:ondemand:/bin/echo on-a\n\
             y:AB:ondemand:/bin/echo on-ab\n",
        );

        assert_eq!(it.ondemand_lines_for("a").len(), 2);
        assert_eq!(it.ondemand_lines_for("A").len(), 2);
        assert_eq!(it.ondemand_lines_for("b").len(), 1);
        assert!(it.ondemand_lines_for("2").is_empty());
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.ondemand_services("a").len(), 2);
        assert_eq!(ctx.ondemand_services("A").len(), 2);
        assert_eq!(ctx.ondemand_services("b").len(), 1);
        assert!(ctx.ondemand_services("2").is_empty());
    }

    #[test]
    fn test_valid_level_covers_single_user_lowercase_and_ondemand() {
        assert!(Inittab::valid_level("2"));
        assert!(Inittab::valid_level("S"));
        assert!(Inittab::valid_level("s"));
        assert!(Inittab::valid_level("a"));
        assert!(Inittab::valid_level("C"));
        assert!(Inittab::is_ondemand("a"));
        assert!(Inittab::is_ondemand("C"));
        assert!(!Inittab::is_ondemand("2"));
        assert!(!Inittab::is_ondemand("S"));
        assert!(!Inittab::valid_level("23"));
        assert!(!Inittab::valid_level(""));
        assert_eq!(Inittab::normalize_level("s"), "S");
        assert_eq!(Inittab::normalize_level("2"), "2");
    }

    #[test]
    fn test_default_level_rejects_multi_and_validates() {
        let it = Inittab::parse_str("id:23:initdefault:\n");
        assert_eq!(it.default_level(), None);
        let it = Inittab::parse_str("id:s:initdefault:\n");
        assert_eq!(it.default_level().as_deref(), Some("S"));
        let it = Inittab::parse_str("id:a:initdefault:\n");
        assert_eq!(it.default_level(), None);
        let it = Inittab::parse_str("id:2:initdefault:\n");
        assert_eq!(it.default_level().as_deref(), Some("2"));
    }

    #[test]
    fn test_duplicate_respawn_names_merge_levels() {

        let it = Inittab::parse_str(
            "1:2:respawn:/sbin/getty 38400 tty1\n\
             2:3:respawn:/sbin/getty 38400 tty1\n",
        );
        let ctx = SysvContext::build(&it, "2");
        assert_eq!(ctx.respawn_names, vec!["getty@tty1".to_string()]);
        assert!(ctx.respawn_active("getty@tty1", "2"));
        assert!(ctx.respawn_active("getty@tty1", "3"));
        assert!(!ctx.respawn_active("getty@tty1", "S"));
    }

    #[test]
    fn test_getty_flavours_share_the_tty_naming() {

        assert_eq!(respawn_name("1", "/sbin/rungetty tty1"), "getty@tty1");
        assert_eq!(respawn_name("1", "/sbin/fgetty tty2"), "getty@tty2");
    }
}
