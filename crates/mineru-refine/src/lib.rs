// mineru-refine：MinerU 解析结果的 linter/fixer（Rust core）。
// content_list（item 对象数组）进，同 schema 出：只做削减与重组，绝不新增一个字。
// 硬保证：C_out ⊆ C_in（机器闸门裁决，违反即回滚/fail-open）。
// 唯一例外是 opt-in 的混淆修正层（fix_ocr_confusion，见 confusion.rs）：
// 准入名单内的稀疏一换一替换，全量进 report/provenance，可审计可撤销。
//
// 公共入口：refine()。内部模块（agent_loop/ops/llm）按需 re-export。

pub mod agent_loop;
pub mod confusion;
pub mod detect;
pub mod id;
pub mod invariant;
pub mod llm;
pub mod markdown;
pub mod mechanical;
pub mod ops;
pub mod refine;
pub mod types;

pub use refine::{
    MODEL_ID, PROMPT_VERSION, REFINE_LOGIC_VERSION, RefineOptions, adaptive_max_iterations,
    cache_key_for, cache_key_for_opts, clear_refine_cache, refine,
};

pub use types::{
    ConfusionFix, MineruItem, OpCall, ProvenanceEntry, RefineReport, RefineResult, RemovedSpan,
    StripPattern, SuspectKind, WorkItem,
};

pub use confusion::CONFUSION_PROMPT_VERSION;

pub use agent_loop::Logger;
pub use llm::{
    ChatClient, ChatResult, ImageDirLoader, LlmError, LoadImage, Message, SplitTableVerdict,
    ToolCall, Usage, VisionClient,
};

// 独立可用的工具件：探测器（疑点统计）、机械清洗与 full.md 确定性重渲染
pub use detect::{detect, detect_items, droppable_ids};
pub use id::{assign_ids, strip_ids};
pub use markdown::render_markdown;
pub use mechanical::{MechOutcome, mechanical_clean};
