//! §1.8.6 A — 记忆召回 eval 套件（「eval 先行」纪律：结构化召回打分/图扩散动手前先有它）。
//!
//! 用**真 bge-small-zh** 嵌入器（model-embed）+ 一个手编小语料 + 带「相似≠相关」陷阱的 query 集，
//! 量出当前 `recall_facts_in_bank` 的 recall@k 与「陷阱规避」准确度，作为后续调召回打分的基线。
//!
//! 重（要加载内嵌模型），故 `#[ignore]`。手动跑：
//!   `cargo test -p webui-bin --features full --test memory_eval -- --ignored --nocapture`
//!
//! 设计意图：陷阱 = 关键词重叠但话题无关的干扰项（如「登录页面按钮颜色」对「登录排查」query）。
//! bge 语义召回应把**话题相关**排在**字面重叠**之上——这正是 botobot「相似≠相关」口径要守的。

#![cfg(feature = "full")]

use std::sync::Arc;

use agent_act::memory::MemoryStore;

const BANK: &str = "default";

/// 语料：16 条事实，跨 5 个话题（登录/数据库/前端/部署/agent）。陷阱项标在 query 里。
const CORPUS: &[&str] = &[
    // 登录/认证
    "登录失败常见原因：密码错误、账号被锁、session 过期",
    "JWT token 在 Authorization header 里以 Bearer 形式传递",
    "用户登录后服务端签发 session，存 Redis，30 分钟过期",
    // 数据库
    "Postgres 慢查询用 EXPLAIN ANALYZE 看执行计划",
    "数据库连接池满会导致请求排队超时",
    "数据库迁移用 sqlx migrate，每个版本一个 up/down 文件",
    // 前端/UI（含陷阱：含「登录」但讲样式）
    "登录页面的提交按钮用主题绿色，圆角 22 像素",
    "前端用 vanilla JS，无框架，零依赖",
    "暗色主题切换跟随系统，圆形揭开过渡动画",
    // 部署
    "部署用 docker-compose，单容器静态链接二进制",
    "生产环境关闭 debug 日志，只留 info 以上级别",
    "健康检查端点 /health 返回 200 表示存活",
    // 记忆/agent
    "记忆召回用 bge 向量余弦，本地无外部向量数据库",
    "agent 用心跳驱动，cron 到点 submit 任务",
    "工具结果过长折叠进画布，对话流只显摘要",
    "session 持久化到 .bot/sessions，刷新后回读历史",
];

struct Case {
    query: &'static str,
    /// 相关事实（CORPUS 子串匹配）。
    relevant: &'static [&'static str],
    /// 「相似≠相关」陷阱：关键词重叠但不该排在相关项之上的干扰事实。
    trap: Option<&'static str>,
}

const CASES: &[Case] = &[
    Case {
        query: "用户登录失败怎么排查",
        relevant: &["登录失败常见原因", "服务端签发 session"],
        trap: Some("登录页面的提交按钮"), // 含「登录」但讲样式
    },
    Case {
        query: "数据库性能慢怎么办",
        relevant: &["慢查询用 EXPLAIN", "连接池满"],
        trap: None,
    },
    Case {
        query: "怎么把服务部署上线",
        relevant: &["docker-compose", "关闭 debug 日志", "健康检查端点"],
        trap: None,
    },
    Case {
        query: "记忆是怎么召回的",
        relevant: &["记忆召回用 bge 向量"],
        trap: Some("session 持久化"), // 都与存储沾边但话题不同
    },
    Case {
        query: "认证 token 怎么传递",
        relevant: &["JWT token"],
        trap: None,
    },
    // 更难：多关键词重叠陷阱（「数据库」字面命中前端无关项？这里用「迁移」近义干扰）。
    Case {
        query: "数据库 schema 怎么改",
        relevant: &["数据库迁移用 sqlx"],
        trap: Some("session，存 Redis"), // 含「存储/Redis」近义但讲 session 非 schema
    },
    // 更难：跨话题多事实（agent 运行机制）。
    Case {
        query: "agent 是怎么跑起来的",
        relevant: &["agent 用心跳驱动", "session 持久化到 .bot"],
        trap: None,
    },
];

fn contains_any(content: &str, needle: &str) -> bool {
    content.contains(needle)
}

const K: usize = 5;

/// 跑一遍全部 case，打印每条 + 返回 (mean recall@K, 陷阱规避率)。
fn eval_pass(store: &MemoryStore, label: &str) -> (f64, f64) {
    let mut recalls: Vec<f64> = Vec::new();
    let mut traps_ok = 0usize;
    let mut traps_total = 0usize;
    println!("\n--- pass: {label} ---");
    for c in CASES {
        let hits = store.recall_facts_in_bank(BANK, c.query, K);
        let got: Vec<&str> = hits.iter().map(|h| h.content.as_str()).collect();
        let found = c
            .relevant
            .iter()
            .filter(|r| got.iter().any(|g| contains_any(g, r)))
            .count();
        let recall = found as f64 / c.relevant.len() as f64;
        recalls.push(recall);

        let mut trap_note = String::new();
        if let Some(trap) = c.trap {
            traps_total += 1;
            let trap_rank = got.iter().position(|g| contains_any(g, trap));
            let best_rel_rank = c
                .relevant
                .iter()
                .filter_map(|r| got.iter().position(|g| contains_any(g, r)))
                .min();
            let ok = match (trap_rank, best_rel_rank) {
                (Some(tr), Some(rr)) => rr < tr,
                (None, _) => true,
                (Some(_), None) => false,
            };
            if ok {
                traps_ok += 1;
            }
            trap_note = format!(" · 陷阱「{trap}」{}", if ok { "规避✓" } else { "未规避✗" });
        }
        println!(
            "  q「{}」 recall@{K}={recall:.2} ({found}/{}){trap_note}",
            c.query,
            c.relevant.len()
        );
    }
    let mean_recall = recalls.iter().sum::<f64>() / recalls.len() as f64;
    let trap_rate = if traps_total > 0 {
        traps_ok as f64 / traps_total as f64
    } else {
        1.0
    };
    println!(
        "  汇总：mean recall@{K}={mean_recall:.3} · 陷阱规避 {traps_ok}/{traps_total}({:.0}%)",
        trap_rate * 100.0
    );
    (mean_recall, trap_rate)
}

