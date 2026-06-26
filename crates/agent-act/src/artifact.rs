//! 大输出外置（P-4/§8，借鉴 oh-my-pi blob/artifact）。
//!
//! 大工具输出不塞进历史：存到 [`ArtifactStore`]（落盘），历史只留 `artifact://id` 短引用 +
//! 提示，模型按需 `read(artifact://id)` 取回（无损，优于 prune 的有损截断）。
//! Blob 产物另走内容寻址 `blob:sha256:<hex>`，用于二进制/图片与跨引用去重。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use base_types::{Tool, ToolResult, ToolTier};
use serde_json::{Value, json};

use crate::resource::{Resource, ResourceDoc};

/// 内容寻址 blob 引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub uri: String,
    pub sha256: String,
    pub bytes: usize,
    pub content_type: String,
}

#[derive(Debug, Clone)]
pub struct BlobDoc {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlobMeta {
    sha256: String,
    bytes: usize,
    content_type: String,
}

/// 落盘的工件存储：`artifact://aN` 保持文本短 id，`blob:sha256:<hex>` 提供内容寻址二进制。
pub struct ArtifactStore {
    dir: PathBuf,
    counter: AtomicU64,
}

impl ArtifactStore {
    /// 在 `dir` 建库（自动创建目录）。
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            counter: AtomicU64::new(0),
        })
    }

    // 文本工件落 `<root>/text/`（§2.9：root 已是 `.bot/artifacts`，终态 `.bot/artifacts/text/`；
    // 修掉旧 `join("artifacts")` 在 root=`.bot/artifacts` 时的 `artifacts/artifacts/` 双层 wart）。
    fn artifact_dir(&self) -> PathBuf {
        self.dir.join("text")
    }

    fn blob_dir(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    fn path(&self, id: &str) -> PathBuf {
        self.artifact_dir().join(format!("{id}.txt"))
    }

    /// 旧扁平位置 `<root>/aN.txt`（最早布局），仅读兼容。
    fn legacy_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.txt"))
    }

    /// 过渡期双层位置 `<root>/artifacts/aN.txt`（§2.9 修复前的 wart），仅读兼容。
    fn legacy_nested_path(&self, id: &str) -> PathBuf {
        self.dir.join("artifacts").join(format!("{id}.txt"))
    }

    fn blob_path(&self, sha: &str) -> PathBuf {
        self.blob_dir().join(format!("{sha}.bin"))
    }

    fn blob_meta_path(&self, sha: &str) -> PathBuf {
        self.blob_dir().join(format!("{sha}.json"))
    }

    /// 存一段文本，返回其 id。
    pub fn put_text(&self, text: &str) -> std::io::Result<String> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("a{n}");
        std::fs::create_dir_all(self.artifact_dir())?;
        std::fs::write(self.path(&id), text)?;
        Ok(id)
    }

    /// 按 id 取回文本。
    pub fn get_text(&self, id: &str) -> std::io::Result<String> {
        validate_artifact_id(id)?;
        match std::fs::read_to_string(self.path(id)) {
            Ok(text) => Ok(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // 回退旧布局：扁平 `<root>/aN.txt` → 过渡双层 `<root>/artifacts/aN.txt`。
                std::fs::read_to_string(self.legacy_path(id))
                    .or_else(|_| std::fs::read_to_string(self.legacy_nested_path(id)))
            }
            Err(err) => Err(err),
        }
    }

    /// 列出文本工件 `(id, 字节数, head 预览)`，按 id 排序。供 `artifact_list` 在上下文压缩后
    /// 找回被 spill 的大输出（其 `artifact://id` 可能随历史折叠丢失）。`head`=首 ~120 字符。
    pub fn list_text(&self) -> Vec<(String, usize, String)> {
        let dir = self.artifact_dir();
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("txt") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let head: String = text
                    .chars()
                    .take(120)
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                out.push((id.to_string(), text.len(), head.trim().to_string()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 存二进制 blob，按 sha256 内容寻址并全局去重。
    pub fn put_blob(
        &self,
        bytes: &[u8],
        content_type: impl Into<String>,
    ) -> std::io::Result<BlobRef> {
        let sha = sha256_hex(bytes);
        let content_type = content_type.into();
        std::fs::create_dir_all(self.blob_dir())?;
        let path = self.blob_path(&sha);
        if !path.exists() {
            std::fs::write(&path, bytes)?;
        }
        let meta_path = self.blob_meta_path(&sha);
        if !meta_path.exists() {
            let meta = BlobMeta {
                sha256: sha.clone(),
                bytes: bytes.len(),
                content_type: content_type.clone(),
            };
            let data = serde_json::to_vec_pretty(&meta).map_err(invalid_json)?;
            std::fs::write(meta_path, data)?;
        }
        Ok(BlobRef {
            uri: format!("blob:sha256:{sha}"),
            sha256: sha,
            bytes: bytes.len(),
            content_type,
        })
    }

    /// 按 sha256 取回 blob bytes。
    pub fn get_blob(&self, sha: &str) -> std::io::Result<BlobDoc> {
        validate_sha256(sha)?;
        let bytes = std::fs::read(self.blob_path(sha))?;
        let content_type = match std::fs::read(self.blob_meta_path(sha)) {
            Ok(meta) => serde_json::from_slice::<BlobMeta>(&meta)
                .map(|m| m.content_type)
                .unwrap_or_else(|_| "application/octet-stream".into()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                "application/octet-stream".into()
            }
            Err(err) => return Err(err),
        };
        Ok(BlobDoc {
            bytes,
            content_type,
        })
    }
}

/// §2.9 ④ 孤儿 GC 报告：扫描数 / 判定为孤儿的 id/sha / 释放字节 / 是否实删（`applied=false`=dry-run）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub scanned_text: usize,
    pub scanned_blobs: usize,
    pub orphan_text: Vec<String>,
    pub orphan_blobs: Vec<String>,
    pub freed_bytes: u64,
    pub applied: bool,
}

/// §2.9 ④：从任意文本里收集 artifact/blob 引用（扫活会话 messages 用）。
/// `artifact://<id>`（id=字母/数字/`_`/`-`）→ `ids`；`blob:sha256:<64 hex>` → `shas`。
pub fn collect_refs(
    hay: &str,
    ids: &mut std::collections::HashSet<String>,
    shas: &mut std::collections::HashSet<String>,
) {
    scan_after(
        hay,
        "artifact://",
        |c| c.is_ascii_alphanumeric() || c == '_' || c == '-',
        ids,
    );
    let mut raw = std::collections::HashSet::new();
    scan_after(hay, "blob:sha256:", |c| c.is_ascii_hexdigit(), &mut raw);
    for s in raw {
        // 只接受恰好 64 hex 的前缀（防把后续字符并入）；scan 已按 hex 截断，这里取前 64。
        if s.len() >= 64 {
            shas.insert(s[..64].to_ascii_lowercase());
        }
    }
}

/// 在 `hay` 中找所有 `prefix` 出现处，收集其后连续满足 `is_tok` 的 token。
fn scan_after(
    hay: &str,
    prefix: &str,
    is_tok: fn(char) -> bool,
    out: &mut std::collections::HashSet<String>,
) {
    let mut start = 0usize;
    while let Some(p) = hay[start..].find(prefix) {
        let idx = start + p + prefix.len();
        let tok: String = hay[idx..].chars().take_while(|c| is_tok(*c)).collect();
        if !tok.is_empty() {
            out.insert(tok);
        }
        start = idx.max(start + p + 1); // 至少前进一格防死循环
    }
}

impl ArtifactStore {
    /// §2.9 ④ 孤儿 GC（mark-sweep）：删除**未被任何引用集命中**的工件文件。
    /// 调用方先扫活会话收 `ref_ids`/`ref_shas`（见 `collect_refs`），本方法做 sweep。
    /// **保守安全**：① `apply=false` 仅报告不删（dry-run）；② `grace_secs` 宽限——修改时间在此秒数内
    /// 的文件**跳过**（可能是在途 run 尚未持久化引用的新工件，防误删）；③ 取不到 mtime 的文件**不删**。
    /// 只扫终态布局 `text/*.txt` 与 `blobs/*.bin`（旧扁平/双层兼容位置不参与 GC，避免误判）。
    pub fn sweep_orphans(
        &self,
        ref_ids: &std::collections::HashSet<String>,
        ref_shas: &std::collections::HashSet<String>,
        grace_secs: u64,
        apply: bool,
    ) -> std::io::Result<GcReport> {
        let now = std::time::SystemTime::now();
        let mut report = GcReport {
            applied: apply,
            ..Default::default()
        };
        let old_enough = |path: &std::path::Path| -> bool {
            match std::fs::metadata(path).and_then(|m| m.modified()) {
                Ok(mtime) => now
                    .duration_since(mtime)
                    .map(|d| d.as_secs() >= grace_secs)
                    .unwrap_or(false), // 未来时间戳 → 保守不删
                Err(_) => false, // 取不到 mtime → 保守不删
            }
        };
        // 文本工件。
        let tdir = self.artifact_dir();
        if tdir.exists() {
            for entry in std::fs::read_dir(&tdir)? {
                let path = entry?.path();
                if path.extension().and_then(|x| x.to_str()) != Some("txt") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                report.scanned_text += 1;
                if ref_ids.contains(stem) || !old_enough(&path) {
                    continue;
                }
                let sz = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                report.orphan_text.push(stem.to_string());
                report.freed_bytes += sz;
                if apply {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        // blob（.bin + .json 边车）。
        let bdir = self.blob_dir();
        if bdir.exists() {
            for entry in std::fs::read_dir(&bdir)? {
                let path = entry?.path();
                if path.extension().and_then(|x| x.to_str()) != Some("bin") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                report.scanned_blobs += 1;
                if ref_shas.contains(&stem.to_ascii_lowercase()) || !old_enough(&path) {
                    continue;
                }
                let sz = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                report.orphan_blobs.push(stem.to_string());
                report.freed_bytes += sz;
                if apply {
                    std::fs::remove_file(&path)?;
                    let _ = std::fs::remove_file(self.blob_meta_path(stem));
                }
            }
        }
        Ok(report)
    }
}

fn invalid_json(err: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// 校验文本工件 id（内部生成形如 `aN`）：只允许 ascii 字母数字，挡掉 `..`/路径分隔符等——
/// 与 `validate_sha256` 同等防 `read(artifact://../../x)` 越出工件目录（虽本机模型另有 file://，
/// 仍保一致性 + 防意外路径穿越）。
fn validate_artifact_id(id: &str) -> std::io::Result<()> {
    let valid = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid artifact id: {id}"),
        ))
    }
}

fn validate_sha256(sha: &str) -> std::io::Result<()> {
    let valid = sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid sha256 blob id: {sha}"),
        ))
    }
}

