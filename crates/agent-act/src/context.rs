//! 统一上下文原语（§1.8.2，设计定稿 2026-06-23）。
//!
//! 洞察（§1.8.1）：tool/skill/book/memory 四桶不是四种数据，是同一空间里的四个点——
//! 每块「能进窗口的上下文」由几个正交坐标描述。与其为「第五种上下文」再造一条
//! bespoke 管线，不如做一台被坐标参数化的统一引擎，新源只是「再拧一组旋钮」。
//!
//! 本模块**只做第 1 步：纯定义**（trait + 坐标类型 + 常驻把手）。不接线、不造
//! [`ContextAssembler`]（静态按可信度+预算挑选，后续步骤）。现有
//! [`crate::resource::Resource`]（`scheme()` + `resolve()`）已是「按需展开」那半边；
//! [`ContextSource`] 在其上补「常驻把手 `handle()`」+「坐标声明 `facets()`」。
//!
//! ⚠️ 边界（§0 不预付）：不造「各源动态竞标桌面」的优化器；先用「可信度排序 + 预算
//! 封顶」笨办法，真有预算撕扯再升级。

use async_trait::async_trait;

use crate::resource::ResourceDoc;

/// 可信度（§1.5 可信度纪律）：决定常驻优先级与召回标签。
///
/// 序：`Authority`(Law/Book) > `Skill` > `Observation`(本次工具事实) > `Memory`(低可信联想)。
/// `derive(Ord)` 按声明序升序，故最低可信 `Memory` 在前、最高 `Authority` 在后，
/// 于是 `Authority > Memory` 成立——预算紧时高可信优先常驻、低可信先砍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trust {
    /// 最低可信：经验/印象/联想，需核验（memory://）。
    Memory,
    /// 本次执行事实：工具 observation。
    Observation,
    /// 可进化 SOP：经 gate 优化（skill://）。
    Skill,
    /// 最高可信：可溯源依据，法律/制度/手册/书本（book://）。
    Authority,
}

/// 易变度（§1.8.1）：从不可变法理到实时观测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Volatility {
    /// 不可变（法理、已归档依据）。
    Immutable,
    /// 当前世界态（文件/cwd/会话状态，会变但非实时）。
    WorldState,
    /// 实时（live 观测、流）。
    Realtime,
}

/// 检索法（§1.8.1）：如何取到 body。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Retrieval {
    /// 常驻，无需检索（已在前缀里）。
    Resident,
    /// 精确键查（scheme://exact-id）。
    ExactKey,
    /// 结构树 + 推理式检索（book 标题树 / citation）。
    StructuredTree,
    /// 语义相似（向量召回，低可信粗筛）。
    Semantic,
}

/// 写回通道（§1.8.1 / §1.5 变更模型）：谁能改、怎么改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Writeback {
    /// 只读（book 原始依据、http 抓取）。
    ReadOnly,
    /// 经 gate 优化（skill_patch + 人审/eval）。
    Gated,
    /// 自由累积（memory retain/forget）。
    Free,
    /// 不是数据，是动作（tool；不进检索）。
    NotData,
}

/// 一个上下文源的坐标声明（§1.8.1 五坐标的前四个；第五个「承载/residency」由
/// [`ContextSource::handle`] 返回 `Some`/`None` 隐式表达）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextFacets {
    pub trust: Trust,
    pub volatility: Volatility,
    pub retrieval: Retrieval,
    pub writeback: Writeback,
}

/// 常驻把手（§1.8.2 ①）：摆上桌面的蒸馏索引（不是全文）。
///
/// 模型先看一桌目录（各源的 `digest`），要细节才 `read(scheme://…)` 触发
/// [`ContextSource::expand`]。`est_tokens` 供 [`ContextAssembler`]（后续步骤）按预算
/// 取舍；`trust` 冗余携带便于排序时无需回查 facets。
#[derive(Debug, Clone)]
pub struct Handle {
    /// 蒸馏后的常驻文本（概要/索引/标题树），将拼进 system 前缀。
    pub digest: String,
    /// 估算 token 数，供预算封顶。
    pub est_tokens: usize,
    /// 可信度，供 assembler 排序（= `facets().trust`）。
    pub trust: Trust,
}

