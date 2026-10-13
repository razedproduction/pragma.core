//! Экспорт графа для отладки: Graphviz DOT и Mermaid.

use std::fmt::Write as _;

use crate::graph::{FrameGraph, Node};

/// Имя типа без путей модулей: `game::physics::Bodies` → `Bodies`,
/// `Vec<game::Item>` → `Vec<Item>`.
fn short_type_name(full: &str) -> String {
    fn flush(token: &mut String, out: &mut String) {
        out.push_str(token.rsplit("::").next().unwrap_or(token));
        token.clear();
    }
    let mut out = String::with_capacity(full.len());
    let mut token = String::new();
    for c in full.chars() {
        if c.is_alphanumeric() || c == '_' || c == ':' {
            token.push(c);
        } else {
            flush(&mut token, &mut out);
            out.push(c);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// Строки подписи задачи: имя, читаемые и записываемые ресурсы.
fn label_lines(node: &Node) -> Vec<String> {
    let list = |write: bool| {
        node.access
            .iter()
            .filter(|a| a.write == write)
            .map(|a| short_type_name(a.key.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![node.name.clone()];
    let reads = list(false);
    if !reads.is_empty() {
        lines.push(format!("R: {reads}"));
    }
    let writes = list(true);
    if !writes.is_empty() {
        lines.push(format!("W: {writes}"));
    }
    lines
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn mermaid_escape(s: &str) -> String {
    s.replace('"', "#quot;")
        .replace('<', "#lt;")
        .replace('>', "#gt;")
}

impl FrameGraph {
    /// Граф в формате Graphviz DOT (`dot -Tsvg graph.dot`): задачи с объявленным доступом
    /// и рёбра после транзитивной редукции.
    #[must_use]
    pub fn to_dot(&self) -> String {
        let mut out = String::from("digraph frame {\n    rankdir=LR;\n    node [shape=box];\n");
        for (i, node) in self.nodes.iter().enumerate() {
            let label = label_lines(node)
                .iter()
                .map(|l| dot_escape(l))
                .collect::<Vec<_>>()
                .join("\\n");
            let _ = writeln!(out, "    t{i} [label=\"{label}\"];");
        }
        for (i, node) in self.nodes.iter().enumerate() {
            for s in &node.successors {
                let _ = writeln!(out, "    t{i} -> t{s};");
            }
        }
        out.push_str("}\n");
        out
    }

    /// Граф в формате Mermaid (`flowchart LR`), например для вставки в Markdown.
    #[must_use]
    pub fn to_mermaid(&self) -> String {
        let mut out = String::from("flowchart LR\n");
        for (i, node) in self.nodes.iter().enumerate() {
            let label = label_lines(node)
                .iter()
                .map(|l| mermaid_escape(l))
                .collect::<Vec<_>>()
                .join("<br/>");
            let _ = writeln!(out, "    t{i}[\"{label}\"]");
        }
        for (i, node) in self.nodes.iter().enumerate() {
            for s in &node.successors {
                let _ = writeln!(out, "    t{i} --> t{s}");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::short_type_name;

    #[test]
    fn short_names() {
        assert_eq!(short_type_name("game::physics::Bodies"), "Bodies");
        assert_eq!(short_type_name("u32"), "u32");
        assert_eq!(short_type_name("alloc::vec::Vec<game::Item>"), "Vec<Item>");
        assert_eq!(
            short_type_name("std::collections::HashMap<a::K, b::V>"),
            "HashMap<K, V>"
        );
    }
}
