use std::collections::{BTreeMap, BTreeSet};

use crate::models::{DependencyType, LsbHeader};

#[derive(Debug, thiserror::Error)]
#[error("Circular dependency detected: [{}]", cycle.join(" → "))]
pub struct CycleError {

    pub cycle: Vec<String>,
}

pub struct DependencyGraph {

    names: Vec<String>,

    name_to_idx: BTreeMap<String, usize>,

    edges: Vec<(usize, usize, DependencyType)>,
}

impl DependencyGraph {

    pub fn from_headers(headers: Vec<(String, LsbHeader)>) -> Self {

        let mut facility_map: BTreeMap<String, String> = BTreeMap::new();

        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (name, header) in &headers {
            if !seen.insert(name.clone()) {
                continue;
            }
            if header.provides.is_empty() {

                facility_map
                    .entry(name.clone())
                    .or_insert_with(|| name.clone());
            } else {
                for facility in &header.provides {
                    facility_map
                        .entry(facility.clone())
                        .and_modify(|owner| {
                            if name < owner {
                                *owner = name.clone();
                            }
                        })
                        .or_insert_with(|| name.clone());
                }
            }
        }

        let mut names = Vec::with_capacity(headers.len());
        let mut name_to_idx = BTreeMap::new();

        for (name, _) in &headers {
            if name_to_idx.contains_key(name) {
                continue;
            }
            name_to_idx.insert(name.clone(), names.len());
            names.push(name.clone());
        }

        let mut edges = Vec::new();
        let mut edged: BTreeSet<String> = BTreeSet::new();
        for (name, header) in &headers {
            if !edged.insert(name.clone()) {
                continue;
            }
            let dependent_idx = name_to_idx[name];

            for req in &header.required_start {
                if let Some(dep_name) = facility_map.get(req) {
                    if let Some(&dep_idx) = name_to_idx.get(dep_name) {
                        if dep_idx != dependent_idx {
                            edges.push((dep_idx, dependent_idx, DependencyType::Requires));
                        }
                    }
                }

            }

            for req in &header.should_start {
                if let Some(dep_name) = facility_map.get(req) {
                    if let Some(&dep_idx) = name_to_idx.get(dep_name) {
                        if dep_idx != dependent_idx {
                            edges.push((dep_idx, dependent_idx, DependencyType::ShouldStart));
                        }
                    }
                }
            }
        }

        Self {
            names,
            name_to_idx,
            edges,
        }
    }

