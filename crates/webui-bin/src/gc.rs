//! §2.9 ④ artifacts 孤儿 GC 入口（`bots gc [--apply]`）。
//!
//! mark-sweep：扫 `.bot/sessions/**` 下所有会话文件收集 `artifact://` / `blob:sha256:` 引用（mark），
//! 再对 `.bot/artifacts` 删除未被引用的工件（sweep）。**保守安全**：默认 dry-run（只报告），
//! 仅 `--apply` 实删；且对**修改时间在宽限期内**的新文件跳过（防误删在途 run 尚未持久化引用的工件）。

use std::collections::HashSet;
use std::path::Path;

use agent_act::artifact::{ArtifactStore, GcReport, collect_refs};

/// 宽限秒数：修改时间在此之内的工件视为「可能在途」，GC 跳过（默认 1 小时）。
/// `BOTOBOT_GC_GRACE_SECS` 可覆盖。
fn grace_secs() -> u64 {
    std::env::var("BOTOBOT_GC_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
}

/// 运行 GC。`apply=false` 仅报告（dry-run）；`apply=true` 实删孤儿。root 为 `.bot`。
pub fn run_gc(root: &str, apply: bool) -> anyhow::Result<()> {
    let sessions_dir = Path::new(root).join("sessions");
    let artifacts_dir = Path::new(root).join("artifacts");

    // 1) mark：扫活会话所有文件收集引用。
    let mut ids = HashSet::new();
    let mut shas = HashSet::new();
    let mut files_scanned = 0usize;
    collect_refs_in_dir(&sessions_dir, &mut ids, &mut shas, &mut files_scanned);

    // 2) sweep。
    let store = ArtifactStore::new(&artifacts_dir)?;
    let report = store.sweep_orphans(&ids, &shas, grace_secs(), apply)?;
    print_report(files_scanned, ids.len(), shas.len(), &report, apply);
    Ok(())
}

/// 递归扫目录下所有常规文件，把文本内容里的引用收进 `ids`/`shas`。
/// 非 UTF-8 / 读失败的文件跳过（不影响其它文件的 mark）。
fn collect_refs_in_dir(
    dir: &Path,
    ids: &mut HashSet<String>,
    shas: &mut HashSet<String>,
    files_scanned: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_refs_in_dir(&path, ids, shas, files_scanned);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            collect_refs(&text, ids, shas);
            *files_scanned += 1;
        }
    }
}

fn print_report(files: usize, ref_ids: usize, ref_shas: usize, r: &GcReport, apply: bool) {
    let mode = if apply {
        "APPLY（已删除）"
    } else {
        "DRY-RUN（仅报告，加 --apply 实删）"
    };
    println!("artifacts GC · {mode}");
    println!("  扫描会话文件 {files} 个 → 引用 artifact={ref_ids} / blob={ref_shas}");
    println!(
        "  工件扫描 text={} / blob={}；判定孤儿 text={} / blob={}；可释放 {:.1} KB",
        r.scanned_text,
        r.scanned_blobs,
        r.orphan_text.len(),
        r.orphan_blobs.len(),
        r.freed_bytes as f64 / 1024.0
    );
    if !r.orphan_text.is_empty() {
        println!("  孤儿 text: {}", r.orphan_text.join(", "));
    }
    if !r.orphan_blobs.is_empty() {
        let short: Vec<String> = r
            .orphan_blobs
            .iter()
            .map(|s| s[..s.len().min(12)].to_string())
            .collect();
        println!("  孤儿 blob: {}…", short.join("…, "));
    }
    if !apply && (!r.orphan_text.is_empty() || !r.orphan_blobs.is_empty()) {
        println!("  → 确认无误后运行 `bots gc --apply` 实删（宽限期内的新工件已自动跳过）。");
    }
}