#[test]
#[ignore = "重(加载 bge 模型)，手动跑：cargo test -p webui-bin --features full --test memory_eval -- --ignored --nocapture"]
fn memory_recall_eval() {
    let core = model_embed::EmbedCore::load().expect("加载 bge 模型失败");
    let path = std::env::temp_dir().join(format!("botobot-memeval-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = MemoryStore::open(&path).expect("open store");
    store.set_embedder(Arc::new(core)); // 真语义召回
    for f in CORPUS {
        store.retain(f).expect("retain");
    }

    println!("\n=== §1.8.6 A 记忆召回 eval（bge-small-zh, top{K}）===");

    // pass 1：对称召回（无 query 前缀，默认）。
    unsafe { std::env::remove_var("BOTOBOT_MEMORY_QUERY_PREFIX") };
    let (base_recall, base_trap) = eval_pass(&store, "对称（无前缀，默认）");

    // pass 2：非对称 s2p——bge-zh 推荐的 query 指令前缀（§1.8.3b 已有 env 钩子）。这是用 eval 量
    // 「开 query 前缀是否真提升召回」的数据驱动实验（bge 模型卡推荐，但要数据验证，不拍脑袋）。
    unsafe {
        std::env::set_var(
            "BOTOBOT_MEMORY_QUERY_PREFIX",
            "为这个句子生成表示以用于检索相关文章：",
        )
    };
    let (pfx_recall, pfx_trap) = eval_pass(&store, "非对称（bge 推荐 query 前缀）");
    unsafe { std::env::remove_var("BOTOBOT_MEMORY_QUERY_PREFIX") };

    println!(
        "\n=== A/B 结论：query 前缀 {} 召回（{base_recall:.3}→{pfx_recall:.3}，Δ{:+.3}）· 陷阱 {base_trap:.0?}→{pfx_trap:.0?} ===\n",
        if pfx_recall > base_recall + 0.01 {
            "提升"
        } else if pfx_recall < base_recall - 0.01 {
            "降低"
        } else {
            "基本不变"
        },
        pfx_recall - base_recall,
    );

    // 实验 C：1-hop 实体扩散能否救多事实弱点（§1.8.6 C 核心假设）。
    // 「怎么部署上线」纯余弦只 recall 1/3——因为 3 条部署事实彼此独立、只最相似的进 top。
    // 若把它们作为**共享实体「部署」的 episode** 存，recall_expanded 的 1-hop 扩散应能从命中的
    // 一条把另外两条拉进来。这是用 eval 验证「结构化（实体链接）修多事实召回」的数据，informs §1.8.6 C。
    {
        let dpath = std::env::temp_dir().join(format!("botobot-memeval-ep-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&dpath);
        let estore = MemoryStore::open(&dpath).expect("open ep store");
        let core2 = model_embed::EmbedCore::load().expect("加载 bge");
        estore.set_embedder(Arc::new(core2));
        let deploy: &[&str] = &[
            "部署用 docker-compose，单容器静态链接二进制",
            "部署后生产环境关闭 debug 日志，只留 info 以上",
            "部署的健康检查端点 /health 返回 200 表示存活",
        ];
        for d in deploy {
            // 共享实体「部署」（实际产线由 EpisodeWriter LLM 抽取；此处手标以验证扩散机制）。
            estore
                .append_episode(BANK, d, vec!["部署".into()], vec![], None, Some(0))
                .expect("append ep");
        }
        let q = "怎么把服务部署上线";
        let cosine_only = estore.recall_facts_in_bank(BANK, q, 1); // 只取最相似 1 条（模拟多事实只命中一条）
        let (primary, expanded) = estore.recall_expanded(BANK, q, 1, 5);
        let cos_n = cosine_only.len();
        let exp_total = primary.len() + expanded.len();
        println!("\n=== 实验 C：1-hop 实体扩散救多事实 ===");
        println!("  q「{q}」 纯余弦(top1)={cos_n} 条 · 扩散后={exp_total} 条（主 {} + 扩散 {}）", primary.len(), expanded.len());
        println!("  扩散补回：{:?}", expanded.iter().map(|h| &h.content[..h.content.char_indices().nth(10).map(|(i,_)| i).unwrap_or(h.content.len())]).collect::<Vec<_>>());
        let _ = std::fs::remove_file(&dpath);
        // 结论门：扩散应把另外两条部署事实拉回（共享实体「部署」），总数 > 纯余弦。
        assert!(
            exp_total > cos_n,
            "1-hop 实体扩散未能补回多事实（{exp_total} ≤ {cos_n}）——§1.8.6 C 假设需复查"
        );
    }

    let _ = std::fs::remove_file(&path);

    // 基线门（防退化，用对称默认那一遍）：调召回打分时此门必须不降。
    assert!(base_recall >= 0.6, "mean recall@{K}={base_recall:.3} 低于基线 0.6");
    assert!(base_trap >= 0.5, "陷阱规避率 {base_trap:.2} 低于基线 0.5");
}
