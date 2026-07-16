use crate::event::RuleResult;
use crate::graph::{Graph, GraphError, Node, NodeKind};

/// A deterministic diagnostic rule over the production graph.
pub trait Rule {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn check(&self, graph: &Graph) -> Result<RuleResult, GraphError>;
}

/// Upsert all rules into the graph as `Rule` nodes and return the IDs of any
/// that are not yet approved. New rules get `approved: false`.
pub fn upsert_rules(graph: &mut Graph, rules: &[Box<dyn Rule>]) -> Result<Vec<String>, GraphError> {
    let mut unapproved = Vec::new();
    for rule in rules {
        let id = if let Some(existing) = graph.find_node_by_name(NodeKind::Rule, rule.id())? {
            // Preserve existing approval state; only update name if needed.
            let mut payload = existing.payload.clone();
            payload["name"] = serde_json::Value::String(rule.name().to_string());
            graph.update_node_payload(existing.id, payload)?;
            existing.id
        } else {
            let payload = serde_json::json!({ "name": rule.name(), "approved": false });
            let node = Node::new(NodeKind::Rule, rule.id(), payload);
            graph.create_node(&node)?
        };

        // Check if already approved from a previous run.
        if let Some(existing) = graph.get_node(id)? {
            if existing.payload.get("approved") != Some(&serde_json::Value::Bool(true)) {
                unapproved.push(rule.id().to_string());
            }
        } else {
            unapproved.push(rule.id().to_string());
        }
    }
    Ok(unapproved)
}

/// Approve a rule by its ID. Returns true if the rule node was found and updated.
pub fn approve_rule(graph: &mut Graph, rule_id: &str) -> Result<bool, GraphError> {
    graph.approve_rule(rule_id)
}

/// All built-in rules. Add new rules here.
pub fn built_in_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(EdgeIntegrityRule),
        Box::new(CodeLinkedRule),
        Box::new(NoEmptyPayloadRule),
    ]
}

/// Every edge must connect two existing nodes.
pub struct EdgeIntegrityRule;

impl Rule for EdgeIntegrityRule {
    fn id(&self) -> &'static str {
        "edge_integrity"
    }

    fn name(&self) -> &'static str {
        "Edge integrity"
    }

    fn check(&self, graph: &Graph) -> Result<RuleResult, GraphError> {
        let edges = graph.list_edges()?;
        let mut broken = Vec::new();
        for edge in edges {
            if graph.get_node(edge.from)?.is_none() {
                broken.push(format!(
                    "edge {} references missing from node {}",
                    edge.id, edge.from
                ));
            }
            if graph.get_node(edge.to)?.is_none() {
                broken.push(format!(
                    "edge {} references missing to node {}",
                    edge.id, edge.to
                ));
            }
        }
        if broken.is_empty() {
            Ok(RuleResult::Pass)
        } else {
            Ok(RuleResult::Fail {
                reason: broken.join("; "),
            })
        }
    }
}

/// Every code node should be linked to at least one other node.
pub struct CodeLinkedRule;

impl Rule for CodeLinkedRule {
    fn id(&self) -> &'static str {
        "code_linked"
    }

    fn name(&self) -> &'static str {
        "Code files are linked"
    }

    fn check(&self, graph: &Graph) -> Result<RuleResult, GraphError> {
        let code_nodes = graph.list_nodes(Some(NodeKind::Code))?;
        let mut unlinked = Vec::new();
        for node in code_nodes {
            let neighbors = graph.neighbors(node.id)?;
            if neighbors.is_empty() {
                unlinked.push(node.name);
            }
        }
        if unlinked.is_empty() {
            Ok(RuleResult::Pass)
        } else {
            Ok(RuleResult::Warn {
                reason: format!("unlinked code files: {}", unlinked.join(", ")),
            })
        }
    }
}

/// Nodes should not have empty payloads unless they are intentional stubs.
pub struct NoEmptyPayloadRule;

