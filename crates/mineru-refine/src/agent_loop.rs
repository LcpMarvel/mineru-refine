// 确定性外层循环：弹出 worklist 疑点 → 交 LLM（带上下文）→
// LLM 回一个 op 或 dismiss → 执行（保真闸+回滚）→ 重探测。loop-until-dry + 守卫。
// LLM 不当司机：每个疑点一个独立小对话，工具集固定，tool_choice:required。

use crate::detect::{detect, droppable_ids};
use crate::id::{IdGen, index_of_id, must_index_of_id};
use crate::invariant::{input_pages, table_rows};
use crate::llm::{ChatClient, LlmError, LoadImage, Message, VisionClient, parse_json_safe};
use crate::ops::{ApplyContext, ApplyResult, RejectKind, apply_op_checked};
use crate::types::{OpCall, RefItem, RemovedSpan, StripPattern, SuspectKind, TokenUsage, WorkItem};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

pub fn default_logger() -> Logger {
    Arc::new(|m: &str| eprintln!("[mineru-refine] {m}"))
}

pub struct LoopResult {
    pub items: Vec<RefItem>,
    pub iterations: u64,
    pub op_counts: BTreeMap<String, u64>,
    pub dismissed: u64,
    pub removed_spans: Vec<RemovedSpan>,
    pub violations: u64,
    pub token_usage: TokenUsage,
}

pub struct LoopOptions {
    /// 外层硬上限（防永不终止的守卫）
    pub max_iterations: u64,
    /// 单疑点内层对话轮数上限
    pub max_rounds_per_suspect: u32,
    /// 同批并行裁决的疑点数（1 = 严格串行）
    pub concurrency: usize,
    pub chat: Arc<dyn ChatClient>,
    /// split_table 仅视觉裁决：提供时走 Qwen-VL，取不到图/视觉失败 → 搁置该疑点（不回退文本）；
    /// 不提供 = 无视觉模型，split_table 整体跳过
    pub load_image: Option<Arc<dyn LoadImage>>,
    pub vision: Option<Arc<dyn VisionClient>>,
    pub log: Logger,
}

pub const DEFAULT_MAX_ITERATIONS: u64 = 48;
pub const DEFAULT_MAX_ROUNDS: u32 = 8;
pub const DEFAULT_CONCURRENCY: usize = 8;

// ── 工具定义（op 全集 → DeepSeek function schema）──