/// 统一上下文源（§1.8.2）：在现有 [`Resource`](crate::resource::Resource) 的「按需展开」
/// 之上，补「常驻把手」+「坐标声明」，让 skill/book/memory/压缩历史等都用同一台引擎装配。
///
/// 第 1 步纯定义；接线（给现有源实现本 trait、造 `ContextAssembler`）属后续步骤。
#[async_trait]
pub trait ContextSource: Send + Sync {
    /// 复用 `read(scheme://)` 寻址的 scheme（不带 `://`）。
    fn scheme(&self) -> &str;

    /// 坐标自报（③ 坐标声明）。
    fn facets(&self) -> ContextFacets;

    /// 常驻把手（① 摆桌上）。`None` = 本源不常驻（如 `file://`，纯按需）。
    /// `budget` = 分给本源的常驻 token 预算；源据此决定蒸馏多细。
    async fn handle(&self, budget: usize) -> Option<Handle>;

    /// 按需展开（② ≈ `Resource::resolve`）：把 query 解析成完整文档。
    async fn expand(&self, query: &str) -> anyhow::Result<ResourceDoc>;
}

/// 桌面管理员（§1.8.2 / §1.8.7）：收齐各源 `handle()`，**按源优先级顺序**拼成一个
/// 「你的工作记忆」块（顶部一段心法 + 各源自报的小节），预算封顶按顺序砍尾部。
/// 模型看这张「脑子目录」，要细节就 `read(scheme://)` → `expand`（渐进披露）。
///
/// §1.8.7 改动：不再按可信度排序 / 贴 `[TRUST]` 头——可信度改由「新近度」在**召回时**
/// 体现（见各源 read 出口）；常驻只呈现「现在记着的(可信) + 知道能去翻的(钩子)」。
/// 源顺序即优先级（memory→skill→book），预算紧时尾部（书指针）先被砍。
///
/// ⚠️ 边界（§0 不预付）：静态顺序拼接 + 预算封顶，不造动态竞标优化器。
pub struct ContextAssembler {
    /// 常驻前缀的总 token 预算。
    budget: usize,
}

/// 「工作记忆」总框头（§1.8.7 心法）：近期明确→直接用；技能/书→翻开；模糊旧的→召回+核实。
const WORKING_MEMORY_HEADER: &str = "# Your working memory\n\
     What you currently hold in mind. Recent, explicit facts: rely on them and act directly. \
     Skills and books: open them (read) when a request calls for it. Anything faint, old, or \
     not shown here: recall it via read(memory://<topic>) or search past sessions, and verify \
     before relying.\n";

/// 把若干源小节拼成一块（可选带「工作记忆」总框头）；空小节返回空串。
fn render_sections(with_header: bool, sections: &[String]) -> String {
    if sections.is_empty() {
        return String::new();
    }
    let mut out = if with_header {
        String::from(WORKING_MEMORY_HEADER)
    } else {
        String::new()
    };
    for s in sections {
        out.push('\n');
        out.push_str(s.trim_end());
        out.push('\n');
    }
    out
}

impl ContextAssembler {
    pub fn new(budget: usize) -> Self {
        Self { budget }
    }

    /// 按源优先级顺序收 `handle()`，预算内拼成「工作记忆」块（各源自报小节）。
    /// 返回空串=无可常驻或预算为 0。
    pub async fn assemble(&self, sources: &[std::sync::Arc<dyn ContextSource>]) -> String {
        let mut sections: Vec<String> = Vec::new();
        let mut spent = 0usize;
        for src in sources {
            if let Some(h) = src.handle(self.budget).await {
                if spent + h.est_tokens > self.budget {
                    continue; // 预算外：尾部（书指针）先被砍
                }
                spent += h.est_tokens;
                sections.push(h.digest);
            }
        }
        render_sections(true, &sections)
    }

