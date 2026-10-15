use std::path::PathBuf;

use velocity_init::graph::DependencyGraph;
use velocity_init::lsb_parser;
use velocity_init::models::{DependencyType, LsbHeader};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn cycle_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cycle_data")
}

fn parse(name: &str) -> LsbHeader {
    let path = data_dir().join(name);
    lsb_parser::parse_header(&path).unwrap_or_else(|_| panic!("failed to parse {}", name))
}

fn parse_cycle(name: &str) -> LsbHeader {
    let path = cycle_data_dir().join(name);
    lsb_parser::parse_header(&path).unwrap_or_else(|_| panic!("failed to parse {}", name))
}

mod parser_tests {
    use super::*;

    #[test]
    fn test_parse_network() {
        let h = parse("network");
        assert_eq!(h.provides, vec!["$network"]);
        assert!(h.required_start.is_empty());
        assert!(h.required_stop.is_empty());
        assert_eq!(h.default_start, vec!["2", "3", "4", "5"]);
        assert_eq!(h.default_stop, vec!["0", "1", "6"]);
        assert_eq!(
            h.short_description.as_deref(),
            Some("Raise network interfaces")
        );
    }

    #[test]
    fn test_parse_remote_fs() {
        let h = parse("remote_fs");
        assert_eq!(h.provides, vec!["$remote_fs"]);
        assert_eq!(h.required_start, vec!["$network"]);
        assert_eq!(h.required_stop, vec!["$network"]);
    }

    #[test]
    fn test_parse_syslog() {
        let h = parse("syslog");
        assert_eq!(h.provides, vec!["$syslog"]);
        assert_eq!(h.required_start, vec!["$remote_fs"]);
        assert_eq!(h.required_stop, vec!["$remote_fs"]);
    }

    #[test]
    fn test_parse_ssh() {
        let h = parse("ssh");
        assert_eq!(h.provides, vec!["sshd"]);
        assert_eq!(h.required_start, vec!["$remote_fs", "$syslog"]);
        assert_eq!(h.required_stop, vec!["$remote_fs", "$syslog"]);
        assert_eq!(h.should_start, vec!["$network"]);
        assert_eq!(h.default_start, vec!["2", "3", "4", "5"]);
        assert_eq!(h.default_stop, vec!["0", "1", "6"]);
        assert_eq!(h.short_description.as_deref(), Some("OpenSSH server"));
    }

    #[test]
    fn test_parse_cron() {
        let h = parse("cron");
        assert_eq!(h.provides, vec!["cron"]);
        assert_eq!(h.required_start, vec!["$remote_fs", "$syslog"]);
    }

    #[test]
    fn test_parse_no_header_returns_error() {
        let path = data_dir().join("no_header");
        let result = lsb_parser::parse_header(&path);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(matches!(err, lsb_parser::ParseError::NoHeader));
    }

    #[test]
    fn test_parse_nonexistent_file_returns_io_error() {
        let path = data_dir().join("does_not_exist");
        let result = lsb_parser::parse_header(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), lsb_parser::ParseError::Io(_)));
    }

    #[test]
    fn test_parse_cycle_scripts() {
        let a = parse_cycle("cycle_a");
        assert_eq!(a.provides, vec!["cycle_a"]);
        assert_eq!(a.required_start, vec!["cycle_b"]);

        let b = parse_cycle("cycle_b");
        assert_eq!(b.provides, vec!["cycle_b"]);
        assert_eq!(b.required_start, vec!["cycle_c"]);

        let c = parse_cycle("cycle_c");
        assert_eq!(c.provides, vec!["cycle_c"]);
        assert_eq!(c.required_start, vec!["cycle_a"]);
    }
}

mod graph_tests {
    use super::*;

    fn normal_headers() -> Vec<(String, LsbHeader)> {
        let names = ["network", "remote_fs", "syslog", "ssh", "cron"];
        names.iter().map(|&n| (n.to_string(), parse(n))).collect()
    }

    fn cycle_headers() -> Vec<(String, LsbHeader)> {
        let names = ["cycle_a", "cycle_b", "cycle_c"];
        names
            .iter()
            .map(|&n| (n.to_string(), parse_cycle(n)))
            .collect()
    }

    #[test]
    fn test_startup_order_no_cycles() {
        let headers = normal_headers();
        let g = DependencyGraph::from_headers(headers);

        assert!(g.detect_cycles().is_none());

        let order = g.get_startup_order().expect("should succeed");

        assert_eq!(order.first().unwrap(), "network");

        let net_idx = order.iter().position(|n| n == "network").unwrap();
        let rfs_idx = order.iter().position(|n| n == "remote_fs").unwrap();
        assert!(net_idx < rfs_idx);

        let sys_idx = order.iter().position(|n| n == "syslog").unwrap();
        assert!(rfs_idx < sys_idx);

        let ssh_idx = order.iter().position(|n| n == "ssh").unwrap();
        assert!(rfs_idx < ssh_idx);
        assert!(sys_idx < ssh_idx);

        let cron_idx = order.iter().position(|n| n == "cron").unwrap();
        assert!(rfs_idx < cron_idx);
        assert!(sys_idx < cron_idx);

        assert_eq!(order.len(), 5);
    }

