//! §4.6 步② 下半：在一条 [`CdpConnection`] 上开页 + 导航（薄封装 CDP `Page.*`/`Target.*`）。
//! 移植 `.oni/agent-browser` 的 navigate 序列，精简为 botobot 所需最小集。
//!
//! **feature-gated**（`browser`）。编译验证；**运行验证待真 Chrome**（命令实发实收需活连接）。

use serde_json::{Value, json};

use super::connect::CdpConnection;
use super::launch;
use super::snapshot::{AxSnapshot, render_ax_tree};

/// 一个浏览器会话：自持 Chrome 子进程 + CDP 连接。drop 时杀子进程。
pub struct Browser {
    _child: tokio::process::Child,
    conn: CdpConnection,
}

impl Browser {
    /// 启动 Chrome（headless）+ 连上 CDP。需真 Chrome（运行验证待环境）。
    pub async fn launch_headless(port: u16) -> Result<Self, String> {
        let (child, ws) = launch::launch(port).await?;
        let conn = CdpConnection::connect(&ws).await?;
        Ok(Self {
            _child: child,
            conn,
        })
    }

    /// 底层连接（发任意 CDP 命令 / 订阅事件）。
    pub fn conn(&self) -> &CdpConnection {
        &self.conn
    }

    /// 导航到 `url`：先 `Page.enable`（开页面事件域），再 `Page.navigate`。
    /// 返回 `frameId`（成功）。`session_id`=flat-session 目标（单页时 None）。
    pub async fn navigate(&self, url: &str, session_id: Option<&str>) -> Result<String, String> {
        self.conn.send("Page.enable", json!({}), session_id).await?;
        let res: Value = self
            .conn
            .send("Page.navigate", json!({ "url": url }), session_id)
            .await?;
        // 失败时 CDP 在 result 里给 errorText；优先报它。
        if let Some(err) = res.get("errorText").and_then(|e| e.as_str()) {
            if !err.is_empty() {
                return Err(format!("navigate error: {err}"));
            }
        }
        Ok(res
            .get("frameId")
            .and_then(|f| f.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// 取当前页 AX 快照（`Accessibility.enable` + `getFullAXTree`）→ 缩进文本 + RefMap
    /// （§4.6 步③）。文本喂模型「看」页面，ref 供后续 click/fill 定位。
    pub async fn snapshot(&self, session_id: Option<&str>) -> Result<AxSnapshot, String> {
        self.conn
            .send("Accessibility.enable", json!({}), session_id)
            .await?;
        let res = self
            .conn
            .send("Accessibility.getFullAXTree", json!({}), session_id)
            .await?;
        Ok(render_ax_tree(&res))
    }

    /// §4.6 步③：点击 snapshot 里的某 `ref`（如 `e1`）——解析 backendNodeId → `DOM.getBoxModel`
    /// → quad 中心 → 真坐标点击。ref 失效（页面变了）则报错让调用方重新 snapshot。
    pub async fn click_ref(
        &self,
        snap: &AxSnapshot,
        ref_id: &str,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        let backend = snap
            .refs
            .get(ref_id)
            .ok_or_else(|| format!("unknown ref: {ref_id}（请重新 snapshot）"))?;
        let backend_id: i64 = backend
            .parse()
            .map_err(|_| "bad backendNodeId".to_string())?;
        let box_model = self
            .conn
            .send(
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend_id }),
                session_id,
            )
            .await?;
        let quad: Vec<f64> = box_model
            .get("model")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
            .unwrap_or_default();
        let (x, y) = super::interaction::quad_center(&quad)
            .ok_or_else(|| "element 无有效 box（不可见？）".to_string())?;
        self.click_at(x, y, session_id).await
    }
}