static TOOLS: LazyLock<Value> = LazyLock::new(|| {
    let id_param = serde_json::json!({
        "type": "string",
        "description": "item 的稳定 ID（如 it_0003），来自疑点描述或观察工具"
    });
    let id_a = |desc: &str| {
        let mut p = id_param.clone();
        p["description"] = Value::String(desc.to_string());
        p
    };
    serde_json::json!([
        // 观察类（只读）
        { "type": "function", "function": {
            "name": "outline",
            "description": "返回全文标题骨架：所有 header / 带 text_level 的块的 ID、层级、文本。用于判断某块在章节结构中的位置。",
            "parameters": { "type": "object", "properties": {} },
        }},
        { "type": "function", "function": {
            "name": "getItems",
            "description": "查看某 item 及其前后相邻块的完整内容（含类型、页码、文本全文）。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "before": { "type": "integer", "description": "向前取几个相邻块，默认 1" },
                "after": { "type": "integer", "description": "向后取几个相邻块，默认 1" },
            }, "required": ["id"] },
        }},
        { "type": "function", "function": {
            "name": "whyFlagged",
            "description": "查看探测器为何标记某块（该块当前所有疑点及证据）。",
            "parameters": { "type": "object", "properties": { "id": id_param }, "required": ["id"] },
        }},
        { "type": "function", "function": {
            "name": "peekPage",
            "description": "查看某块所在页及上下页的全部块（跨页判断必需：merge 前必须用它确认上下页内容连续）。",
            "parameters": { "type": "object", "properties": { "id": id_param }, "required": ["id"] },
        }},
        // 裁决类
        { "type": "function", "function": {
            "name": "dismiss",
            "description": "判定当前疑点为误报，不做任何改动。拿不准时宁可 dismiss，不可错改/误删真标题。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "reason": { "type": "string", "description": "为何是误报" },
            }, "required": ["id", "reason"] },
        }},
        // 变更类（7 个削减/重组 op）
        { "type": "function", "function": {
            "name": "merge",
            "description": "把两个 text 块拼成一块（修跨页断句）。idB 须在 idA 之后，两者之间只允许隔页眉/页码/页脚（页面家具会原位保留）。合并前必须先 peekPage 确认上下页内容连续。",
            "parameters": { "type": "object", "properties": {
                "idA": id_a("前块 ID"),
                "idB": id_a("紧随其后的块 ID"),
            }, "required": ["idA", "idB"] },
        }},
        { "type": "function", "function": {
            "name": "split",
            "description": "把一个 text 块在字符 offset 处切成两块（拆巨型块）。offset 是 text 中的字符位置（0 < offset < 长度），应切在自然段/小标题边界。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "offset": { "type": "integer", "description": "切分点字符位置" },
            }, "required": ["id", "offset"] },
        }},
        { "type": "function", "function": {
            "name": "demote",
            "description": "把被误判为标题的块降级为正文（清除 text_level）。",
            "parameters": { "type": "object", "properties": { "id": id_param }, "required": ["id"] },
        }},
        { "type": "function", "function": {
            "name": "promote",
            "description": "把 text 块升为标题（设 text_level=level，1 最高）。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "level": { "type": "integer", "description": "标题层级 1-6" },
            }, "required": ["id", "level"] },
        }},
        { "type": "function", "function": {
            "name": "reorder",
            "description": "重排一段连续区间内的块顺序（修跨页错序）。传入这些块 ID 的正确顺序，它们必须在文档中本就连续。",
            "parameters": { "type": "object", "properties": {
                "idsInOrder": { "type": "array", "items": { "type": "string" }, "description": "按正确顺序排列的稳定 ID 列表" },
            }, "required": ["idsInOrder"] },
        }},
        { "type": "function", "function": {
            "name": "drop",
            "description": "删除混入正文的页码/页眉/页脚/水印块。只允许删被探测器标记为 page_artifact 的块。",
            "parameters": { "type": "object", "properties": { "id": id_param }, "required": ["id"] },
        }},
        { "type": "function", "function": {
            "name": "strip",
            "description": "去掉块内残留符号。pattern 白名单：md_link（[文字](url)→文字）、latex_dollar（$\\mathsf{x}$→x 去定界符和命令残骸）、latex_block（整段 $...$ 删除）、latex_command（删无定界符的裸 \\命令 和花括号残骸）、escaped_dollar（\\$→$ 去转义反斜杠，如 \\$APPEALS）、html_tag（删 HTML 标签）。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "pattern": { "type": "string", "enum": ["md_link", "latex_dollar", "latex_block", "latex_command", "escaped_dollar", "html_tag"] },
            }, "required": ["id", "pattern"] },
        }},
        // 注：mergeTable 不在文本工具集里——split_table 仅视觉裁决（try_vision_verdict），
        // 文本 LLM 不该在任何疑点对话里合并表格（首末行摘要不足以核对行归属）。
        { "type": "function", "function": {
            "name": "mergeList",
            "description": "把跨页被拆的两个 list 合成一个（list_items 拼接）。idB 须在 idA 之后，中间只允许隔页眉/页码/页脚。若 A 的尾项在页边界被截断、B 的首项是它的延续（尾项无句末标点且首项非新条目特征），传 joinSeam=true 把两项缝成一项。",
            "parameters": { "type": "object", "properties": {
                "idA": id_a("前 list ID"),
                "idB": id_a("续 list ID"),
                "joinSeam": { "type": "boolean", "description": "A 尾项与 B 首项是否缝成一项（断句跨页时 true），默认 false" },
            }, "required": ["idA", "idB"] },
        }},
    ])
});

/// 公开工具集（测试/调试用）。
pub fn tools() -> &'static Value {
    &TOOLS
}

const OP_NAMES: [&str; 8] = [
    "merge",
    "split",
    "demote",
    "promote",
    "reorder",
    "drop",
    "strip",
    "mergeList",
];
const OBSERVE_NAMES: [&str; 4] = ["outline", "getItems", "whyFlagged", "peekPage"];

// system prompt 稳定不变（放 messages 前缀吃 DeepSeek prefix cache）。
const SYSTEM_PROMPT: &str = r#"你是 MinerU PDF 解析结果的结构修复器（linter/fixer）。文档被解析成块（item）数组，每块有稳定 ID、类型（text/header/table/list/page_number/image）、页码 page_idx 和文本。

你的任务：对【当前疑点】做一次裁决。你只能调用工具，绝不输出正文文本。

规则：
1. 不确定就先观察：getItems 看上下文、peekPage 看整页、outline 看章节骨架、whyFlagged 看证据。
2. 跨页 merge 前【必须】先 peekPage 确认上下页内容确实连续（中间无标题/表格/无关块）。
3. 拿不准就 dismiss（宁可漏修，不可错改/误删真标题）。
   - 伪标题裁决前先看 outline：若存在结构平行的同级编号标题（如 4.1/4.2/4.3…，即使含逗号或表引用），通常是真标题 → dismiss。
   - 漏标标题（missed_heading）promote 前先看 outline，level 必须与同级编号兄弟标题一致。
   - 列表项（-、•、①、(1) 等开头的行）之间绝不 merge——行尾无标点是列表的常态，不是断句。
   - 但 page_artifact 证据若给出「已分类页眉/页脚同文佐证」，说明同文块在别处已被正确分类为页面家具，该块就是漏标的同款 → 应 drop，不要因「像标题」而 dismiss。
   - 同一文本的多处 page_artifact 疑点应裁决一致：要删都删，不要删一处留其余。
