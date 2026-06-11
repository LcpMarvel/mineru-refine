// mineru-refine：MinerU 解析结果的 linter/fixer（Rust core）。
// content_list（item 对象数组）进，同 schema 出：只做削减与重组，绝不新增一个字。
// 硬保证：C_out ⊆ C_in（机器闸门裁决，违反即回滚/fail-open）。
//
// 公共入口：refine()。内部模块（agent_loop/ops/llm）按需 re-export。

pub mod agent_loop;
pub mod detect;
pub mod id;
pub mod invariant;
pub mod llm;
pub mod markdown;
pub mod ops;
pub mod refine;
pub mod types;

pub use refine::{
    MODEL_ID, PROMPT_VERSION, REFINE_LOGIC_VERSION, RefineOptions, adaptive_max_iterations,
    cache_key_for, clear_refine_cache, refine,
};

pub use types::{
    MineruItem, OpCall, ProvenanceEntry, RefineReport, RefineResult, RemovedSpan, StripPattern,
    SuspectKind, WorkItem,
};

pub use agent_loop::Logger;
pub use llm::{
    ChatClient, ChatResult, ImageDirLoader, LlmError, LoadImage, Message, SplitTableVerdict,
    ToolCall, Usage, VisionClient,
};

// 独立可用的工具件：探测器（疑点统计）与 full.md 确定性重渲染
pub use detect::{detect, detect_items, droppable_ids};
pub use id::{assign_ids, strip_ids};
pub use markdown::render_markdown;