    #[test]
    fn test_startup_order_includes_all_services() {
        let headers = normal_headers();
        let g = DependencyGraph::from_headers(headers);
        let order = g.get_startup_order().unwrap();

        for name in &["network", "remote_fs", "syslog", "ssh", "cron"] {
            assert!(
                order.contains(&name.to_string()),
                "missing {} in startup order: {:?}",
                name,
                order
            );
        }
    }

    #[test]
    fn test_cycle_detection() {
        let headers = cycle_headers();
        let g = DependencyGraph::from_headers(headers);

        let cycle = g.detect_cycles();
        assert!(cycle.is_some(), "should detect a cycle");

        let cycle = cycle.unwrap();
        assert_eq!(cycle.len(), 3, "cycle should contain all 3 nodes");
        assert!(cycle.contains(&"cycle_a".to_string()));
        assert!(cycle.contains(&"cycle_b".to_string()));
        assert!(cycle.contains(&"cycle_c".to_string()));
    }

    #[test]
    fn test_cycle_prevents_startup_order() {
        let headers = cycle_headers();
        let g = DependencyGraph::from_headers(headers);

        let result = g.get_startup_order();
        assert!(result.is_err(), "should fail with CycleError");

        let err = result.unwrap_err();
        assert!(!err.cycle.is_empty());
    }

    #[test]
    fn test_mixed_graph_with_cycle() {

        let mut headers = normal_headers();
        headers.extend(cycle_headers());

        let g = DependencyGraph::from_headers(headers);
        assert!(g.detect_cycles().is_some());
        assert!(g.get_startup_order().is_err());
    }

    #[test]
    fn test_get_dependencies_ssh() {
        let headers = normal_headers();
        let g = DependencyGraph::from_headers(headers);

        let deps = g.get_dependencies("ssh").expect("ssh should be in graph");
        assert_eq!(
            deps.len(),
            3,
            "ssh should have 3 dependencies (2 required + 1 should)"
        );

        let req_deps: Vec<_> = deps
            .iter()
            .filter(|(_, t)| *t == DependencyType::Requires)
            .collect();
        assert_eq!(req_deps.len(), 2);
        let req_names: Vec<_> = req_deps.iter().map(|(n, _)| n.as_str()).collect();
        assert!(req_names.contains(&"remote_fs"));
        assert!(req_names.contains(&"syslog"));

        let should_deps: Vec<_> = deps
            .iter()
            .filter(|(_, t)| *t == DependencyType::ShouldStart)
            .collect();
        assert_eq!(should_deps.len(), 1);
        assert_eq!(should_deps[0].0, "network");
    }

    #[test]
    fn test_get_dependencies_network_has_none() {
        let headers = normal_headers();
        let g = DependencyGraph::from_headers(headers);

        let deps = g
            .get_dependencies("network")
            .expect("network should be in graph");
        assert!(deps.is_empty(), "network should have no dependencies");
    }

    #[test]
    fn test_get_dependents_network() {
        let headers = normal_headers();
        let g = DependencyGraph::from_headers(headers);

        let dependents = g
            .get_dependents("network")
            .expect("network should be in graph");
        assert!(dependents.contains(&"remote_fs".to_string()));
        assert!(dependents.contains(&"ssh".to_string()));
    }

    #[test]
    fn test_get_all_services() {
        let headers = normal_headers();
        let g = DependencyGraph::from_headers(headers);

        let mut services = g.get_all_services();
        services.sort();
        assert_eq!(
            services,
            vec!["cron", "network", "remote_fs", "ssh", "syslog"]
        );
    }
}

mod e2e_tests {
    use super::*;

    #[test]
    fn test_dry_run_simulation() {

        let names = ["network", "remote_fs", "syslog", "ssh", "cron"];
        let headers: Vec<(String, LsbHeader)> =
            names.iter().map(|&n| (n.to_string(), parse(n))).collect();

        let g = DependencyGraph::from_headers(headers);

        assert!(g.detect_cycles().is_none());

        let order = g.get_startup_order().unwrap();

        for name in &order {
            if let Some(deps) = g.get_dependencies(name) {
                for (dep_name, _) in &deps {
                    let dep_pos = order.iter().position(|n| n == dep_name);
                    let my_pos = order.iter().position(|n| n == name);
                    if let (Some(dep_pos), Some(my_pos)) = (dep_pos, my_pos) {
                        assert!(
                            dep_pos < my_pos,
                            "Dependency {} should come before {} (got positions {} and {})",
                            dep_name,
                            name,
                            dep_pos,
                            my_pos
                        );
                    }
                }
            }
        }

        println!("Dry-run startup order: {:?}", order);
    }
}
