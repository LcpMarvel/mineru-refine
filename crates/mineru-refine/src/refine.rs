// 入口：refine(items, opts) -> RefineResult { items, provenance, report }。
// 收/发内存对象。fail-open：任何异常/LLM 不可用 → 原样返回输入 + 大声 log。
// 出口闸门：保真不变式 + 异常数单调 + 几何，任一不过 → fail-open。

use crate::agent_loop::{
    DEFAULT_CONCURRENCY, DEFAULT_MAX_ROUNDS, Logger, LoopOptions, default_logger, run_loop,
    skipped_without_vision,
};
use crate::confusion::{
    CONFUSION_PROMPT_VERSION, ConfusionOutcome, ConfusionTable, fix_confusions,
};
use crate::detect::detect;
use crate::garbled::{GARBLED_PROMPT_VERSION, GarbledOutcome, rewrite_garbled_tables};
use crate::id::{assign_ids, strip_ids};
use crate::invariant::check_fidelity;
use crate::llm::{
    ChatClient, DeepSeekClient, ImageDirLoader, LlmError, LoadImage, QwenVlClient,
    SplitTableVerdict, TableTranscription, VisionClient,
};
use crate::mechanical::mechanical_clean;
use crate::types::{MineruItem, ProvenanceEntry, RefItem, RefineReport, RefineResult, WorkItem};
use futures::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

// 0.5：split_table 视觉裁决；0.6：拆表检测放宽到链式 + split_table 仅视觉裁决；
// 0.7：Rust 重写（逻辑对齐 0.6，实现换底）；
// 0.8：机械清洗 pass + 三个新探测器（missed_heading/trailing_marker/separated_caption）；
// 0.9：赘字/衍字删除（extra_char 探测器 + deleteChar op，全走 LLM 裁决）；
// 0.10：重度乱码表视觉重转写层（rewrite_garbled_tables，opt-in）;
// 0.11：矛盾决策守卫 + 兄弟组/同文 page_artifact 联合裁决 + dismiss 时序竞争守卫 + promote 层级锚点校正
pub const REFINE_LOGIC_VERSION: &str = "0.11.0";
pub const PROMPT_VERSION: &str = "p6"; // p6：extra_char 疑点 op_hint + deleteChar 工具
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
    /// OCR 字符混淆修正层（opt-in）。开启后输出不再满足 C_out ⊆ C_in，
    /// 改为双契约：核心层只删不增 + 混淆层在准入名单内做稀疏一换一替换
    ///（每条进 report.confusionFixes 与 provenance，可审计可撤销）。
    pub fix_ocr_confusion: bool,
    /// 混淆准入名单的用户补充对：每项恰好 2 个不同字符（如 "0D" 表示 0↔D），
    /// 非法配置立即失败（fail-open + 大声 log），不静默吞。
    pub extra_confusion_pairs: Vec<String>,
    /// 重度乱码表的视觉重转写层（opt-in）。机械检测器（词典覆盖率塌方）选定目标，
    /// 视觉 LLM 对照 img_path 截图逐单元格重转写，闸门 + 全量 provenance，可程序化撤销。
    /// 开启时必须提供 image_dir/load_image（取表格截图），否则按配置错误 fail-open。
    pub rewrite_garbled_tables: bool,
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

/// 含混淆层配置的缓存 key：flag/混淆 prompt 版本/补充对都改变输出，必须进 key——
/// 否则开关不同的两次调用会互相污染缓存。关 flag 时与 cache_key_for 完全一致。
/// 补充对先排序：语义相同但顺序不同的配置必须命中同一份缓存。
pub fn cache_key_for_opts(sha256: &str, opts: &RefineOptions) -> String {
    let mut key = cache_key_for(sha256);
    if opts.fix_ocr_confusion {
        let mut pairs = opts.extra_confusion_pairs.clone();
        pairs.sort();
        key = format!(
            "{key}:confusion-{CONFUSION_PROMPT_VERSION}:{}",
            pairs.join(",")
        );
    }
    if opts.rewrite_garbled_tables {
        key = format!("{key}:garbled-{GARBLED_PROMPT_VERSION}");
    }
    key
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

    async fn transcribe_table(&self, _: &[u8], _: &str) -> Result<TableTranscription, LlmError> {
        Err(LlmError(self.0.clone()))
    }
}