4. 修复只许削减/重组（merge/split/demote/promote/reorder/drop/strip/mergeList），系统会机器校验"不新增任何字符、表格行不被篡改"，违规会被自动回滚。
5. 每个疑点最终以【一个】变更 op 或 dismiss 收尾。"#;

// ── 观察工具实现（确定性，只读）──

fn char_prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn fmt_item(r: &RefItem, max_text: usize) -> String {
    let it = &r.item;
    let mut fields: Vec<String> = vec![
        format!("id={}", r.id),
        format!("type={}", it.item_type()),
        format!(
            "page={}",
            it.page_idx()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "undefined".into())
        ),
    ];
    if let Some(level) = it.text_level() {
        fields.push(format!("text_level={level}"));
    }
    if let Some(t) = it.text() {
        let char_len = t.chars().count();
        let shown = if char_len > max_text {
            format!("{}…(共{char_len}字)", char_prefix(t, max_text))
        } else {
            t.to_string()
        };
        fields.push(format!("text=「{shown}」"));
    }
    if let Some(v) = it.0.get("list_items")
        && v.is_array()
    {
        fields.push(format!("list_items={}", char_prefix(&v.to_string(), 300)));
    }
    if let Some(v) = it.0.get("table_caption")
        && v.is_array()
    {
        fields.push(format!("table_caption={v}"));
    }
    if let Some(body) = it.table_body() {
        // 表格只给首末行摘要：足够判断跨页连续性（mergeTable），又不撑爆上下文
        let rows = table_rows(body);
        if rows.is_empty() {
            fields.push("table_body=(空壳，0 行)".into());
        } else {
            let summary = if rows.len() == 1 {
                format!("首行「{}」", char_prefix(rows[0], 200))
            } else {
                format!(
                    "首行「{}」 末行「{}」",
                    char_prefix(rows[0], 200),
                    char_prefix(rows[rows.len() - 1], 200)
                )
            };
            fields.push(format!("table_body=({} 行) {summary}", rows.len()));
        }
    }
    if let Some(p) = it.img_path() {
        fields.push(format!("img_path={p}"));
    }
    fields.join(" | ")
}

fn arg_str(args: &Value, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".into(),
    }
}