    /// §4.9 B4：**静/动两段装配**——为保 provider 前缀 KV 缓存，把**不可变源**（skill/book，
    /// [`Volatility::Immutable`]）烤进**静态前缀**，把**易变源**（memory「现在记着的」等
    /// `WorldState`/`Realtime`）单独放**动态段**置于静态之后。返回 `(static_prefix, dynamic_suffix)`。
    ///
    /// 不变量：`static_prefix + dynamic_suffix` 与 [`Self::assemble`] 在「全静态源」时**完全一致**，
    /// 含混合源时仅把易变小节移到尾部（语义等价、利于将来 `complete_split(static, dynamic)`）。
    /// 总框头只随**第一个非空段**出现一次（静态非空则在静态，否则在动态），拼接后不重复。
    /// 预算按源顺序统一封顶（与 `assemble()` 同口径）。
    pub async fn assemble_split(
        &self,
        sources: &[std::sync::Arc<dyn ContextSource>],
    ) -> (String, String) {
        let mut static_sections: Vec<String> = Vec::new();
        let mut dynamic_sections: Vec<String> = Vec::new();
        let mut spent = 0usize;
        for src in sources {
            if let Some(h) = src.handle(self.budget).await {
                if spent + h.est_tokens > self.budget {
                    continue; // 预算外：尾部先被砍（与 assemble 同口径）
                }
                spent += h.est_tokens;
                if src.facets().volatility == Volatility::Immutable {
                    static_sections.push(h.digest);
                } else {
                    dynamic_sections.push(h.digest);
                }
            }
        }
        let static_prefix = render_sections(true, &static_sections);
        // 静态段已带框头则动态段不再带（拼接后只一份框头）；静态为空时动态承载框头。
        let dynamic_suffix = render_sections(static_prefix.is_empty(), &dynamic_sections);
        (static_prefix, dynamic_suffix)
    }
}

/// 粗估 token 数（对齐 `BOTOBOT_TOKENIZER` 缺省的 `chars/3` 估计）。
pub fn est_tokens(s: &str) -> usize {
    s.chars().count() / 3 + 1
}