impl Rule for NoEmptyPayloadRule {
    fn id(&self) -> &'static str {
        "no_empty_payload"
    }

    fn name(&self) -> &'static str {
        "No empty payloads"
    }

    fn check(&self, graph: &Graph) -> Result<RuleResult, GraphError> {
        let nodes = graph.list_nodes(None)?;
        let empty: Vec<String> = nodes
            .iter()
            .filter(|n| n.payload == serde_json::Value::Object(Default::default()))
            .map(|n| format!("{} ({})", n.name, n.kind.as_str()))
            .collect();
        if empty.is_empty() {
            Ok(RuleResult::Pass)
        } else {
            Ok(RuleResult::Warn {
                reason: format!("nodes with empty payloads: {}", empty.join(", ")),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeKind, Graph, Node, NodeKind};

    #[test]
    fn rule_executions_become_graph_history() {
        // The graph must learn its own health history: every rule execution
        // is recorded as a RuleTriggered event tied to the rule's node.
        let mut graph = Graph::open_in_memory().unwrap();
        let rules = built_in_rules();
        upsert_rules(&mut graph, &rules).unwrap();

        for rule in &rules {
            let result = rule.check(&graph).unwrap();
            let node = graph
                .find_node_by_name(NodeKind::Rule, rule.id())
                .unwrap()
                .expect("upserted rule has a node");
            graph
                .record_event(&crate::Event::RuleTriggered {
                    rule_id: node.id,
                    result,
                })
                .unwrap();
        }

        let triggered: Vec<_> = graph
            .events(50)
            .unwrap()
            .into_iter()
            .filter(|(_, event)| event.kind() == "rule_triggered")
            .collect();
        assert_eq!(
            triggered.len(),
            rules.len(),
            "one RuleTriggered event per executed rule"
        );
    }

    #[test]
    fn edge_integrity_passes_when_all_edges_are_valid() {
        let mut graph = Graph::open_in_memory().unwrap();
        let a = Node::new(NodeKind::Task, "a", serde_json::json!({"status": "open"}));
        let b = Node::new(NodeKind::Code, "b", serde_json::json!({"path": "b.rs"}));
        let aid = graph.create_node(&a).unwrap();
        let bid = graph.create_node(&b).unwrap();
        graph.create_edge(aid, bid, EdgeKind::Implements).unwrap();

        let rule = EdgeIntegrityRule;
        assert!(matches!(rule.check(&graph).unwrap(), RuleResult::Pass));
    }

    #[test]
    fn edge_integrity_fails_on_dangling_edge() {
        let mut graph = Graph::open_in_memory().unwrap();
        let a = Node::new(NodeKind::Task, "a", serde_json::json!({}));
        let b = Node::new(NodeKind::Code, "b", serde_json::json!({}));
        let aid = graph.create_node(&a).unwrap();
        let bid = graph.create_node(&b).unwrap();
        graph.create_edge(aid, bid, EdgeKind::Implements).unwrap();

        // Simulate corruption: delete `b` but leave the edge.
        graph.test_delete_node_leave_edges(bid).unwrap();

        let rule = EdgeIntegrityRule;
        let result = rule.check(&graph).unwrap();
        assert!(
            matches!(&result, RuleResult::Fail { reason } if reason.contains("missing to node")),
            "expected fail with missing to node, got {:?}",
            result
        );
    }

    #[test]
    fn code_linked_warns_for_orphan_code() {
        let mut graph = Graph::open_in_memory().unwrap();
        let code = Node::new(
            NodeKind::Code,
            "orphan.rs",
            serde_json::json!({"path": "orphan.rs"}),
        );
        graph.create_node(&code).unwrap();

        let rule = CodeLinkedRule;
        let result = rule.check(&graph).unwrap();
        assert!(
            matches!(&result, RuleResult::Warn { reason } if reason.contains("orphan.rs")),
            "expected warn for orphan code, got {:?}",
            result
        );
    }

    #[test]
    fn code_linked_passes_when_code_is_linked() {
        let mut graph = Graph::open_in_memory().unwrap();
        let task = Node::new(NodeKind::Task, "t", serde_json::json!({}));
        let code = Node::new(NodeKind::Code, "linked.rs", serde_json::json!({}));
        let tid = graph.create_node(&task).unwrap();
        let cid = graph.create_node(&code).unwrap();
        graph.create_edge(tid, cid, EdgeKind::DependsOn).unwrap();

        let rule = CodeLinkedRule;
        assert!(matches!(rule.check(&graph).unwrap(), RuleResult::Pass));
    }

    #[test]
    fn no_empty_payload_warns_for_empty_nodes() {
        let mut graph = Graph::open_in_memory().unwrap();
        let task = Node::new(NodeKind::Task, "empty-task", serde_json::json!({}));
        graph.create_node(&task).unwrap();

        let rule = NoEmptyPayloadRule;
        let result = rule.check(&graph).unwrap();
        assert!(
            matches!(&result, RuleResult::Warn { reason } if reason.contains("empty-task")),
            "expected warn for empty payload, got {:?}",
            result
        );
    }

    #[test]
    fn no_empty_payload_passes_for_indexed_code() {
        let mut graph = Graph::open_in_memory().unwrap();
        graph.index_code("src/lib.rs", "pub struct Graph;").unwrap();

        let rule = NoEmptyPayloadRule;
        assert!(matches!(rule.check(&graph).unwrap(), RuleResult::Pass));
    }

    #[test]
    fn new_rule_requires_approval() {
        let mut graph = Graph::open_in_memory().unwrap();
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(EdgeIntegrityRule)];
        let unapproved = upsert_rules(&mut graph, &rules).unwrap();
        assert_eq!(unapproved, vec!["edge_integrity"]);
    }

    #[test]
    fn approving_rule_allows_it_to_run() {
        let mut graph = Graph::open_in_memory().unwrap();
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(EdgeIntegrityRule)];

        // First upsert flags it as unapproved.
        let unapproved = upsert_rules(&mut graph, &rules).unwrap();
        assert_eq!(unapproved, vec!["edge_integrity"]);

        // Approve it.
        assert!(approve_rule(&mut graph, "edge_integrity").unwrap());

        // Second upsert finds it approved.
        let unapproved_after = upsert_rules(&mut graph, &rules).unwrap();
        assert!(unapproved_after.is_empty());
    }

    #[test]
    fn approve_rule_returns_false_for_unknown_rule() {
        let mut graph = Graph::open_in_memory().unwrap();
        assert!(!approve_rule(&mut graph, "nonexistent").unwrap());
    }
}