fn exec_observe(
    name: &str,
    args: &Value,
    items: &[RefItem],
    worklist: &[WorkItem],
) -> Result<String, String> {
    match name {
        "outline" => {
            // 注意 MinerU 的 type=header 是页眉而非标题；文档标题 = text + text_level
            let heads: Vec<&RefItem> = items
                .iter()
                .filter(|r| r.item.text_level().is_some())
                .collect();
            if heads.is_empty() {
                return Ok("（全文没有任何标题块）".into());
            }
            Ok(heads
                .iter()
                .map(|r| {
                    format!(
                        "{} L{} {}",
                        r.id,
                        r.item
                            .text_level()
                            .map(|l| l.to_string())
                            .unwrap_or_else(|| "?".into()),
                        char_prefix(r.item.text().unwrap_or(""), 60)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "getItems" => {
            let id = arg_str(args, "id");
            let i = must_index_of_id(items, &id)?;
            let clamp = |v: Option<&Value>, default: i64| -> usize {
                v.and_then(Value::as_i64).unwrap_or(default).clamp(0, 5) as usize
            };
            let before = clamp(args.get("before"), 1);
            let after = clamp(args.get("after"), 1);
            let lo = i.saturating_sub(before);
            let hi = (i + after).min(items.len() - 1);
            Ok(items[lo..=hi]
                .iter()
                .map(|r| {
                    if r.id == id {
                        format!(">>> {}", fmt_item(r, 2000))
                    } else {
                        format!("    {}", fmt_item(r, 600))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "whyFlagged" => {
            let id = arg_str(args, "id");
            let flags: Vec<&WorkItem> = worklist.iter().filter(|w| w.item_id == id).collect();
            if flags.is_empty() {
                return Ok(format!("{id} 当前没有疑点。"));
            }
            Ok(flags
                .iter()
                .map(|w| {
                    format!(
                        "[{}]{} {}",
                        w.kind.as_str(),
                        if w.has_op {
                            ""
                        } else {
                            "（仅标记，无对应 op，只能 dismiss）"
                        },
                        w.evidence
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "peekPage" => {
            let id = arg_str(args, "id");
            let i = must_index_of_id(items, &id)?;
            let Some(page) = items[i].item.page_idx() else {
                return Ok(format!("{id} 没有 page_idx。"));
            };
            let mut lines: Vec<String> = Vec::new();
            for p in [page - 1, page, page + 1] {
                let in_page: Vec<&RefItem> = items
                    .iter()
                    .filter(|r| r.item.page_idx() == Some(p))
                    .collect();
                if in_page.is_empty() {
                    continue;
                }
                lines.push(format!("── 第 {p} 页 ──"));
                for r in in_page {
                    lines.push(if r.id == id {
                        format!(">>> {}", fmt_item(r, 600))
                    } else {
                        format!("    {}", fmt_item(r, 600))
                    });
                }
            }
            Ok(lines.join("\n"))
        }
        _ => Err(format!("未知观察工具: {name}")),
    }
}

fn to_op_call(name: &str, args: &Value) -> Result<OpCall, String> {
    let int_of = |key: &str| -> Result<i64, String> {
        args.get(key).and_then(Value::as_i64).ok_or_else(|| {
            format!(
                "{key} 必须是整数（实际: {}）",
                args.get(key).cloned().unwrap_or(Value::Null)
            )
        })
    };
    match name {
        "merge" => Ok(OpCall::Merge {
            id_a: arg_str(args, "idA"),
            id_b: arg_str(args, "idB"),
        }),
        "split" => Ok(OpCall::Split {
            id: arg_str(args, "id"),
            offset: int_of("offset")?,
        }),
        "demote" => Ok(OpCall::Demote {
            id: arg_str(args, "id"),
        }),
        "promote" => Ok(OpCall::Promote {
            id: arg_str(args, "id"),
            level: int_of("level")?,
        }),
        "reorder" => {
            let ids = args
                .get("idsInOrder")
                .and_then(Value::as_array)
                .ok_or("idsInOrder 必须是字符串数组")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect();
            Ok(OpCall::Reorder { ids_in_order: ids })
        }
        "drop" => Ok(OpCall::Drop {
            id: arg_str(args, "id"),
        }),
        "strip" => {
            let pattern: StripPattern =
                serde_json::from_value(args.get("pattern").cloned().unwrap_or(Value::Null))
                    .map_err(|_| {
                        format!("strip pattern 不在白名单：{}", arg_str(args, "pattern"))
                    })?;
            Ok(OpCall::Strip {
                id: arg_str(args, "id"),
                pattern,
            })
        }
        "mergeList" => Ok(OpCall::MergeList {
            id_a: arg_str(args, "idA"),
            id_b: arg_str(args, "idB"),
            join_seam: Some(
                args.get("joinSeam")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
        }),
        _ => Err(format!("未知 op: {name}")),
    }
}

/// 防震荡：禁止刚做过的逆操作。merge 产物禁 split；split 产物对禁 merge。
#[derive(Default)]
struct OscillationGuard {
    banned_split_ids: Mutex<HashSet<String>>,   // merge 产物
    banned_merge_pairs: Mutex<HashSet<String>>, // split 产物对 "idA+idB"
}

impl OscillationGuard {
    fn record(&self, call: &OpCall, new_ids: &[String]) {
        match call {
            OpCall::Merge { .. } => {
                if let Some(first) = new_ids.first() {
                    self.banned_split_ids.lock().unwrap().insert(first.clone());
                }
            }
            OpCall::Split { .. } if new_ids.len() == 2 => {
                self.banned_merge_pairs
                    .lock()
                    .unwrap()
                    .insert(format!("{}+{}", new_ids[0], new_ids[1]));
            }
            _ => {}
        }
    }

    fn rejects(&self, call: &OpCall) -> Option<String> {
        match call {
            OpCall::Split { id, .. } if self.banned_split_ids.lock().unwrap().contains(id) => Some(
                format!("{id} 是刚 merge 出来的块，禁止立刻 split（防震荡）"),
            ),
            OpCall::Merge { id_a, id_b }
                if self
                    .banned_merge_pairs
                    .lock()
                    .unwrap()
                    .contains(&format!("{id_a}+{id_b}")) =>
            {
                Some(format!(
                    "{id_a}+{id_b} 是刚 split 出来的块对，禁止立刻 merge 回去（防震荡）"
                ))
            }
            _ => None,
        }
    }
}

fn suspect_key(w: &WorkItem) -> String {
    format!("{}:{}", w.kind.as_str(), w.item_id)
}

/// 无视觉模型（未提供 load_image）时 split_table 不裁决：
/// 表格行级保真只信图像证据，不让纯文本路径做 mergeTable。
pub fn skipped_without_vision(w: &WorkItem, has_load_image: bool) -> bool {
    w.kind == SuspectKind::SplitTable && !has_load_image
}

// ── 主循环 ──

struct SharedTokens {
    prompt: AtomicU64,
    completion: AtomicU64,
}

struct SuspectCtx {
    next_id: IdGen,
    valid_pages: HashSet<i64>,
    chat: Arc<dyn ChatClient>,
    max_rounds: u32,
    guard: OscillationGuard,
    tokens: SharedTokens,
    log: Logger,
    load_image: Option<Arc<dyn LoadImage>>,
    vision: Option<Arc<dyn VisionClient>>,
}

enum SuspectOutcome {
    Applied {
        op_name: String,
        removed_spans: Vec<RemovedSpan>,
    },
    Dismissed {
        reason: &'static str,
        violations: u64,
    },
}

pub async fn run_loop(
    initial: Vec<RefItem>,
    next_id: IdGen,
    opts: LoopOptions,
) -> Result<LoopResult, LlmError> {
    let max_iterations = opts.max_iterations;
    let log = opts.log.clone();
    let valid_pages = {
        let refs: Vec<&crate::types::MineruItem> = initial.iter().map(|r| &r.item).collect();
        input_pages(&refs)
    };
    let concurrency = opts.concurrency.max(1);

    // 共享文档状态：并行对话各自观察/落 op 都读写它。op 落地（apply_op_checked →
    // 持锁替换 items）是原子的；并行对话间的冲突（目标 ID 已被别的 op 吃掉）表现为
    // invalid_args，作为工具结果反馈给 LLM，由它改判或 dismiss。
    let state: Arc<Mutex<Vec<RefItem>>> = Arc::new(Mutex::new(initial));
    let mut dismissed_keys: HashSet<String> = HashSet::new(); // 误报裁决集（防永不终止）
    let ctx = Arc::new(SuspectCtx {
        next_id,
        valid_pages,
        chat: opts.chat,
        max_rounds: opts.max_rounds_per_suspect,
        guard: OscillationGuard::default(),
        tokens: SharedTokens {
            prompt: AtomicU64::new(0),
            completion: AtomicU64::new(0),
        },
        log: log.clone(),
        load_image: opts.load_image,
        vision: opts.vision,
    });
    let mut op_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut removed_spans: Vec<RemovedSpan> = Vec::new();
    let mut violations: u64 = 0;
    let mut iterations: u64 = 0;
    // 至少一次成功 → 后续单点 LLM 故障只搁置疑点；全程零成功 → 上抛触发 fail-open
    let mut llm_successes: u64 = 0;
    let has_vision = ctx.load_image.is_some();
    let mut warned_visionless = false;

    // loop-until-dry：worklist（有 op、未 dismiss、未因无视觉模型跳过）弹空才到底
    while iterations < max_iterations {
        let worklist = {
            let items = state.lock().unwrap();
            detect(&items)
        };
        if !has_vision && !warned_visionless {
            let skipped = worklist
                .iter()
                .filter(|w| skipped_without_vision(w, has_vision))
                .count();
            if skipped > 0 {
                warned_visionless = true;
                log(&format!(
                    "未提供视觉模型（loadImage），跳过 {skipped} 个 split_table 疑点，不做 mergeTable"
                ));
            }
        }
        let actionable: Vec<WorkItem> = worklist
            .iter()
            .filter(|w| {
                w.has_op
                    && !dismissed_keys.contains(&suspect_key(w))
                    && !skipped_without_vision(w, has_vision)
            })
            .cloned()
            .collect();
        if actionable.is_empty() {
            break;
        }

        // 一批最多 concurrency 个疑点并行裁决（不同位置的块相互独立，这是主要提速来源）
        let batch_size = concurrency.min((max_iterations - iterations) as usize);
        let batch: Vec<WorkItem> = actionable.into_iter().take(batch_size).collect();
        iterations += batch.len() as u64;

        let worklist = Arc::new(worklist);
        let futures = batch
            .iter()
            .map(|target| {
                let target = target.clone();
                let state = state.clone();
                let worklist = worklist.clone();
                let ctx = ctx.clone();
                async move {
                    let outcome = handle_suspect(&target, &state, &worklist, &ctx).await;
                    (target, outcome)
                }
            })
            .collect::<Vec<_>>();
        let results = futures::future::join_all(futures).await;

        let mut llm_errors: Vec<LlmError> = Vec::new();
        for (target, outcome) in results {
            match outcome {
                Err(e) => {
                    // 单疑点 LLM 故障（重试耗尽）：搁置该疑点，不毁全局（其它并行对话照常收尾）
                    log(&format!(
                        "疑点 {} LLM 调用失败，搁置: {e}",
                        suspect_key(&target)
                    ));
                    dismissed_keys.insert(suspect_key(&target));
                    llm_errors.push(e);
                }
                Ok(outcome) => {
                    llm_successes += 1;
                    match outcome {
                        SuspectOutcome::Applied {
                            op_name,
                            removed_spans: spans,
                        } => {
                            *op_counts.entry(op_name).or_insert(0) += 1;
                            removed_spans.extend(spans);
                        }
                        SuspectOutcome::Dismissed {
                            reason,
                            violations: v,
                        } => {
                            // dismiss（LLM 主动 / 轮数耗尽 / op 被闸门回滚后放弃）→ 计入裁决集，重探测不再标记
                            dismissed_keys.insert(suspect_key(&target));
                            violations += v;
                            if reason != "llm_dismiss" {
                                log(&format!("疑点 {} 强制搁置: {reason}", suspect_key(&target)));
                            }
                        }
                    }
                }
            }
        }
        // LLM 整体不可用（全程一次都没成功过）→ 上抛，由 refine() fail-open（原样返回输入）
        if !llm_errors.is_empty() && llm_successes == 0 {
            return Err(llm_errors.into_iter().next().unwrap());
        }
    }

    if iterations >= max_iterations {
        log(&format!("到达 maxIterations={max_iterations}，守卫强停"));
    }

    let items = Arc::try_unwrap(state)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone());
    Ok(LoopResult {
        items,
        iterations,
        op_counts,
        dismissed: dismissed_keys.len() as u64,
        removed_spans,
        violations,
        token_usage: TokenUsage {
            prompt: ctx.tokens.prompt.load(Ordering::Relaxed),
            completion: ctx.tokens.completion.load(Ordering::Relaxed),
        },
    })
}

static NEXT_ID_IN_EVIDENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"后块=(it_\d+)").unwrap());

/// split_table 的视觉裁决（唯一路径）：把 A/B 两表的 MinerU 裁剪图交给 Qwen-VL 判
/// "是否同一张表被分页拆开"。仅输出决策，merge 仍走 apply_op_checked 保真闸。
/// 返回 None = 此路不通（无图/无 key/判决 op 被闸门拒）→ 调用方搁置该疑点，
/// 不回退文本：纯文本（首末行摘要）不足以核对表格行的真实归属，错合比漏合更糟。
async fn try_vision_verdict(
    target: &WorkItem,
    state: &Arc<Mutex<Vec<RefItem>>>,
    worklist: &[WorkItem],
    ctx: &SuspectCtx,
) -> Option<SuspectOutcome> {
    if target.kind != SuspectKind::SplitTable {
        return None;
    }
    let load_image = ctx.load_image.as_ref()?;
    let vision = ctx.vision.as_ref()?;
    let id_a = target.item_id.clone();
    let id_b = NEXT_ID_IN_EVIDENCE
        .captures(&target.evidence)?
        .get(1)?
        .as_str()
        .to_string();

    let (path_a, path_b) = {
        let items = state.lock().unwrap();
        let ia = index_of_id(&items, &id_a)?;
        let ib = index_of_id(&items, &id_b)?; // 块已被并行对话吃掉 → 搁置
        let pa = items[ia].item.img_path()?.to_string();
        let pb = items[ib].item.img_path()?.to_string();
        if pa.is_empty() || pb.is_empty() {
            return None;
        }
        (pa, pb)
    };

    let (img_a, img_b) = futures::join!(load_image.load(&path_a), load_image.load(&path_b));
    let (img_a, img_b) = (img_a?, img_b?);
    let v = match vision.judge_split_table(&img_a, &img_b).await {
        Ok(v) => v,
        Err(e) => {
            (ctx.log)(&format!("视觉裁决失败（{e}），搁置"));
            return None;
        }
    };
    ctx.tokens
        .prompt
        .fetch_add(v.usage.prompt_tokens, Ordering::Relaxed);
    ctx.tokens
        .completion
        .fetch_add(v.usage.completion_tokens, Ordering::Relaxed);

    if !v.merge {
        (ctx.log)(&format!(
            "视觉 dismiss [split_table] {id_a}+{id_b}: {}",
            v.reason
        ));
        return Some(SuspectOutcome::Dismissed {
            reason: "llm_dismiss",
            violations: 0,
        });
    }

    let call = OpCall::MergeTable {
        id_a: id_a.clone(),
        id_b: id_b.clone(),
    };
    let droppable = droppable_ids(worklist);
    let mut items = state.lock().unwrap();
    let result = apply_op_checked(
        &items,
        &call,
        &ApplyContext {
            next_id: &ctx.next_id,
            valid_pages: &ctx.valid_pages,
            droppable_ids: Some(&droppable),
        },
    );
    match result {
        ApplyResult::Ok {
            items: new_items,
            removed_spans,
            ..
        } => {
            *items = new_items;
            (ctx.log)(&format!("视觉 mergeTable {id_a}+{id_b}: {}", v.reason));
            Some(SuspectOutcome::Applied {
                op_name: "mergeTable".into(),
                removed_spans,
            })
        }
        ApplyResult::Rejected { reason, .. } => {
            (ctx.log)(&format!("视觉判 merge 但 op 被拒（{reason}），搁置"));
            None
        }
    }
}

fn op_hint(kind: SuspectKind) -> &'static str {
    match kind {
        SuspectKind::PseudoHeading => "确认是被误判的正文 → demote；确认是真标题 → dismiss",
        SuspectKind::CrossPageBreak => "确认上下页内容连续 → merge；不连续 → dismiss",
        SuspectKind::GiantBlock => "找到自然边界 → split；本就是一整段 → dismiss",
        SuspectKind::PageArtifact => {
            "确认是页码/页眉/页脚/水印（非正文）→ drop；是正文 → dismiss。证据含「家具佐证」的基本可直接 drop"
        }
        SuspectKind::ResidualMarkup => {
            "确认是解析残留 → strip（选对 pattern：$...$ 用 latex_dollar、裸 \\命令{} 用 latex_command、\\$ 用 escaped_dollar）；本就该有 → dismiss"
        }
        SuspectKind::EmptyTable => {
            "确认是零内容空壳（无行无字无图）→ drop；探测器已验证为空，一般可直接 drop"
        }
        SuspectKind::SplitList => {
            "确认两 list 是同一列表被分页拆开 → mergeList（A 尾项被截断、B 首项是其延续时 joinSeam=true）；各自独立 → dismiss"
        }
        SuspectKind::MissedHeading => {
            "先 outline 确认证据中的同级编号兄弟确实是标题 → promote（level 与兄弟标题一致）；本块其实是正文/列表项 → dismiss"
        }
        SuspectKind::TrailingMarker => {
            "确认段尾的「[相关文件]」类标记是被粘连的独立结构块 → split（offset 用证据中的建议值）；标记本属句子内容 → dismiss"
        }
        SuspectKind::SeparatedCaption => {
            "用 getItems/peekPage 判断表格归属：表格属于 caption 所在小节 → reorder 把表格挪到标题之前；caption 与表格都属新小节 → reorder 把 caption 挪到标题之后；拿不准 → dismiss"
        }
        _ => "无对应 op，只能 dismiss（仅标记类）",
    }
}

async fn handle_suspect(
    target: &WorkItem,
    state: &Arc<Mutex<Vec<RefItem>>>,
    worklist: &[WorkItem],
    ctx: &SuspectCtx,
) -> Result<SuspectOutcome, LlmError> {
    // split_table 仅视觉裁决，此路不通就搁置——绝不落到文本路径
    if target.kind == SuspectKind::SplitTable {
        let vision_outcome = try_vision_verdict(target, state, worklist, ctx).await;
        return Ok(vision_outcome.unwrap_or(SuspectOutcome::Dismissed {
            reason: "vision_unavailable",
            violations: 0,
        }));
    }

    // 上下文前置：把裁决最可能需要的观察结果直接放进首条消息，省掉 1-2 轮观察往返。
    // 跨页疑点预载整页上下文（等价 peekPage），其余预载 ±2 邻居（等价 getItems）。
    let cross_page = matches!(
        target.kind,
        SuspectKind::CrossPageBreak | SuspectKind::SplitList
    );
    let (preload, current) = {
        let items = state.lock().unwrap();
        let id_arg = serde_json::json!({ "id": target.item_id });
        let preload = if cross_page {
            exec_observe("peekPage", &id_arg, &items, worklist)
                .map(|s| format!("所在页及上下页内容（peekPage 预载）：\n{s}"))
        } else {
            exec_observe(
                "getItems",
                &serde_json::json!({ "id": target.item_id, "before": 2, "after": 2 }),
                &items,
                worklist,
            )
            .map(|s| format!("相邻上下文（getItems ±2 预载）：\n{s}"))
        }
        .unwrap_or_else(|_| "（目标块已不存在，无法预载上下文）".into());
        let current = index_of_id(&items, &target.item_id)
            .map(|i| fmt_item(&items[i], 2000))
            .unwrap_or_else(|| "（已不存在）".into());
        (preload, current)
    };

    let mut messages: Vec<Message> = vec![
        Message::System {
            content: SYSTEM_PROMPT.to_string(),
        },
        Message::User {
            content: format!(
                "当前疑点：[{}] item {}\n证据：{}\n\n该块当前内容：\n{}\n\n{}\n\n该类疑点的典型处置：{}\n若以上上下文已足够判断，请直接给出一个变更 op 或 dismiss；不够再调观察工具（outline 看章节骨架尤其有用）。",
                target.kind.as_str(),
                target.item_id,
                target.evidence,
                current,
                preload,
                op_hint(target.kind)
            ),
        },
    ];

    let mut violation_count: u64 = 0;

    for round in 0..ctx.max_rounds {
        // 倒数第二轮起强制收敛：实测大文档上 LLM 容易反复观察不裁决，烧满轮数被搁置
        if round + 2 == ctx.max_rounds {
            messages.push(Message::User {
                content: "观察轮数即将用完。请基于已有信息【现在就裁决】：给出一个变更 op，或拿不准就 dismiss。不要再调用观察工具。"
                    .into(),
            });
        }
        // LLM 异常直接上抛，由 refine() 的 fail-open 兜（原样返回输入）
        let r = ctx.chat.chat(&messages, tools()).await?;
        ctx.tokens
            .prompt
            .fetch_add(r.usage.prompt_tokens, Ordering::Relaxed);
        ctx.tokens
            .completion
            .fetch_add(r.usage.completion_tokens, Ordering::Relaxed);

        let Some(calls) = r.message.tool_calls.filter(|c| !c.is_empty()) else {
            return Ok(SuspectOutcome::Dismissed {
                reason: "llm_no_tool_call",
                violations: violation_count,
            });
        };
        messages.push(Message::Assistant {
            content: r.message.content.clone(),
            tool_calls: Some(calls.clone()),
        });

        for call in &calls {
            let name = call.function.name.as_str();
            let Some(args) = parse_json_safe(&call.function.arguments) else {
                messages.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: format!(
                        "arguments 解析失败，请重试: {}",
                        char_prefix(&call.function.arguments, 200)
                    ),
                });
                continue;
            };

            if OBSERVE_NAMES.contains(&name) {
                let content = {
                    let items = state.lock().unwrap(); // 读最新状态（并行 op 落地后立即可见）
                    exec_observe(name, &args, &items, worklist)
                        .unwrap_or_else(|e| format!("观察失败: {e}"))
                };
                messages.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content,
                });
                continue;
            }

            if name == "dismiss" {
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("（未给理由）");
                (ctx.log)(&format!(
                    "dismiss [{}] {}: {reason}",
                    target.kind.as_str(),
                    target.item_id
                ));
                return Ok(SuspectOutcome::Dismissed {
                    reason: "llm_dismiss",
                    violations: violation_count,
                });
            }

            if OP_NAMES.contains(&name) {
                let op_call = match to_op_call(name, &args) {
                    Ok(c) => c,
                    Err(e) => {
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: format!("参数错误: {e}"),
                        });
                        continue;
                    }
                };
                if let Some(banned) = ctx.guard.rejects(&op_call) {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: format!("被拒（{banned}）。请 dismiss 或换别的 op。"),
                    });
                    continue;
                }
                let droppable = droppable_ids(worklist);
                let (rejected, outcome) = {
                    let mut items = state.lock().unwrap();
                    match apply_op_checked(
                        &items,
                        &op_call,
                        &ApplyContext {
                            next_id: &ctx.next_id,
                            valid_pages: &ctx.valid_pages,
                            droppable_ids: Some(&droppable),
                        },
                    ) {
                        ApplyResult::Ok {
                            items: new_items,
                            removed_spans,
                            new_ids,
                        } => {
                            ctx.guard.record(&op_call, &new_ids);
                            *items = new_items; // 持锁原子落地
                            (
                                None,
                                Some(SuspectOutcome::Applied {
                                    op_name: name.to_string(),
                                    removed_spans,
                                }),
                            )
                        }
                        ApplyResult::Rejected { reason, kind } => ((Some((reason, kind))), None),
                    }
                };
                if let Some(outcome) = outcome {
                    return Ok(outcome);
                }
                let (reason, kind) = rejected.unwrap();
                if kind == RejectKind::FidelityViolation {
                    violation_count += 1;
                    (ctx.log)(&format!("保真闸回滚 {name}({args}): {reason}"));
                }
                messages.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: format!(
                        "op 被拒绝（{}）: {reason}。请观察后换 op 或 dismiss。",
                        if kind == RejectKind::FidelityViolation {
                            "保真闸门回滚"
                        } else {
                            "参数非法"
                        }
                    ),
                });
                continue;
            }

            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: format!("未知工具 {name}。"),
            });
        }
    }

    Ok(SuspectOutcome::Dismissed {
        reason: "max_rounds_exhausted",
        violations: violation_count,
    })
}
