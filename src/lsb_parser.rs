use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::models::{LsbHeader, SocketKind};

const MAX_SCRIPT_READ: u64 = 1_048_576;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {

    #[error("I/O error reading script: {0}")]
    Io(#[from] std::io::Error),

    #[error("No LSB INIT INFO block found")]
    NoHeader,
}

fn extract_init_block(input: &str) -> Result<&str, ParseError> {

    let begin = input
        .find("### BEGIN INIT INFO")
        .ok_or(ParseError::NoHeader)?;
    let mut block = &input[begin + "### BEGIN INIT INFO".len()..];

    block = block
        .strip_prefix("\r\n")
        .or_else(|| block.strip_prefix('\n'))
        .unwrap_or(block);

    let end = block.find("### END INIT INFO").ok_or(ParseError::NoHeader)?;
    Ok(&block[..end])
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn parse_field_line(line: &str) -> Option<(String, String)> {
    let after_hash = line.trim_start().strip_prefix('#')?.trim_start();
    let (name, value) = after_hash.split_once(':')?;
    if name.is_empty() || !name.chars().all(is_field_char) {
        return None;
    }

    Some((name.to_ascii_lowercase(), value.trim().to_string()))
}

fn split_tokens(s: &str) -> Vec<String> {
    s.split_whitespace().map(|t| t.to_string()).collect()
}

fn apply_field(header: &mut LsbHeader, name: &str, value: &str) {
    match name {
        "provides" => header.provides.extend(split_tokens(value)),
        "required-start" => header.required_start.extend(split_tokens(value)),
        "required-stop" => header.required_stop.extend(split_tokens(value)),
        "should-start" => header.should_start.extend(split_tokens(value)),
        "should-stop" => header.should_stop.extend(split_tokens(value)),
        "default-start" => header.default_start.extend(split_tokens(value)),
        "default-stop" => header.default_stop.extend(split_tokens(value)),
        "short-description" => header.short_description = Some(value.to_string()),
        "description" => header.description = Some(value.to_string()),
        "x-velocity-socket" => {
            header.sockets.extend(
                split_tokens(value)
                    .iter()
                    .filter_map(|t| SocketKind::parse(t)),
            );
        }
        "x-velocity-limit" => {
            for token in split_tokens(value) {
                if let Some((file, val)) = token.split_once('=') {

                    if !file.is_empty() && !val.is_empty() {
                        header.limits.insert(file.to_string(), val.to_string());
                    }
                }
            }
        }
        "x-velocity-env" => {
            header.env_files.extend(split_tokens(value));
        }

        _ => {}
    }
}

fn append_description_continuation(header: &mut LsbHeader, line: &str) {
    let Some(after_hash) = line.trim_start().strip_prefix('#') else {
        return;
    };
    let cont = after_hash.trim();
    if cont.is_empty() {
        return;
    }
    if let Some(desc) = header.description.as_mut() {
        desc.push(' ');
        desc.push_str(cont);
    }
}

pub fn parse_str(content: &str) -> Result<LsbHeader, ParseError> {

    let block = extract_init_block(content)?;

    let mut header = LsbHeader::default();

    let mut last_field: Option<&str> = None;
    for line in block.lines() {
        if let Some((name, value)) = parse_field_line(line) {
            apply_field(&mut header, &name, &value);
            last_field = if name == "description" {
                Some("description")
            } else {
                None
            };
        } else if last_field == Some("description") {
            append_description_continuation(&mut header, line);
        }
    }

    Ok(header)
}

pub fn parse_header(file_path: &Path) -> Result<LsbHeader, ParseError> {

    let mut file = File::open(file_path).map_err(ParseError::Io)?;
    let mut content = String::new();
    file.by_ref()
        .take(MAX_SCRIPT_READ)
        .read_to_string(&mut content)
        .map_err(ParseError::Io)?;
    parse_str(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_block_basic() {
        let input = "#!/bin/sh\n### BEGIN INIT INFO\n# Provides: foo\n### END INIT INFO\necho hi";
        let block = extract_init_block(input).unwrap();
        assert!(block.contains("# Provides: foo"));
    }

    #[test]
    fn test_extract_block_missing() {
        let input = "#!/bin/sh\necho hello\n";
        assert!(extract_init_block(input).is_err());

        let unterminated = "### BEGIN INIT INFO\n# Provides: foo\n";
        assert!(extract_init_block(unterminated).is_err());
    }

    #[test]
    fn test_parse_field_line_shapes() {

        assert_eq!(
            parse_field_line("# Provides: foo"),
            Some(("provides".into(), "foo".into()))
        );
        assert_eq!(
            parse_field_line("  # Required-Start: $a $b  "),
            Some(("required-start".into(), "$a $b".into()))
        );
        assert_eq!(
            parse_field_line("#Short_Description:x"),
            Some(("short_description".into(), "x".into()))
        );

        for bad in ["", "   ", "# just a comment", "# no colon here", "#: x", "# Foo : x"] {
            assert_eq!(parse_field_line(bad), None, "{bad:?}");
        }

        assert_eq!(
            parse_field_line("# X: a:b"),
            Some(("x".into(), "a:b".into()))
        );
    }

    #[test]
    fn test_parse_str_end_to_end() {
        let script = "#!/bin/sh\n\
            ### BEGIN INIT INFO\n\
            # Provides:          sshd\n\
            # Required-Start:    $remote_fs $syslog\n\
            # Default-Start:     2 3 4 5\n\
            # Short-Description: OpenSSH server\n\
            # A plain comment is skipped.\n\
            ### END INIT INFO\n\
            echo hi\n";
        let h = parse_str(script).unwrap();
        assert_eq!(h.provides, vec!["sshd"]);
        assert_eq!(h.required_start, vec!["$remote_fs", "$syslog"]);
        assert_eq!(h.default_start, vec!["2", "3", "4", "5"]);
        assert_eq!(h.short_description.as_deref(), Some("OpenSSH server"));

        assert!(matches!(parse_str("echo hi\n"), Err(ParseError::NoHeader)));
    }

    #[test]
    fn test_split_tokens() {
        assert_eq!(
            split_tokens("$remote_fs $syslog"),
            vec!["$remote_fs", "$syslog"]
        );
        assert_eq!(split_tokens("  "), Vec::<String>::new());
        assert_eq!(split_tokens("single"), vec!["single"]);
    }

    #[test]
    fn test_apply_field_known() {
        let mut h = LsbHeader::default();
        apply_field(&mut h, "provides", "sshd");
        apply_field(&mut h, "required-start", "$remote_fs $syslog");
        apply_field(&mut h, "short-description", "SSH daemon");
        assert_eq!(h.provides, vec!["sshd"]);
        assert_eq!(h.required_start, vec!["$remote_fs", "$syslog"]);
        assert_eq!(h.short_description.as_deref(), Some("SSH daemon"));
    }

    #[test]
    fn test_apply_field_unknown() {
        let mut h = LsbHeader::default();
        apply_field(&mut h, "unknown-field", "value");

        assert_eq!(h, LsbHeader::default());
    }

    #[test]
    fn test_apply_field_sockets() {
        let mut h = LsbHeader::default();
        apply_field(&mut h, "x-velocity-socket", "tcp:8080 unix:/run/foo.sock");
        assert_eq!(
            h.sockets,
            vec![
                SocketKind::Tcp(8080),
                SocketKind::Unix(std::path::PathBuf::from("/run/foo.sock"))
            ]
        );
    }

    #[test]
    fn test_apply_field_sockets_bad_tokens_skipped() {
        let mut h = LsbHeader::default();
        apply_field(&mut h, "x-velocity-socket", "tcp:notaport udp:53 tcp:0");
        assert_eq!(h.sockets, vec![SocketKind::Tcp(0)]);
    }

    #[test]
    fn test_socket_kind_parse() {
        assert_eq!(SocketKind::parse("tcp:22"), Some(SocketKind::Tcp(22)));
        assert_eq!(SocketKind::parse("tcp:0"), Some(SocketKind::Tcp(0)));
        assert_eq!(
            SocketKind::parse("unix:/run/x.sock"),
            Some(SocketKind::Unix(std::path::PathBuf::from("/run/x.sock")))
        );
        assert_eq!(SocketKind::parse("udp:53"), None);
        assert_eq!(SocketKind::parse("tcp"), None);
        assert_eq!(SocketKind::parse(""), None);

        assert_eq!(SocketKind::parse("unix:"), None);
    }

    #[test]
    fn test_apply_field_limits() {
        let mut h = LsbHeader::default();
        apply_field(&mut h, "x-velocity-limit", "memory.max=512M cpu.max=50%");
        assert_eq!(h.limits.get("memory.max").map(String::as_str), Some("512M"));
        assert_eq!(h.limits.get("cpu.max").map(String::as_str), Some("50%"));
        assert_eq!(h.limits.len(), 2);
    }

    #[test]
    fn test_apply_field_env_files() {
        let mut h = LsbHeader::default();
        apply_field(
            &mut h,
            "x-velocity-env",
            "/etc/default/ssh /etc/default/network",
        );
        assert_eq!(
            h.env_files,
            vec![
                "/etc/default/ssh".to_string(),
                "/etc/default/network".to_string()
            ]
        );
        assert!(h.env_files.contains(&"/etc/default/ssh".to_string()));
    }

    #[test]
    fn test_apply_field_limits_bad_tokens_skipped() {
        let mut h = LsbHeader::default();

        apply_field(&mut h, "x-velocity-limit", "memory.max pids.max=64 =junk");
        assert_eq!(h.limits.len(), 1);
        assert_eq!(h.limits.get("pids.max").map(String::as_str), Some("64"));
    }

    #[test]
    fn test_apply_field_limits_empty_value_skipped() {
        let mut h = LsbHeader::default();

        apply_field(&mut h, "x-velocity-limit", "memory.max= pids.max=64");
        assert_eq!(h.limits.len(), 1);
        assert_eq!(h.limits.get("pids.max").map(String::as_str), Some("64"));
    }

    #[test]
    fn test_repeated_list_fields_union_not_overwrite() {

        let script = "#!/bin/sh\n\
            ### BEGIN INIT INFO\n\
            # Provides: x\n\
            # Required-Start: $a\n\
            # Required-Start: $b\n\
            # X-Velocity-Env: /etc/default/one\n\
            # X-Velocity-Env: /etc/default/two\n\
            ### END INIT INFO\n";
        let h = parse_str(script).unwrap();
        assert_eq!(h.required_start, vec!["$a", "$b"]);
        assert_eq!(h.env_files, vec!["/etc/default/one", "/etc/default/two"]);
    }

    #[test]
    fn test_multiline_description_continuation() {

        let script = "#!/bin/sh\n\
            ### BEGIN INIT INFO\n\
            # Provides: cron\n\
            # Description: first line\n\
            #   second line\n\
            # third line\n\
            ### END INIT INFO\n";
        let h = parse_str(script).unwrap();
        assert_eq!(
            h.description.as_deref(),
            Some("first line second line third line")
        );
    }

    #[test]
    fn test_description_continuation_only_for_description() {

        let script = "#!/bin/sh\n\
            ### BEGIN INIT INFO\n\
            # Provides: x\n\
            # Short-Description: one-liner\n\
            # just a comment\n\
            ### END INIT INFO\n";
        let h = parse_str(script).unwrap();
        assert_eq!(h.short_description.as_deref(), Some("one-liner"));
        assert_eq!(h.description, None);
    }

    #[test]
    fn test_field_names_are_ascii_only() {

        assert_eq!(parse_field_line("# é: x"), None);
        assert!(parse_field_line("# Provides: ok").is_some());
    }

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn fuzz_parser_never_panics_nor_hangs() {

        const PIECES: &[&str] = &[
            "### BEGIN INIT INFO",
            "### END INIT INFO",
            "### BEGIN",
            "# Provides:",
            "# Required-Start:",
            "# X-Velocity-Socket:",
            "#",
            "##",
            ":",
            "::",
            "\n",
            "\r\n",
            "\r",
            " ",
            "\t",
            "$remote_fs",
            "ssh",
            "tcp:80",
            "unix:/run/x.sock",
            "\0",
            "é",
            "−",
            "-",
            "_",
            "Description: x",
            "a",
        ];
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..3000 {
            let mut s = String::new();
            let n = (rng.next() % 14) as usize;
            for _ in 0..n {
                s.push_str(PIECES[(rng.next() % PIECES.len() as u64) as usize]);
            }

            let _ = parse_str(&s);
        }

        let mut big = "#".repeat(100_000);
        big.push_str("\n### BEGIN INIT INFO\n# Provides: big\n### END INIT INFO\n");
        let h = parse_str(&big).unwrap();
        assert_eq!(h.provides, vec!["big"]);

        let mut open = String::from("### BEGIN INIT INFO\n");
        open.push_str(&"# x: y\n".repeat(10_000));
        assert!(matches!(parse_str(&open), Err(ParseError::NoHeader)));
    }

    #[test]
    fn test_multiple_fields_end_to_end() {
        let script = "#!/bin/sh\n\
            ### BEGIN INIT INFO\n\
            # Provides: sshd\n\
            # Required-Start: $remote_fs $syslog\n\
            # Default-Start: 2 3 4 5\n\
            # A plain comment is skipped.\n\
            # Short-Description: OpenSSH server\n\
            ### END INIT INFO\n\
            echo hi\n";
        let h = parse_str(script).unwrap();
        assert_eq!(h.provides, vec!["sshd"]);
        assert_eq!(h.required_start, vec!["$remote_fs", "$syslog"]);
        assert_eq!(h.default_start, vec!["2", "3", "4", "5"]);
        assert_eq!(h.short_description.as_deref(), Some("OpenSSH server"));
    }
}