    fn adjacency(&self) -> Vec<Vec<usize>> {
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); self.names.len()];
        for (from, to, _) in &self.edges {
            adj[*from].push(*to);
        }
        adj
    }

    fn successors(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        self.edges
            .iter()
            .filter(move |(from, _, _)| *from == node)
            .map(|(_, to, _)| *to)
    }

    fn predecessors(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        self.edges
            .iter()
            .filter(move |(_, to, _)| *to == node)
            .map(|(from, _, _)| *from)
    }

    pub fn detect_cycles(&self) -> Option<Vec<String>> {
        let adj = self.adjacency();

        let mut color = vec![0u8; self.names.len()];
        for start in 0..self.names.len() {
            if color[start] != 0 {
                continue;
            }

            let mut stack = vec![start];
            let mut next = vec![0usize];
            color[start] = 1;
            while let Some(&node) = stack.last() {
                let depth = stack.len() - 1;
                if next[depth] < adj[node].len() {
                    let succ = adj[node][next[depth]];
                    next[depth] += 1;
                    if color[succ] == 1 {

                        let pos = stack.iter().position(|&x| x == succ).unwrap_or(0);
                        return Some(
                            stack[pos..]
                                .iter()
                                .map(|&i| self.names[i].clone())
                                .collect(),
                        );
                    }
                    if color[succ] == 0 {
                        color[succ] = 1;
                        stack.push(succ);
                        next.push(0);
                    }
                } else {
                    color[node] = 2;
                    stack.pop();
                    next.pop();
                }
            }
        }
        None
    }

    pub fn break_cycles(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();

        let edge_budget = self.edges.len();
        while let Some(cycle) = self.detect_cycles() {
            let cyc_idx: Vec<usize> = cycle
                .iter()
                .filter_map(|n| self.name_to_idx.get(n).copied())
                .collect();
            if cyc_idx.is_empty() {
                break;
            }

            let ring: Vec<(usize, usize)> = cyc_idx
                .iter()
                .enumerate()
                .map(|(i, &n)| (n, cyc_idx[(i + 1) % cyc_idx.len()]))
                .collect();
            let on_ring = |(from, to): (usize, usize)| {
                ring.iter().any(|&(r_from, r_to)| r_from == from && r_to == to)
            };

            let victim = self
                .edges
                .iter()
                .position(|(from, to, kind)| {
                    *kind == DependencyType::ShouldStart && on_ring((*from, *to))
                })
                .or_else(|| {
                    self.edges
                        .iter()
                        .position(|(from, to, _)| on_ring((*from, *to)))
                })
                .or_else(|| {
                    let members: BTreeSet<usize> = cyc_idx.iter().copied().collect();
                    let head = cyc_idx.into_iter().min().unwrap_or(0);
                    self.edges
                        .iter()
                        .position(|(from, to, _)| *to == head && members.contains(from))
                        .or_else(|| {
                            self.edges.iter().position(|(_, to, _)| *to == head)
                        })
                });
            let Some(id) = victim else { break };

            let (src, dst, kind) = self.edges[id].clone();
            let verb = match kind {
                DependencyType::ShouldStart => "should-start",
                DependencyType::Requires => "requires",
                DependencyType::Provides => "provides",
            };
            dropped.push(format!(
                "{} {verb} {}",
                self.names[dst], self.names[src]
            ));
            self.edges.remove(id);
            if dropped.len() >= edge_budget {

                break;
            }
        }
        dropped
    }

    pub fn get_startup_order(&self) -> Result<Vec<String>, CycleError> {

        if let Some(cycle) = self.detect_cycles() {
            return Err(CycleError { cycle });
        }

        let mut indegree = vec![0usize; self.names.len()];
        for (_, to, _) in &self.edges {
            indegree[*to] += 1;
        }

        let mut ready: BTreeSet<usize> = indegree
            .iter()
            .enumerate()
            .filter(|(_, &d)| d == 0)
            .map(|(i, _)| i)
            .collect();
        let mut sorted = Vec::with_capacity(self.names.len());
        while let Some(&node) = ready.iter().next() {
            ready.remove(&node);
            sorted.push(node);
            for succ in self.successors(node) {
                indegree[succ] -= 1;
                if indegree[succ] == 0 {
                    ready.insert(succ);
                }
            }
        }
        if sorted.len() != self.names.len() {

            let stuck: Vec<String> = (0..self.names.len())
                .filter(|i| indegree[*i] > 0)
                .map(|i| self.names[i].clone())
                .collect();
            return Err(CycleError { cycle: stuck });
        }

        Ok(sorted.iter().map(|&idx| self.names[idx].clone()).collect())
    }

    pub fn startup_levels(&self) -> Result<Vec<Vec<String>>, CycleError> {
        let order = self.get_startup_order()?;

        let mut level_of: BTreeMap<String, usize> = BTreeMap::new();
        for name in &order {
            let idx = self.name_to_idx[name];
            let dep_level = self
                .predecessors(idx)
                .map(|d| level_of.get(&self.names[d]).copied().unwrap_or(0))
                .max()
                .unwrap_or(0);
            level_of.insert(name.clone(), dep_level + 1);
        }

        let mut levels: Vec<Vec<String>> = Vec::new();
        for name in &order {
            let l = level_of[name] - 1;
            if levels.len() <= l {
                levels.resize(l + 1, Vec::new());
            }
            levels[l].push(name.clone());
        }
        for level in &mut levels {
            level.sort();
        }
        Ok(levels)
    }

    #[allow(dead_code)]
    pub fn get_dependencies(&self, service: &str) -> Option<Vec<(String, DependencyType)>> {
        let idx = *self.name_to_idx.get(service)?;
        let mut deps: Vec<(String, DependencyType)> = self
            .edges
            .iter()
            .filter(|(_, to, _)| *to == idx)
            .map(|(from, _, kind)| (self.names[*from].clone(), kind.clone()))
            .collect();
        deps.sort_by(|a, b| a.0.cmp(&b.0));
        Some(deps)
    }

    #[allow(dead_code)]
    pub fn get_dependents(&self, service: &str) -> Option<Vec<String>> {
        let idx = *self.name_to_idx.get(service)?;
        let mut dependents: Vec<String> = self
            .edges
            .iter()
            .filter(|(from, _, _)| *from == idx)
            .map(|(_, to, _)| self.names[*to].clone())
            .collect();
        dependents.sort();
        Some(dependents)
    }

    #[allow(dead_code)]
    pub fn get_all_services(&self) -> Vec<String> {
        self.names.clone()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(provides: &[&str], req_start: &[&str], should_start: &[&str]) -> LsbHeader {
        LsbHeader {
            provides: provides.iter().map(|s| s.to_string()).collect(),
            required_start: req_start.iter().map(|s| s.to_string()).collect(),
            should_start: should_start.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_simple_chain() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &[], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
            ("c".into(), hdr(&["c"], &["b"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);

        assert!(g.detect_cycles().is_none());

        let order = g.get_startup_order().unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_diamond_dependency() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &[], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
            ("c".into(), hdr(&["c"], &["a"], &[])),
            ("d".into(), hdr(&["d"], &["b", "c"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);

        assert!(g.detect_cycles().is_none());

        let order = g.get_startup_order().unwrap();
        let a_pos = order.iter().position(|n| n == "a").unwrap();
        let b_pos = order.iter().position(|n| n == "b").unwrap();
        let c_pos = order.iter().position(|n| n == "c").unwrap();
        let d_pos = order.iter().position(|n| n == "d").unwrap();

        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[test]
    fn test_cycle_detection_three_nodes() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &["b"], &[])),
            ("b".into(), hdr(&["b"], &["c"], &[])),
            ("c".into(), hdr(&["c"], &["a"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);

        let cycle = g.detect_cycles();
        assert!(cycle.is_some(), "should detect cycle");

        let cycle = cycle.unwrap();
        assert_eq!(cycle.len(), 3);
        assert!(cycle.contains(&"a".to_string()));
        assert!(cycle.contains(&"b".to_string()));
        assert!(cycle.contains(&"c".to_string()));

        assert!(g.get_startup_order().is_err());
    }

    #[test]
    fn test_self_loop_detection() {
        let headers = vec![
            ("x".into(), hdr(&["x"], &["x"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);

        assert!(g.detect_cycles().is_none());
        assert_eq!(g.get_startup_order().unwrap(), vec!["x"]);
    }

    #[test]
    fn test_should_start_edge() {
        let headers = vec![
            ("base".into(), hdr(&["base"], &[], &[])),
            ("upper".into(), hdr(&["upper"], &[], &["base"])),
        ];
        let g = DependencyGraph::from_headers(headers);

        let deps = g.get_dependencies("upper").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0, "base");
        assert_eq!(deps[0].1, DependencyType::ShouldStart);
    }

    #[test]
    fn test_unresolved_facility_ignored() {

        let headers = vec![("a".into(), hdr(&["a"], &["$nonexistent"], &[]))];
        let g = DependencyGraph::from_headers(headers);

        assert!(g.detect_cycles().is_none());
        assert_eq!(g.get_startup_order().unwrap(), vec!["a"]);
    }

    #[test]
    fn test_get_dependents() {
        let headers = vec![
            ("a".into(), hdr(&["a"], &[], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
            ("c".into(), hdr(&["c"], &["a"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);

        let dependents = g.get_dependents("a").unwrap();
        assert_eq!(dependents, vec!["b", "c"]);
    }

    #[test]
    fn test_get_all_services() {
        let headers = vec![
            ("alpha".into(), hdr(&["alpha"], &[], &[])),
            ("beta".into(), hdr(&["beta"], &["alpha"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);

        let mut services = g.get_all_services();
        services.sort();
        assert_eq!(services, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_empty_graph() {
        let g = DependencyGraph::from_headers(vec![]);
        assert!(g.detect_cycles().is_none());
        assert_eq!(g.get_startup_order().unwrap(), Vec::<String>::new());
        assert_eq!(g.startup_levels().unwrap(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn test_startup_levels_chain() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &[], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
            ("c".into(), hdr(&["c"], &["b"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert_eq!(
            g.startup_levels().unwrap(),
            vec![
                vec!["a".to_string()],
                vec!["b".to_string()],
                vec!["c".to_string()]
            ]
        );
    }

    #[test]
    fn test_startup_levels_diamond() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &[], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
            ("c".into(), hdr(&["c"], &["a"], &[])),
            ("d".into(), hdr(&["d"], &["b", "c"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert_eq!(
            g.startup_levels().unwrap(),
            vec![
                vec!["a".to_string()],
                vec!["b".to_string(), "c".to_string()],
                vec!["d".to_string()]
            ]
        );
    }

    #[test]
    fn test_startup_levels_unrelated_services_parallel() {

        let headers = vec![
            ("x".into(), hdr(&["x"], &[], &[])),
            ("y".into(), hdr(&["y"], &[], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert_eq!(
            g.startup_levels().unwrap(),
            vec![vec!["x".to_string(), "y".to_string()]]
        );
    }

    #[test]
    fn test_startup_levels_rejects_cycles() {
        let headers = vec![
            ("a".into(), hdr(&["a"], &["b"], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert!(g.startup_levels().is_err());
    }

    #[test]
    fn test_startup_levels_should_start_counts() {

        let headers = vec![
            ("base".into(), hdr(&["base"], &[], &[])),
            ("upper".into(), hdr(&["upper"], &[], &["base"])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert_eq!(
            g.startup_levels().unwrap(),
            vec![vec!["base".to_string()], vec!["upper".to_string()]]
        );
    }

    #[test]
    fn test_break_cycles_hard_cycle_boots() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &["b"], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
        ];
        let mut g = DependencyGraph::from_headers(headers);
        assert!(g.detect_cycles().is_some());
        let dropped = g.break_cycles();
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].contains("requires"), "{dropped:?}");
        assert!(g.detect_cycles().is_none());

        assert_eq!(g.startup_levels().unwrap().len(), 2);
    }

    #[test]
    fn test_break_cycles_prefers_soft_edges() {

        let headers = vec![
            ("a".into(), hdr(&["a"], &["b"], &[])),
            ("b".into(), hdr(&["b"], &[], &["a"])),
        ];
        let mut g = DependencyGraph::from_headers(headers);
        assert!(g.detect_cycles().is_some());
        let dropped = g.break_cycles();
        assert_eq!(dropped.len(), 1);
        assert!(
            dropped[0].contains("should-start"),
            "soft edge must go first, got {dropped:?}"
        );
        assert!(g.detect_cycles().is_none());

        let order = g.get_startup_order().unwrap();
        assert!(order.iter().position(|n| n == "b") < order.iter().position(|n| n == "a"));
    }

    #[test]
    fn test_break_cycles_acyclic_is_noop() {
        let headers = vec![
            ("a".into(), hdr(&["a"], &[], &[])),
            ("b".into(), hdr(&["b"], &["a"], &[])),
        ];
        let mut g = DependencyGraph::from_headers(headers);
        assert!(g.break_cycles().is_empty());
        assert_eq!(g.get_startup_order().unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn test_facility_alias_resolves_to_provider() {

        let headers = vec![
            ("web".into(), hdr(&["httpd"], &[], &[])),
            ("app".into(), hdr(&["app"], &["httpd"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert_eq!(g.get_startup_order().unwrap(), vec!["web", "app"]);
        let deps = g.get_dependencies("app").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0, "web");
    }

    #[test]
    fn test_facility_conflict_resolves_deterministically() {

        let mk = |order: &[&str]| {
            let headers: Vec<(String, LsbHeader)> = order
                .iter()
                .map(|n| (n.to_string(), hdr(&["shared"], &[], &[])))
                .chain(std::iter::once((
                    "client".to_string(),
                    hdr(&["client"], &["shared"], &[]),
                )))
                .collect();
            DependencyGraph::from_headers(headers)
        };
        let g1 = mk(&["bbb", "aaa"]);
        let g2 = mk(&["aaa", "bbb"]);
        let dep_of = |g: &DependencyGraph| g.get_dependencies("client").unwrap()[0].0.clone();
        assert_eq!(dep_of(&g1), "aaa");
        assert_eq!(dep_of(&g2), "aaa");
    }

    #[test]
    fn test_duplicate_service_names_keep_first() {

        let headers = vec![
            ("dup".into(), hdr(&["dup"], &[], &[])),
            ("dup".into(), hdr(&["other"], &["dup"], &[])),
        ];
        let g = DependencyGraph::from_headers(headers);
        assert_eq!(g.len(), 1);
        assert_eq!(g.get_startup_order().unwrap(), vec!["dup"]);
        assert!(g.detect_cycles().is_none());
    }

    #[test]
    fn test_len_and_is_empty() {
        let empty = DependencyGraph::from_headers(vec![]);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        let headers = vec![("a".into(), hdr(&["a"], &[], &[]))];
        let g = DependencyGraph::from_headers(headers);
        assert!(!g.is_empty());
        assert_eq!(g.len(), 1);
    }
}