/// `artifact://id` 资源 handler：取回外置的工具输出（immutable）。
pub struct ArtifactResource {
    store: Arc<ArtifactStore>,
}

impl ArtifactResource {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Resource for ArtifactResource {
    fn scheme(&self) -> &str {
        "artifact"
    }
    fn immutable(&self) -> bool {
        true
    }
    async fn resolve(&self, id: &str) -> anyhow::Result<ResourceDoc> {
        let content = self
            .store
            .get_text(id)
            .map_err(|e| anyhow::anyhow!("artifact {id} 取回失败: {e}"))?;
        Ok(ResourceDoc {
            url: format!("artifact://{id}"),
            content,
            content_type: "text/plain",
            immutable: true,
        })
    }
}

/// `artifact_list` 工具（读）：**枚举**已落盘文本工件 `(id, bytes, head)`。补盲区——大输出 spill 成
/// `artifact://id` 后，其 id 在上下文压缩时可能随历史折叠丢失；此工具让 agent 找回再 `read(artifact://id)`。
pub struct ArtifactListTool {
    store: Arc<ArtifactStore>,
}

impl ArtifactListTool {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ArtifactListTool {
    fn name(&self) -> &str {
        "artifact_list"
    }
    fn description(&self) -> &str {
        "List spilled text artifacts (id, byte size, head preview). Use to recover an artifact:// id \
         lost after context compaction, then read(artifact://<id>) for the full content."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> ToolResult {
        let items: Vec<Value> = self
            .store
            .list_text()
            .into_iter()
            .map(|(id, bytes, head)| json!({ "id": id, "uri": format!("artifact://{id}"), "bytes": bytes, "head": head }))
            .collect();
        Ok(json!({ "artifacts": items }))
    }
}

/// `blob:sha256:<hex>` 资源 handler：回读内容寻址二进制。
///
/// UTF-8 文本按原文返回；非 UTF-8 bytes 以 base64 文本返回，`content_type` 保持写入时类型。
pub struct BlobResource {
    store: Arc<ArtifactStore>,
}

impl BlobResource {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Resource for BlobResource {
    fn scheme(&self) -> &str {
        "blob"
    }
    fn immutable(&self) -> bool {
        true
    }
    async fn resolve(&self, rest: &str) -> anyhow::Result<ResourceDoc> {
        let sha = rest
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("blob URL must be blob:sha256:<hex>"))?;
        let doc = self
            .store
            .get_blob(sha)
            .map_err(|e| anyhow::anyhow!("blob {sha} 取回失败: {e}"))?;
        let content_type = blob_content_type(&doc.content_type);
        let content = String::from_utf8(doc.bytes.clone())
            .unwrap_or_else(|_| base64::engine::general_purpose::STANDARD.encode(&doc.bytes));
        Ok(ResourceDoc {
            url: format!("blob:sha256:{sha}"),
            content,
            content_type,
            immutable: true,
        })
    }
}

fn blob_content_type(raw: &str) -> &'static str {
    match raw
        .to_ascii_lowercase()
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
    {
        "text/plain" => "text/plain",
        "text/markdown" => "text/markdown",
        "application/json" => "application/json",
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_lands_in_text_subdir_not_double_artifacts() {
        let dir = std::env::temp_dir().join(format!("botobot-art-layout-{}", uuid::Uuid::new_v4()));
        let store = ArtifactStore::new(&dir).unwrap();
        let id = store.put_text("X").unwrap();
        // §2.9：落 <root>/text/aN.txt，不再 <root>/artifacts/aN.txt（root=.bot/artifacts 时的双层 wart）
        assert!(dir.join("text").join(format!("{id}.txt")).exists());
        assert!(!dir.join("artifacts").join(format!("{id}.txt")).exists());
        // 旧双层位置仍可读（兼容）
        std::fs::create_dir_all(dir.join("artifacts")).unwrap();
        std::fs::write(dir.join("artifacts").join("a99.txt"), "LEGACY").unwrap();
        assert_eq!(store.get_text("a99").unwrap(), "LEGACY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_get_roundtrip_via_resource() {
        let dir = std::env::temp_dir().join("botobot-artifact-test");
        let store = Arc::new(ArtifactStore::new(&dir).unwrap());
        let id = store.put_text("BIG CONTENT").unwrap();
        assert_eq!(store.get_text(&id).unwrap(), "BIG CONTENT");

        let res = ArtifactResource::new(store);
        let doc = res.resolve(&id).await.unwrap();
        assert_eq!(doc.content, "BIG CONTENT");
        assert!(res.resolve("nope").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // artifact_list 枚举已落盘文本工件（id + bytes + head），供压缩后找回 id。
    #[tokio::test]
    async fn artifact_list_enumerates_spilled_text() {
        let dir = std::env::temp_dir().join(format!("botobot-art-list-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(ArtifactStore::new(&dir).unwrap());
        let id1 = store.put_text("first artifact body").unwrap();
        let id2 = store
            .put_text("second longer artifact content here")
            .unwrap();
        let out = ArtifactListTool::new(store)
            .call(serde_json::json!({}))
            .await
            .unwrap();
        let arts = out["artifacts"].as_array().unwrap();
        assert_eq!(arts.len(), 2);
        assert!(
            arts.iter()
                .any(|a| a["id"] == id1 && a["uri"] == format!("artifact://{id1}"))
        );
        assert!(
            arts.iter()
                .any(|a| a["id"] == id2 && a["head"].as_str().unwrap().contains("second"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 路径穿越：非字母数字 id（含 ../、分隔符）被拒，不越出工件目录。
    #[test]
    fn get_text_rejects_traversal_ids() {
        let dir = std::env::temp_dir().join(format!("botobot-art-trav-{}", uuid::Uuid::new_v4()));
        let store = ArtifactStore::new(&dir).unwrap();
        for bad in ["../secret", "..\\secret", "a/b", "a.b", "", "a 1"] {
            assert!(store.get_text(bad).is_err(), "应拒绝非法 id: {bad:?}");
        }
        // 合法 id 仍正常。
        let id = store.put_text("ok").unwrap();
        assert_eq!(store.get_text(&id).unwrap(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn blob_store_dedupes_by_sha256_and_reads_text() {
        let dir = std::env::temp_dir().join("botobot-blob-text-test");
        let store = Arc::new(ArtifactStore::new(&dir).unwrap());
        let a = store.put_blob(b"hello blob", "text/plain").unwrap();
        let b = store.put_blob(b"hello blob", "text/plain").unwrap();
        assert_eq!(a.uri, b.uri);
        assert_eq!(a.bytes, 10);

        let res = BlobResource::new(store);
        let doc = res
            .resolve(a.uri.strip_prefix("blob:").unwrap())
            .await
            .unwrap();
        assert_eq!(doc.url, a.uri);
        assert_eq!(doc.content, "hello blob");
        assert_eq!(doc.content_type, "text/plain");
        assert!(doc.immutable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_refs_extracts_artifact_and_blob() {
        use std::collections::HashSet;
        let mut ids = HashSet::new();
        let mut shas = HashSet::new();
        let sha = "a".repeat(64);
        let hay = format!(
            "see artifact://a3 and artifact://a12_x, image blob:sha256:{sha} done. \
             not-a-ref artifactX nope"
        );
        collect_refs(&hay, &mut ids, &mut shas);
        assert!(ids.contains("a3"));
        assert!(ids.contains("a12_x"));
        assert!(shas.contains(&sha));
        assert_eq!(ids.len(), 2);
        assert_eq!(shas.len(), 1);
    }

    #[test]
    fn sweep_keeps_referenced_and_fresh_removes_old_orphans() {
        use std::collections::HashSet;
        let dir = std::env::temp_dir().join(format!("botobot-gc-{}", uuid::Uuid::new_v4()));
        let store = ArtifactStore::new(&dir).unwrap();
        let keep = store.put_text("REFERENCED").unwrap();
        let orphan = store.put_text("ORPHAN").unwrap();
        let blob = store.put_blob(b"orphan blob", "text/plain").unwrap();
        let sha = blob.sha256.clone();

        let mut ref_ids = HashSet::new();
        ref_ids.insert(keep.clone()); // 只引用 keep，不引用 orphan/blob
        let ref_shas = HashSet::new();

        // grace=0：刚写的也算"够老"。dry-run 先报告不删。
        let dry = store.sweep_orphans(&ref_ids, &ref_shas, 0, false).unwrap();
        assert!(!dry.applied);
        assert!(
            dry.orphan_text.contains(&orphan),
            "orphan 应入候选: {dry:?}"
        );
        assert!(!dry.orphan_text.contains(&keep), "referenced 不应入候选");
        assert!(dry.orphan_blobs.contains(&sha));
        // dry-run 未真删。
        assert!(store.get_text(&orphan).is_ok(), "dry-run 不应删除");

        // grace 极大：所有文件都"太新"被跳过（保护在途）。
        let protected = store
            .sweep_orphans(&ref_ids, &ref_shas, 86_400, true)
            .unwrap();
        assert!(
            protected.orphan_text.is_empty(),
            "宽限期内应全跳过: {protected:?}"
        );
        assert!(store.get_text(&orphan).is_ok());

        // apply + grace=0：真删 orphan + blob，保留 referenced。
        let applied = store.sweep_orphans(&ref_ids, &ref_shas, 0, true).unwrap();
        assert!(applied.applied);
        assert!(applied.freed_bytes > 0);
        assert_eq!(
            store.get_text(&keep).unwrap(),
            "REFERENCED",
            "referenced 必须保留"
        );
        assert!(store.get_text(&orphan).is_err(), "orphan 应被删除");
        assert!(store.get_blob(&sha).is_err(), "orphan blob 应被删除");
        assert!(
            !dir.join("blobs").join(format!("{sha}.json")).exists(),
            "blob 边车也删"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn blob_resource_returns_base64_for_binary_image() {
        let dir = std::env::temp_dir().join("botobot-blob-image-test");
        let store = Arc::new(ArtifactStore::new(&dir).unwrap());
        let blob = store
            .put_blob(&[0, 159, 146, 150, 255], "image/png")
            .unwrap();

        let res = BlobResource::new(store);
        let doc = res
            .resolve(blob.uri.strip_prefix("blob:").unwrap())
            .await
            .unwrap();
        assert_eq!(doc.content_type, "image/png");
        assert_eq!(doc.content, "AJ+Slv8=");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