pub async fn refine(items: Vec<MineruItem>, opts: RefineOptions) -> RefineResult {
    let log: Logger = opts.log.clone().unwrap_or_else(default_logger);

    let key = opts.sha256.as_ref().map(|s| cache_key_for_opts(s, &opts));
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
    // 混淆层配置先验证（早抛）：配置错误不该烧掉任何 token 才暴露
    let confusion_table = if opts.fix_ocr_confusion {
        Some(
            ConfusionTable::build(&opts.extra_confusion_pairs)
                .map_err(|e| format!("extraConfusionPairs 配置非法: {e}"))?,
        )
    } else {
        None
    };

    let (mut ref_items, next_id) = assign_ids(snapshot);

    // 机械清洗 pass（确定性、自校验、不打 LLM）。先于基线快照执行：
    // 后续所有闸门（保真/异常数单调）都以清洗后的 items 为基准。
    let mech = mechanical_clean(&mut ref_items, log);

    let load_image: Option<Arc<dyn LoadImage>> = opts.load_image.clone().or_else(|| {
        opts.image_dir
            .as_ref()
            .map(|d| ImageDirLoader::new(d.clone()) as Arc<dyn LoadImage>)
    });
    let has_vision = load_image.is_some();

    // 重转写层配置先验证（早抛）：没有图片访问器就无从对照图像，是调用方配置错误
    if opts.rewrite_garbled_tables && !has_vision {
        return Err("rewriteGarbledTables 需要 imageDir 或 loadImage（取表格截图对照）".into());
    }

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
            chat: chat.clone(),
            load_image: load_image.clone(),
            vision: vision.clone(),
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

    // ── 乱码表视觉重转写层（opt-in）：出口闸门之后、混淆层之前运行——
    // 整表重转写先把废表救回，混淆层再在干净文本上做稀疏定点修正。
    let (items, garbled) = apply_garbled_layer(
        opts.rewrite_garbled_tables,
        loop_result.items,
        vision,
        load_image,
        opts.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        log,
    )
    .await;

    // ── 混淆修正层（opt-in）：出口闸门之后运行，核心承诺已经定格。
    let (items, confusion) = apply_confusion_layer(
        confusion_table,
        items,
        chat,
        opts.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        log,
    )
    .await;

    let mut token_usage = loop_result.token_usage;
    token_usage.prompt += garbled.usage.prompt + confusion.usage.prompt;
    token_usage.completion += garbled.usage.completion + confusion.usage.completion;

    // 两层修复逐条登记 provenance（层只产出 fixes，溯源条目格式由编排层统一）
    let mut provenance: Vec<ProvenanceEntry> = garbled
        .fixes
        .iter()
        .map(|f| ProvenanceEntry {
            item_id: f.item_id.clone(),
            field: "table_body".into(),
            char_start: f.char_start,
            char_end: f.char_end,
            origin: "garbled_table".into(),
            op: "rewriteCell".into(),
            confidence: 1.0,
            note: Some(format!(
                "r{}c{}「{}」→「{}」",
                f.row, f.col, f.before, f.after
            )),
        })
        .collect();
    provenance.extend(confusion.fixes.iter().map(|f| ProvenanceEntry {
        item_id: f.item_id.clone(),
        field: f.field.clone(),
        char_start: f.char_offset,
        char_end: f.char_offset + 1,
        origin: "ocr_confusion".into(),
        op: "fixConfusion".into(),
        confidence: 1.0,
        note: Some(format!("「{}」→「{}」（{}）", f.before, f.after, f.note)),
    }));

    // 机械清洗的统计并入报告：opCounts 用 mech* 前缀区分，removedSpans 置于最前
    let mut op_counts = loop_result.op_counts;
    for (k, v) in mech.counts {
        *op_counts.entry(k).or_insert(0) += v;
    }
    let mut removed_spans = mech.removed_spans;
    removed_spans.extend(loop_result.removed_spans);

    Ok(RefineResult {
        items: strip_ids(&items), // 出口剥除内部 ID（schema 透明）
        provenance,
        report: RefineReport {
            iterations: loop_result.iterations,
            op_counts,
            dismissed: loop_result.dismissed,
            removed_spans,
            violations: loop_result.violations,
            token_usage,
            fail_open: false,
            confusion_fixes: confusion.fixes,
            confusion_rejected: confusion.rejected,
            confusion_observations: confusion.observations,
            table_rewrites: garbled.fixes,
            table_rewrite_rejected: garbled.rejected,
        },
    })
}

/// 乱码表视觉重转写层（opt-in）。flag 关 → 原样直通；层内 panic → 丢弃整层、保留核心产物。
async fn apply_garbled_layer(
    enabled: bool,
    items: Vec<RefItem>,
    vision: Option<Arc<dyn VisionClient>>,
    load_image: Option<Arc<dyn LoadImage>>,
    concurrency: usize,
    log: &Logger,
) -> (Vec<RefItem>, GarbledOutcome) {
    if !enabled {
        return (items, GarbledOutcome::default());
    }
    // refine_inner 已早抛校验过 has_vision；这里的 expect 只兜内部不变量
    let vision = vision.expect("重转写层内部错误：vision 未构造");
    let load_image = load_image.expect("重转写层内部错误：load_image 未构造");

    // 进层前留快照：panic 时整层丢弃，原件返还
    let attempt = AssertUnwindSafe(rewrite_garbled_tables(
        items.clone(),
        vision,
        load_image,
        concurrency,
        log,
    ))
    .catch_unwind()
    .await;
    match attempt {
        Ok((fixed, outcome)) => (fixed, outcome),
        Err(_) => {
            log("重转写层异常 —— 丢弃本层全部结果，保留核心产物");
            (items, GarbledOutcome::default())
        }
    }
}

/// 混淆修正层（opt-in）。flag 关 → 原样直通；层内 panic → 丢弃整层、保留核心产物。
async fn apply_confusion_layer(
    table: Option<ConfusionTable>,
    items: Vec<RefItem>,
    chat: Arc<dyn ChatClient>,
    concurrency: usize,
    log: &Logger,
) -> (Vec<RefItem>, ConfusionOutcome) {
    let Some(table) = table else {
        return (items, ConfusionOutcome::default());
    };

    // 进层前留快照：panic 时整层丢弃，原件返还
    let attempt = AssertUnwindSafe(fix_confusions(
        items.clone(),
        chat,
        concurrency,
        &table,
        log,
    ))
    .catch_unwind()
    .await;
    match attempt {
        Ok((fixed, outcome)) => (fixed, outcome),
        Err(_) => {
            log("混淆层异常 —— 丢弃本层全部结果，保留核心产物");
            (items, ConfusionOutcome::default())
        }
    }
}