/// 把常驻文本裁到预算内（按行边界、保整行）。`budget` 为 token 预算；
/// 超出则截断并追加省略标记，便于 [`ContextSource::handle`] 自限。
pub fn fit_budget(digest: &str, budget_tokens: usize) -> String {
    if est_tokens(digest) <= budget_tokens {
        return digest.to_string();
    }
    let cap_chars = budget_tokens.saturating_mul(3);
    let mut out = String::new();
    for line in digest.lines() {
        if out.chars().count() + line.chars().count() + 1 > cap_chars {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("…（受预算截断）\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn est_tokens_and_fit_budget() {
        assert_eq!(est_tokens(""), 1);
        let big = "line one\nline two\nline three\nline four\n";
        // 充足预算：原样返回。
        assert_eq!(fit_budget(big, 1000), big);
        // 紧预算：截断 + 标记，且不超过原文。
        let tight = fit_budget(big, 3);
        assert!(tight.contains("受预算截断"));
        assert!(tight.chars().count() < big.chars().count());
    }

    #[test]
    fn trust_orders_authority_above_memory() {
        // 可信度纪律：Book/Authority > Skill > Observation > Memory。
        assert!(Trust::Authority > Trust::Skill);
        assert!(Trust::Skill > Trust::Observation);
        assert!(Trust::Observation > Trust::Memory);
        // 极端：最高 > 最低，供 assembler 预算淘汰用。
        assert!(Trust::Authority > Trust::Memory);
        // 排序后最高可信在末尾（max）。
        let mut v = [
            Trust::Memory,
            Trust::Authority,
            Trust::Skill,
            Trust::Observation,
        ];
        v.sort();
        assert_eq!(v.last().copied(), Some(Trust::Authority));
        assert_eq!(v.first().copied(), Some(Trust::Memory));
    }

    #[test]
    fn facets_and_handle_are_plain_values() {
        // 纯定义自洽：facets 可构造、handle 携带冗余 trust。
        let facets = ContextFacets {
            trust: Trust::Skill,
            volatility: Volatility::Immutable,
            retrieval: Retrieval::ExactKey,
            writeback: Writeback::Gated,
        };
        let handle = Handle {
            digest: "skill: brainstorming, writing-plans".into(),
            est_tokens: 12,
            trust: facets.trust,
        };
        assert_eq!(handle.trust, facets.trust);
        // PartialEq 自洽：相同坐标的两个 facets 相等。
        let same = ContextFacets {
            trust: Trust::Skill,
            volatility: Volatility::Immutable,
            retrieval: Retrieval::ExactKey,
            writeback: Writeback::Gated,
        };
        assert_eq!(facets, same);
    }

    use std::sync::Arc;

    /// 测试桩源：返回固定 trust + digest 的 handle。
    struct StubSource {
        scheme: &'static str,
        trust: Trust,
        digest: String,
        resident: bool,
    }
    #[async_trait]
    impl ContextSource for StubSource {
        fn scheme(&self) -> &str {
            self.scheme
        }
        fn facets(&self) -> ContextFacets {
            ContextFacets {
                trust: self.trust,
                volatility: Volatility::Immutable,
                retrieval: Retrieval::ExactKey,
                writeback: Writeback::ReadOnly,
            }
        }
        async fn handle(&self, budget: usize) -> Option<Handle> {
            if !self.resident {
                return None;
            }
            let digest = fit_budget(&self.digest, budget);
            Some(Handle {
                est_tokens: est_tokens(&digest),
                digest,
                trust: self.trust,
            })
        }
        async fn expand(&self, _q: &str) -> anyhow::Result<ResourceDoc> {
            anyhow::bail!("stub")
        }
    }

    #[tokio::test]
    async fn assembler_keeps_source_order_frames_and_skips_nonresident() {
        // §1.8.7：按源优先级顺序拼接（memory→skill→book），顶部「工作记忆」框，无 trust 头。
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(StubSource {
                scheme: "memory",
                trust: Trust::Memory,
                digest: "mem holding-now".into(),
                resident: true,
            }),
            Arc::new(StubSource {
                scheme: "book",
                trust: Trust::Authority,
                digest: "book pointer".into(),
                resident: true,
            }),
            Arc::new(StubSource {
                scheme: "file",
                trust: Trust::Observation,
                digest: "should not appear".into(),
                resident: false, // 不常驻
            }),
        ];
        let prefix = ContextAssembler::new(1000).assemble(&sources).await;
        // 顶部工作记忆框。
        assert!(prefix.contains("Your working memory"));
        // 源顺序保留（memory 在 book 之前）；不再有 trust 头。
        let mem_at = prefix.find("mem holding-now").unwrap();
        let book_at = prefix.find("book pointer").unwrap();
        assert!(mem_at < book_at, "应按源序 memory→book:\n{prefix}");
        assert!(!prefix.contains("low-confidence") && !prefix.contains("AUTHORITY"));
        // 不常驻源（file）不出现。
        assert!(!prefix.contains("should not appear"));
    }

    #[tokio::test]
    async fn assembler_caps_budget_dropping_tail_first() {
        // 极紧预算：保前面的源、砍尾部（§1.8.7：书指针最先被砍）。
        let big = "x".repeat(60); // est ~ 21 tokens
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(StubSource {
                scheme: "memory",
                trust: Trust::Memory,
                digest: "KEEP".into(),
                resident: true,
            }),
            Arc::new(StubSource {
                scheme: "book",
                trust: Trust::Authority,
                digest: big.clone(),
                resident: true,
            }),
        ];
        // 预算只够第一个源（"KEEP"），第二个（60 字符）超预算被砍。
        let prefix = ContextAssembler::new(4).assemble(&sources).await;
        assert!(prefix.contains("KEEP"), "前置源应保留:\n{prefix}");
        assert!(!prefix.contains(&big), "尾部源应被预算砍掉");
    }

    /// 测试桩源：可指定 volatility（验证 §4.9 B4 静/动分段）。
    struct VolSource {
        scheme: &'static str,
        digest: String,
        volatility: Volatility,
    }
    #[async_trait]
    impl ContextSource for VolSource {
        fn scheme(&self) -> &str {
            self.scheme
        }
        fn facets(&self) -> ContextFacets {
            ContextFacets {
                trust: Trust::Memory,
                volatility: self.volatility,
                retrieval: Retrieval::ExactKey,
                writeback: Writeback::ReadOnly,
            }
        }
        async fn handle(&self, budget: usize) -> Option<Handle> {
            let digest = fit_budget(&self.digest, budget);
            Some(Handle {
                est_tokens: est_tokens(&digest),
                digest,
                trust: Trust::Memory,
            })
        }
        async fn expand(&self, _q: &str) -> anyhow::Result<ResourceDoc> {
            anyhow::bail!("stub")
        }
    }

    #[tokio::test]
    async fn assemble_split_partitions_by_volatility_and_concatenates_to_assemble() {
        // skill/book = Immutable（静态前缀）；memory = WorldState（动态段，置于静态之后）。
        let sources: Vec<Arc<dyn ContextSource>> = vec![
            Arc::new(VolSource {
                scheme: "skill",
                digest: "SKILL section".into(),
                volatility: Volatility::Immutable,
            }),
            Arc::new(VolSource {
                scheme: "book",
                digest: "BOOK section".into(),
                volatility: Volatility::Immutable,
            }),
            Arc::new(VolSource {
                scheme: "memory",
                digest: "MEMORY holding-now".into(),
                volatility: Volatility::WorldState,
            }),
        ];
        let asm = ContextAssembler::new(1000);
        let (static_prefix, dynamic_suffix) = asm.assemble_split(&sources).await;

        // 静态段含框头 + 不可变源；不含易变源。
        assert!(static_prefix.contains("Your working memory"));
        assert!(static_prefix.contains("SKILL section"));
        assert!(static_prefix.contains("BOOK section"));
        assert!(
            !static_prefix.contains("MEMORY holding-now"),
            "易变源不进静态:\n{static_prefix}"
        );

        // 动态段只含易变源、不重复框头。
        assert!(dynamic_suffix.contains("MEMORY holding-now"));
        assert!(
            !dynamic_suffix.contains("Your working memory"),
            "框头不应重复:\n{dynamic_suffix}"
        );

        // 框头只出现一次（KV：拼接后一份）。
        let combined = format!("{static_prefix}{dynamic_suffix}");
        assert_eq!(combined.matches("Your working memory").count(), 1);
        // 静态在前、动态在后。
        assert!(
            combined.find("SKILL section").unwrap() < combined.find("MEMORY holding-now").unwrap()
        );
    }

    #[tokio::test]
    async fn assemble_split_all_static_equals_assemble() {
        // 全静态源：split 拼接结果 == assemble（不变量）。
        let make = || -> Vec<Arc<dyn ContextSource>> {
            vec![
                Arc::new(VolSource {
                    scheme: "skill",
                    digest: "A".into(),
                    volatility: Volatility::Immutable,
                }),
                Arc::new(VolSource {
                    scheme: "book",
                    digest: "B".into(),
                    volatility: Volatility::Immutable,
                }),
            ]
        };
        let asm = ContextAssembler::new(1000);
        let one = asm.assemble(&make()).await;
        let (s, d) = asm.assemble_split(&make()).await;
        assert!(d.is_empty(), "全静态时动态段应为空");
        assert_eq!(one, s, "全静态时 assemble == 静态前缀");
    }

    #[tokio::test]
    async fn assemble_split_only_dynamic_carries_header() {
        // 仅易变源：动态段承载框头（静态为空）。
        let sources: Vec<Arc<dyn ContextSource>> = vec![Arc::new(VolSource {
            scheme: "memory",
            digest: "only mem".into(),
            volatility: Volatility::Realtime,
        })];
        let (s, d) = ContextAssembler::new(1000).assemble_split(&sources).await;
        assert!(s.is_empty(), "无不可变源时静态前缀应为空");
        assert!(
            d.contains("Your working memory"),
            "静态空时动态承载框头:\n{d}"
        );
        assert!(d.contains("only mem"));
    }

    /// 编译期确认 trait 对象安全（object-safe）：能装进 `Arc<dyn ContextSource>`。
    #[test]
    fn context_source_is_object_safe() {
        struct Dummy;
        #[async_trait]
        impl ContextSource for Dummy {
            fn scheme(&self) -> &str {
                "dummy"
            }
            fn facets(&self) -> ContextFacets {
                ContextFacets {
                    trust: Trust::Memory,
                    volatility: Volatility::Realtime,
                    retrieval: Retrieval::Semantic,
                    writeback: Writeback::Free,
                }
            }
            async fn handle(&self, _budget: usize) -> Option<Handle> {
                None
            }
            async fn expand(&self, _query: &str) -> anyhow::Result<ResourceDoc> {
                anyhow::bail!("dummy")
            }
        }
        let src: std::sync::Arc<dyn ContextSource> = std::sync::Arc::new(Dummy);
        assert_eq!(src.scheme(), "dummy");
        assert_eq!(src.facets().trust, Trust::Memory);
    }
}
