// 入口：refine(items, opts) -> RefineResult { items, provenance, report }。
// 收/发内存对象。fail-open：任何异常/LLM 不可用 → 原样返回输入 + 大声 log。
// 出口闸门：保真不变式 + 异常数单调 + 几何，任一不过 → fail-open。

use crate::agent_loop::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_ROUNDS, Logger, LoopOptions, default_logger, run_loop,
    skipped_without_vision,
};
use crate::detect::detect;
use crate::id::{assign_ids, strip_ids};
use crate::invariant::check_fidelity;
use crate::llm::{
    ChatClient, DeepSeekClient, ImageDirLoader, LlmError, LoadImage, QwenVlClient,
    SplitTableVerdict, VisionClient,
};
use crate::types::{MineruItem, RefineReport, RefineResult, WorkItem};
use futures::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

// 0.5：split_table 视觉裁决；0.6：拆表检测放宽到链式 + split_table 仅视觉裁决；
// 0.7：Rust 重写（逻辑对齐 0.6，实现换底）
pub const REFINE_LOGIC_VERSION: &str = "0.7.0";
pub const PROMPT_VERSION: &str = "p4"; // p4：system prompt 与工具集移除 mergeTable
/// 默认文本裁决模型;运行时可被 `DEEPSEEK_MODEL` 覆盖(见 `cache_key_for`)。
pub const MODEL_ID: &str = crate::llm::DEEPSEEK_DEFAULT_MODEL;

#[derive(Default)]
pub struct RefineOptions {
    /// 源文件 SHA256；提供时启用进程内缓存
    pub sha256: Option<String>,
    /// 外层循环硬上限；不传则自适应（adaptive_max_iterations，随疑点数 48~512）
    pub max_iterations: Option<u64>,
    /// 并行裁决的疑点数（默认 8；1 = 严格串行）
    pub concurrency: Option<usize>,
    /// MinerU 产物目录：提供则构造 ImageDirLoader 启用视觉裁决（与 load_image 二选一）
    pub image_dir: Option<PathBuf>,
    /// 只读图片访问器。split_table 仅视觉裁决：提供时走 Qwen-VL（取不到图/视觉失败 →
    /// 搁置该疑点）；不提供 = 无视觉模型，split_table 整体跳过。任何情况下都不走文本路径做 mergeTable。
    pub load_image: Option<Arc<dyn LoadImage>>,
    /// 内部/测试用：注入 LLM 调用（默认 DeepSeek 裸 API）。
    pub chat: Option<Arc<dyn ChatClient>>,
    /// 内部/测试用：注入视觉裁决（默认 Qwen-VL 裸 API）。
    pub vision: Option<Arc<dyn VisionClient>>,
    pub log: Option<Logger>,
}

/// maxIterations 的自适应默认值：随初始可处理疑点数走，固定常数对大文档必然截断。
/// 2× 给"修复解锁新疑点"留余量（实测大文档总工作量 ≈ 1.6× 初始疑点数：空壳 drop 后
/// 表格变相邻冒出新拆表对等），下限 48 保持小文档现状，上限 512 兜病态文档的成本。
/// 显式传 opts.max_iterations 时不走这里。
pub fn adaptive_max_iterations(actionable_suspects: usize) -> u64 {
    (2 * actionable_suspects as u64 + 16).clamp(48, 512)
}

// 缓存 key = sha256(源文件) + refineLogicVersion + model + promptVersion（只用源文件 SHA256 是错的）
static CACHE: LazyLock<Mutex<HashMap<String, RefineResult>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn cache_key_for(sha256: &str) -> String {
    format!(
        "{sha256}:{REFINE_LOGIC_VERSION}:{}:{PROMPT_VERSION}",
        crate::llm::effective_deepseek_model()
    )
}

/// 测试/运维用：清空进程内缓存。
pub fn clear_refine_cache() {
    CACHE.lock().unwrap().clear();
}

/// 视觉客户端不可用时的占位：每次调用都报构造期的错误（对齐 TS 行为——
/// 默认 judgeSplitTable 缺 key 在【调用时】才抛，被 try_vision_verdict 捕获 → 搁置 + log）。
struct UnavailableVision(String);

#[async_trait::async_trait]
impl VisionClient for UnavailableVision {
    async fn judge_split_table(&self, _: &[u8], _: &[u8]) -> Result<SplitTableVerdict, LlmError> {
        Err(LlmError(self.0.clone()))
    }
}

