//! §4.6 步③上半：把 CDP `Accessibility.getFullAXTree` 结果渲染成**缩进文本树 + RefMap**
//! （移植 `.oni/agent-browser` snapshot 的核心：可交互节点编 ref 供后续 click/fill 定位；
//! 精简掉 iframe 递归 / cursor 元素 / 去重 nth——那些属步③下半与边界增强）。
//!
//! **纯函数可单测**（喂 AX JSON 即出文本 + ref 映射，不需 Chrome）。本模块不 feature-gate——
//! 解析逻辑零网络依赖；真正取 AX 树（发 CDP 命令）才需 `browser` feature + 真 Chrome。

use std::collections::HashMap;

use serde_json::Value;

/// 可交互角色（编 ref，供 click/fill 定位）。
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "checkbox",
    "radio",
    "combobox",
    "menuitem",
    "tab",
    "switch",
    "slider",
    "searchbox",
    "option",
];
/// 有内容（有 name）才值得显示的内容角色。
const CONTENT_ROLES: &[&str] = &["heading", "StaticText", "paragraph", "listitem", "cell"];

/// 一次 AX 快照：缩进文本 + `ref_id → backendDOMNodeId` 映射（供 element 经 `DOM.getBoxModel`
/// 解析坐标后点击/填值；存 backendDOMNodeId 而非 AX nodeId，因交互链需它）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AxSnapshot {
    pub text: String,
    pub refs: HashMap<String, String>,
}

/// 把 `Accessibility.getFullAXTree` 的 `result` 渲染成 [`AxSnapshot`]。
/// 节点形如 `{nodeId, role:{value}, name:{value}, childIds:[], ignored}`；忽略 `ignored` 节点。
pub fn render_ax_tree(result: &Value) -> AxSnapshot {
    let Some(nodes) = result.get("nodes").and_then(|n| n.as_array()) else {
        return AxSnapshot::default();
    };
    // 索引 nodeId → node；并记录被引用的子集以找根（无人指向的）。
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    let mut is_child: HashMap<&str, bool> = HashMap::new();
    for n in nodes {
        if let Some(id) = n.get("nodeId").and_then(|v| v.as_str()) {
            by_id.insert(id, n);
        }
    }
    for n in nodes {
        for c in n
            .get("childIds")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(cid) = c.as_str() {
                is_child.insert(cid, true);
            }
        }
    }
    // 根 = 第一个非子节点（CDP 一般首个即根）。
    let root = nodes
        .iter()
        .find_map(|n| n.get("nodeId").and_then(|v| v.as_str()))
        .filter(|id| !is_child.get(id).copied().unwrap_or(false))
        .or_else(|| {
            nodes
                .first()
                .and_then(|n| n.get("nodeId"))
                .and_then(|v| v.as_str())
        });

    let mut snap = AxSnapshot::default();
    let mut ref_seq = 0usize;
    if let Some(root_id) = root {
        walk(root_id, 0, &by_id, &mut snap, &mut ref_seq);
    }
    snap
}

fn walk(
    id: &str,
    depth: usize,
    by_id: &HashMap<&str, &Value>,
    snap: &mut AxSnapshot,
    ref_seq: &mut usize,
) {
    let Some(node) = by_id.get(id) else { return };
    let ignored = node
        .get("ignored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let role = node
        .get("role")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = node
        .get("name")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let interactive = INTERACTIVE_ROLES.contains(&role);
    let content = CONTENT_ROLES.contains(&role) && !name.is_empty();
    let show = !ignored && (interactive || content);
    let child_depth = if show {
        let indent = "  ".repeat(depth);
        let mut line = format!("{indent}{role}");
        if !name.is_empty() {
            line.push_str(&format!(" \"{name}\""));
        }
        if interactive {
            *ref_seq += 1;
            let ref_id = format!("e{}", *ref_seq);
            // 存 backendDOMNodeId（交互链 DOM.getBoxModel 需它）；缺失则退回 ax nodeId。
            let backend = node
                .get("backendDOMNodeId")
                .and_then(|b| b.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| id.to_string());
            snap.refs.insert(ref_id.clone(), backend);
            line.push_str(&format!(" [ref={ref_id}]"));
        }
        snap.text.push_str(&line);
        snap.text.push('\n');
        depth + 1
    } else {
        depth // 不显示的节点不增加缩进层级（扁平化无名容器）
    };

    for c in node
        .get("childIds")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(cid) = c.as_str() {
            walk(cid, child_depth, by_id, snap, ref_seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_interactive_with_refs_and_skips_ignored() {
        // root(WebArea) → heading"Hi" + button"Submit" + ignored generic + link"Docs"
        let result = json!({
            "nodes": [
                { "nodeId": "1", "role": {"value": "WebArea"}, "name": {"value": ""}, "childIds": ["2","3","4","5"] },
                { "nodeId": "2", "role": {"value": "heading"}, "name": {"value": "Hi"}, "childIds": [] },
                { "nodeId": "3", "role": {"value": "button"}, "name": {"value": "Submit"}, "backendDOMNodeId": 301, "childIds": [] },
                { "nodeId": "4", "role": {"value": "generic"}, "name": {"value": ""}, "ignored": true, "childIds": [] },
                { "nodeId": "5", "role": {"value": "link"}, "name": {"value": "Docs"}, "backendDOMNodeId": 501, "childIds": [] }
            ]
        });
        let snap = render_ax_tree(&result);
        // 交互节点编 ref。
        assert!(
            snap.text.contains("button \"Submit\" [ref=e1]"),
            "got:\n{}",
            snap.text
        );
        assert!(
            snap.text.contains("link \"Docs\" [ref=e2]"),
            "got:\n{}",
            snap.text
        );
        // 内容角色显示但无 ref。
        assert!(snap.text.contains("heading \"Hi\""));
        assert!(!snap.text.contains("heading \"Hi\" [ref"));
        // ignored / 无名 generic 不出现。
        assert!(!snap.text.contains("generic"));
        // ref 映射回 backendDOMNodeId（供 DOM.getBoxModel）。
        assert_eq!(snap.refs.get("e1").map(String::as_str), Some("301"));
        assert_eq!(snap.refs.get("e2").map(String::as_str), Some("501"));
    }

    #[test]
    fn empty_or_malformed_yields_empty_snapshot() {
        assert_eq!(render_ax_tree(&json!({})), AxSnapshot::default());
        assert_eq!(
            render_ax_tree(&json!({ "nodes": [] })),
            AxSnapshot::default()
        );
    }
}