pub async fn refine(items: Vec<MineruItem>, opts: RefineOptions) -> RefineResult {
    let log: Logger = opts.log.clone().unwrap_or_else(default_logger);

    let key = opts.sha256.as_ref().map(|s| cache_key_for(s));
    if let Some(k) = &key
        && let Some(hit) = CACHE.lock().unwrap().get(k)
    {
        return hit.clone();
    }

    // fail-open 基准：输入的不可变快照
    let snapshot = items;

    let fail_open = |why: &str| -> RefineResult {
        log(&format!(
            "FAIL-OPEN：{why} —— 原样返回输入 {} 个 items",
            snapshot.len()
        ));
        RefineResult {
            items: snapshot.clone(),
            provenance: vec![],
            report: RefineReport {
                fail_open: true,
                ..RefineReport::default()
            },
        }
    };

    let attempt = AssertUnwindSafe(refine_inner(&snapshot, &opts, &log))
        .catch_unwind()
        .await;
    match attempt {
        Ok(Ok(result)) => {
            if let Some(k) = key {
                CACHE.lock().unwrap().insert(k, result.clone());
            }
            result
        }
        Ok(Err(why)) => fail_open(&why),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "（无法还原的 panic）".into());
            fail_open(&format!("异常: {msg}"))
        }
    }
}

async fn refine_inner(
    snapshot: &[MineruItem],
    opts: &RefineOptions,
    log: &Logger,
) -> Result<RefineResult, String> {
    let (ref_items, next_id) = assign_ids(snapshot);

    let load_image: Option<Arc<dyn LoadImage>> = opts.load_image.clone().or_else(|| {
        opts.image_dir
            .as_ref()
            .map(|d| ImageDirLoader::new(d.clone()) as Arc<dyn LoadImage>)
    });
    let has_vision = load_image.is_some();

    // 无视觉模型时 split_table 整体跳过，不计入迭代预算，也不参与"异常数单调"闸门
    //（跳过的疑点原样留在输出里，按原计数会被误判为"修不动"触发 fail-open）
    let gate_countable =
        |w: &WorkItem| -> bool { w.has_op && !skipped_without_vision(w, has_vision) };
    let input_suspects = detect(&ref_items)
        .iter()
        .filter(|w| gate_countable(w))
        .count();
    let ref_before = ref_items.clone();

    let chat: Arc<dyn ChatClient> = match &opts.chat {
        Some(c) => c.clone(),
        None => DeepSeekClient::from_env().map_err(|e| e.to_string())?,
    };
    let vision: Option<Arc<dyn VisionClient>> = match &opts.vision {
        Some(v) => Some(v.clone()),
        None if has_vision => Some(match QwenVlClient::from_env() {
            Ok(v) => v as Arc<dyn VisionClient>,
            Err(e) => Arc::new(UnavailableVision(e.to_string())),
        }),
        None => None,
    };

    let loop_result = run_loop(
        ref_items,
        next_id,
        LoopOptions {
            max_iterations: opts
                .max_iterations
                .unwrap_or_else(|| adaptive_max_iterations(input_suspects)),
            max_rounds_per_suspect: DEFAULT_MAX_ROUNDS,
            concurrency: opts.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
            chat,
            load_image,
            vision,
            log: log.clone(),
        },
    )
    .await
    .map_err(|e| format!("异常: {e}"))?;

    // ── 出口闸门（合格判定）──
    check_fidelity(&ref_before, &loop_result.items, None)
        .map_err(|reason| format!("出口保真闸门不过: {reason}"))?;

    let output_suspects = detect(&loop_result.items)
        .iter()
        .filter(|w| gate_countable(w))
        .count();
    if output_suspects > input_suspects {
        return Err(format!(
            "异常数不单调: 输入 {input_suspects} → 输出 {output_suspects}"
        ));
    }

    Ok(RefineResult {
        items: strip_ids(&loop_result.items), // 出口剥除内部 ID（schema 透明）
        provenance: vec![],                   // 纯削减模式（不加字）→ 恒为空，结构预留
        report: RefineReport {
            iterations: loop_result.iterations,
            op_counts: loop_result.op_counts,
            dismissed: loop_result.dismissed,
            removed_spans: loop_result.removed_spans,
            violations: loop_result.violations,
            token_usage: loop_result.token_usage,
            fail_open: false,
        },
    })
}
