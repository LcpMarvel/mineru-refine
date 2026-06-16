// 确定性外层循环：弹出 worklist 疑点 → 交 LLM（带上下文）→
// LLM 回一个 op 或 dismiss → 执行（保真闸+回滚）→ 重探测。loop-until-dry + 守卫。
// LLM 不当司机：每个疑点一个独立小对话，工具集固定，tool_choice:required。

use crate::detect::{NumStyle, detect, droppable_caption_ids, droppable_ids, parse_numbering};
use crate::id::{IdGen, index_of_id, must_index_of_id};
use crate::invariant::{input_pages, table_rows};
use crate::llm::{
    ChatClient, LlmError, LoadImage, Message, ToolCall, VisionClient, parse_json_safe,
};
use crate::ops::{ApplyContext, ApplyResult, RejectKind, apply_op_checked};
use crate::types::{
    DismissedSuspect, OpCall, RefItem, RemovedSpan, StripPattern, SuspectKind, TokenUsage, WorkItem,
};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

pub fn default_logger() -> Logger {
    Arc::new(|m: &str| eprintln!("[mineru-refine] {m}"))
}

/// 清洗（refine）阶段的进度快照。每轮迭代吐出一次（含起点 iterations=0 与终点
/// worklist_remaining=0）。`iterations`/`max_iterations` 是迭代预算口径，
/// `worklist_remaining` 是本轮待裁决的可处理疑点数（递减趋势但非严格单调——
/// 修复会解锁新疑点），`input_suspects` 是初始可处理疑点数（分母）。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub iterations: u64,
    pub max_iterations: u64,
    pub worklist_remaining: usize,
    pub input_suspects: usize,
}

/// 可选进度回调。在 tokio 任务线程上同步调用，实现应尽量轻（如塞进 channel），
/// 不要在里面阻塞或 panic。缺省（None）时 run_loop 行为与现状逐字节一致。
pub type ProgressSink = Arc<dyn Fn(Progress) + Send + Sync>;

pub struct LoopResult {
    pub items: Vec<RefItem>,
    pub iterations: u64,
    pub op_counts: BTreeMap<String, u64>,
    pub dismissed: u64,
    pub dismissed_suspects: Vec<DismissedSuspect>,
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
    /// 可选进度回调：每轮迭代吐出一次 Progress。缺省 None 时不构造事件、不调用，
    /// 行为与现状逐字节一致。
    pub progress: Option<ProgressSink>,
    /// 初始可处理疑点数，仅作为进度事件的分母透传（run_loop 不据此做任何决策）。
    pub input_suspects: usize,
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
    // 所有变更 op 共用的裁决依据参数：拆掉「只有 dismiss 能说话」的不对称
    // （实测 LLM 想留下分析时会被吸到唯一带自由文本参数的 dismiss 上，酿成矛盾决策），
    // 同时让 op 落地在日志里可审计。
    let reason_param = serde_json::json!({
        "type": "string",
        "description": "一句话裁决依据（写入审计日志）"
    });
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
                "reason": reason_param.clone(),
            }, "required": ["idA", "idB"] },
        }},
        { "type": "function", "function": {
            "name": "split",
            "description": "把一个 text 块在字符 offset 处切成两块（拆巨型块）。offset 是 text 中的字符位置（0 < offset < 长度），应切在自然段/小标题边界。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "offset": { "type": "integer", "description": "切分点字符位置" },
                "reason": reason_param.clone(),
            }, "required": ["id", "offset"] },
        }},
        { "type": "function", "function": {
            "name": "demote",
            "description": "把被误判为标题的块降级为正文（清除 text_level）。",
            "parameters": { "type": "object", "properties": { "id": id_param, "reason": reason_param.clone() }, "required": ["id"] },
        }},
        { "type": "function", "function": {
            "name": "promote",
            "description": "把 text 块升为标题（设 text_level=level，1 最高）。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "level": { "type": "integer", "description": "标题层级 1-6" },
                "reason": reason_param.clone(),
            }, "required": ["id", "level"] },
        }},
        { "type": "function", "function": {
            "name": "reorder",
            "description": "重排一段连续区间内的块顺序（修跨页错序）。传入这些块 ID 的正确顺序，它们必须在文档中本就连续。",
            "parameters": { "type": "object", "properties": {
                "idsInOrder": { "type": "array", "items": { "type": "string" }, "description": "按正确顺序排列的稳定 ID 列表" },
                "reason": reason_param.clone(),
            }, "required": ["idsInOrder"] },
        }},
        { "type": "function", "function": {
            "name": "drop",
            "description": "删除混入正文的页码/页眉/页脚/水印块。只允许删被探测器标记为 page_artifact 的块。",
            "parameters": { "type": "object", "properties": { "id": id_param, "reason": reason_param.clone() }, "required": ["id"] },
        }},
        { "type": "function", "function": {
            "name": "strip",
            "description": "去掉块内残留符号。pattern 白名单：md_link（[文字](url)→文字）、latex_dollar（$\\mathsf{x}$→x 去定界符和命令残骸）、latex_block（整段 $...$ 删除）、latex_command（删无定界符的裸 \\命令 和花括号残骸）、escaped_dollar（\\$→$ 去转义反斜杠，如 \\$APPEALS）、html_tag（删 HTML 标签）。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "pattern": { "type": "string", "enum": ["md_link", "latex_dollar", "latex_block", "latex_command", "escaped_dollar", "html_tag"] },
                "reason": reason_param.clone(),
            }, "required": ["id", "pattern"] },
        }},
        { "type": "function", "function": {
            "name": "deleteChar",
            "description": "删除 text 块中 offset 处的单个 OCR 衍字。白名单严格：只能删与紧邻字符重复的功能词叠字（的/地/是/了）或孤立的偏旁部首部件（亻氵扌…），其余字符一律拒绝；的的确确/地地道道/是是非非 受构造性保护删不动。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "offset": { "type": "integer", "description": "待删字符的字符位置（疑点证据中给出）" },
                "reason": reason_param.clone(),
            }, "required": ["id", "offset"] },
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
                "reason": reason_param.clone(),
            }, "required": ["idA", "idB"] },
        }},
        { "type": "function", "function": {
            "name": "extractCaption",
            "description": "把被 MinerU 吞进 table_caption 的小节标题抽出为独立标题块（字符纯移动，表格本体不动）。position 按内容归属判断：表格属于该标题之前的小节（标题统领表格之后的内容）→ after；表格是该标题小节的首个内容 → before。level 应与同级编号兄弟标题一致。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "captionIndex": { "type": "integer", "description": "待抽出条目在 table_caption 数组中的下标（疑点证据中给出）" },
                "position": { "type": "string", "enum": ["before", "after"], "description": "抽出块插在表格之前还是之后" },
                "level": { "type": "integer", "description": "标题层级 1-6；省略则抽出为普通正文块" },
                "reason": reason_param.clone(),
            }, "required": ["id", "captionIndex", "position"] },
        }},
        { "type": "function", "function": {
            "name": "dropCaption",
            "description": "删掉 table_caption 数组里的某一条——专用于被 MinerU 吞进 caption 的页眉/页脚家具（如跑马灯页眉、「编制人：X」页脚）。纯削减，表格本体与其余 caption 不动。仅当该条目确为页面家具（疑点证据给出「同文已分类为 header/footer 佐证」或「全文高频重复」）时用；若是真表格题注 → dismiss。",
            "parameters": { "type": "object", "properties": {
                "id": id_param,
                "captionIndex": { "type": "integer", "description": "待删条目在 table_caption 数组中的下标（疑点证据中给出）" },
                "reason": reason_param.clone(),
            }, "required": ["id", "captionIndex"] },
        }},
    ])
});

/// 公开工具集（测试/调试用）。
pub fn tools() -> &'static Value {
    &TOOLS
}

const OP_NAMES: [&str; 11] = [
    "merge",
    "split",
    "demote",
    "promote",
    "reorder",
    "drop",
    "strip",
    "deleteChar",
    "mergeList",
    "extractCaption",
    "dropCaption",
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
4. 修复只许削减/重组（merge/split/demote/promote/reorder/drop/strip/deleteChar/mergeList/extractCaption），系统会机器校验"不新增任何字符、表格行不被篡改"，违规会被自动回滚。
5. 每个疑点最终以【一个】变更 op 或 dismiss 收尾；绝不对同一个块既 dismiss 又调变更 op（矛盾决策会被整体驳回重裁）。变更 op 请在 reason 参数里给一句话依据。"#;

/// 同一响应同时出现 dismiss 与变更 op 时回灌给 LLM 的驳回话术。
const CONTRADICTION_FEEDBACK: &str = "决策矛盾：同一条回复同时调用了 dismiss 和变更 op，均未执行。请重新裁决，只给出【一个】变更 op 或 dismiss。";

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
        "deleteChar" => Ok(OpCall::DeleteChar {
            id: arg_str(args, "id"),
            offset: int_of("offset")?,
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
        "extractCaption" => {
            let position: crate::types::ExtractPosition =
                serde_json::from_value(args.get("position").cloned().unwrap_or(Value::Null))
                    .map_err(|_| {
                        format!(
                            "extractCaption position 必须是 before/after：{}",
                            arg_str(args, "position")
                        )
                    })?;
            Ok(OpCall::ExtractCaption {
                id: arg_str(args, "id"),
                caption_index: int_of("captionIndex")?,
                position,
                level: args.get("level").and_then(Value::as_i64),
            })
        }
        "dropCaption" => Ok(OpCall::DropCaption {
            id: arg_str(args, "id"),
            caption_index: int_of("captionIndex")?,
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

// ── 裁决单元（兄弟组归并）──

/// 裁决单元：单疑点，或一组需一致裁决的同类疑点（一次对话联合裁决）——
/// missed_heading 编号兄弟组 / page_artifact 同文组。
enum Unit {
    Single(WorkItem),
    Group(Vec<WorkItem>),
}

/// 兄弟组单次对话的成员上限：组再大就切块，避免撑爆单对话上下文。
const MAX_GROUP_SIZE: usize = 10;

/// 把需一致裁决的同类疑点归并为联合裁决单元：missed_heading 编号兄弟组 +
/// page_artifact 同文组。
/// 同组判据：同数制、同深度、同父编号，且按文档序末位编号严格递增
/// （编号回落 = 新小节，断组）。组 ≥2 才联合，单个照走单疑点流程。
/// 根治两类实测抖动：组内成员并行裁决互相不可见导致的节内不一致，
/// 以及大组逐个烧 iteration 预算导致末位成员被 max_iterations 饿死。
fn assemble_units(actionable: Vec<WorkItem>, items: &[RefItem]) -> Vec<Unit> {
    // (actionable 下标, 编号路径, 数制)，仅 missed_heading 且行首编号可解析的
    let parsed: Vec<(usize, Vec<u64>, NumStyle)> = actionable
        .iter()
        .enumerate()
        .filter(|(_, w)| w.kind == SuspectKind::MissedHeading)
        .filter_map(|(pos, w)| {
            let i = index_of_id(items, &w.item_id)?;
            let (path, style, _) = parse_numbering(items[i].item.text()?)?;
            Some((pos, path, style))
        })
        .collect();
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for (k, (pos, path, style)) in parsed.iter().enumerate() {
        let extends = k > 0
            && {
                let (_, ppath, pstyle) = &parsed[k - 1];
                pstyle == style
                    && ppath.len() == path.len()
                    && ppath[..ppath.len() - 1] == path[..path.len() - 1]
                    && ppath[ppath.len() - 1] < path[path.len() - 1]
            }
            && runs.last().is_some_and(|r| r.len() < MAX_GROUP_SIZE);
        match runs.last_mut() {
            Some(run) if extends => run.push(*pos),
            _ => runs.push(vec![*pos]),
        }
    }
    // 同文 page_artifact 归并：同一文本的多处疑点合成一组（实测 11 处「问题导向：」
    // 并行裁决互不可见，10 处 dismiss、1 处 drop，违反「同文裁决一致」规则）。
    // 同文组不设上限：拆块会把不一致问题原样带回来，且成员行短、撑不爆上下文。
    let mut by_text: HashMap<&str, Vec<usize>> = HashMap::new();
    for (pos, w) in actionable.iter().enumerate() {
        if w.kind != SuspectKind::PageArtifact {
            continue;
        }
        let Some(i) = index_of_id(items, &w.item_id) else {
            continue;
        };
        let Some(text) = items[i].item.text() else {
            continue;
        };
        by_text.entry(text.trim()).or_default().push(pos);
    }
    runs.extend(by_text.into_values().filter(|g| g.len() >= 2));
    let mut head_of: HashMap<usize, &[usize]> = HashMap::new(); // 组首位置 → 整组位置
    let mut tail_member: HashSet<usize> = HashSet::new(); // 非组首成员位置（跳过）
    for run in runs.iter().filter(|r| r.len() >= 2) {
        head_of.insert(run[0], run);
        tail_member.extend(run[1..].iter().copied());
    }
    actionable
        .iter()
        .enumerate()
        .filter(|(pos, _)| !tail_member.contains(pos))
        .map(|(pos, w)| match head_of.get(&pos) {
            Some(run) => Unit::Group(run.iter().map(|&p| actionable[p].clone()).collect()),
            None => Unit::Single(w.clone()),
        })
        .collect()
}

// ── promote 层级确定性校正 ──

/// 由现存编号标题推举 promote 应得的 level，消除 LLM 对同型编号给出 L2/L3 横跳的抖动：
/// 同数制同深度标题的 level 众数（平局取更小 level，保证确定性）；
/// 无同级锚点 → 父编号标题 level+1；锚点全无 → None（沿用 LLM 给的 level）。
fn expected_heading_level(items: &[RefItem], target_id: &str) -> Option<i64> {
    let i = index_of_id(items, target_id)?;
    let (path, style, _) = parse_numbering(items[i].item.text()?)?;
    let depth = path.len();
    let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
    let mut parent_level: Option<i64> = None;
    for r in items {
        let Some(level) = r.item.text_level() else {
            continue;
        };
        let Some((p, s, _)) = r.item.text().and_then(parse_numbering) else {
            continue;
        };
        if s != style {
            continue;
        }
        if p.len() == depth {
            *counts.entry(level).or_insert(0) += 1;
        } else if depth >= 2 && p.len() == depth - 1 && p[..] == path[..depth - 1] {
            parent_level = Some(level);
        }
    }
    counts
        .into_iter()
        .max_by_key(|&(level, n)| (n, std::cmp::Reverse(level)))
        .map(|(level, _)| level)
        .or(parent_level.map(|l| l + 1))
}

// ── 共用裁决件 ──

/// 工具调用的主目标 id（id 或 idA）。
fn primary_id(args: &Value) -> Option<String> {
    args.get("id")
        .or_else(|| args.get("idA"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 同一响应里既被 dismiss 又被变更 op 指向的 id 集（矛盾决策，相关调用全部驳回）。
/// 兄弟组对话里 dismiss(A)+promote(B) 是合法的逐成员裁决，矛盾必须按 id 判。
fn contradicted_ids(calls: &[ToolCall]) -> HashSet<String> {
    let id_of = |c: &ToolCall| parse_json_safe(&c.function.arguments).and_then(|a| primary_id(&a));
    let dismissed: HashSet<String> = calls
        .iter()
        .filter(|c| c.function.name == "dismiss")
        .filter_map(id_of)
        .collect();
    calls
        .iter()
        .filter(|c| OP_NAMES.contains(&c.function.name.as_str()))
        .filter_map(id_of)
        .filter(|id| dismissed.contains(id))
        .collect()
}

/// 解析并落地一个变更 op（参数校验 → 防震荡 → promote 层级校正 → 保真闸 → 持锁原子替换）。
/// 成功返回 Applied 并写审计日志；任何拒绝返回应回灌给 LLM 的反馈文本。
#[allow(clippy::too_many_arguments)] // 单/组两条裁决路径的共用收口，参数即全部依赖
fn apply_op_call(
    name: &str,
    args: &Value,
    kind_label: &str,
    log_id: &str,
    state: &Arc<Mutex<Vec<RefItem>>>,
    worklist: &[WorkItem],
    ctx: &SuspectCtx,
    violation_count: &mut u64,
) -> Result<SuspectOutcome, String> {
    let mut op_call = to_op_call(name, args).map_err(|e| format!("参数错误: {e}"))?;
    if let Some(banned) = ctx.guard.rejects(&op_call) {
        return Err(format!("被拒（{banned}）。请 dismiss 或换别的 op。"));
    }
    let droppable = droppable_ids(worklist);
    let droppable_captions = droppable_caption_ids(worklist);
    let mut items = state.lock().unwrap();
    if let OpCall::Promote { id, level } = &op_call
        && let Some(exp) = expected_heading_level(&items, id)
        && exp != *level
    {
        (ctx.log)(&format!(
            "promote level 校正 {level}→{exp} [{kind_label}] {id}（同级编号锚点）"
        ));
        op_call = OpCall::Promote {
            id: id.clone(),
            level: exp,
        };
    }
    match apply_op_checked(
        &items,
        &op_call,
        &ApplyContext {
            next_id: &ctx.next_id,
            valid_pages: &ctx.valid_pages,
            droppable_ids: Some(&droppable),
            droppable_caption_ids: Some(&droppable_captions),
        },
    ) {
        ApplyResult::Ok {
            items: new_items,
            removed_spans,
            new_ids,
        } => {
            ctx.guard.record(&op_call, &new_ids);
            *items = new_items; // 持锁原子落地
            drop(items);
            if matches!(op_call, OpCall::Promote { .. } | OpCall::Demote { .. }) {
                ctx.outline_version.fetch_add(1, Ordering::Relaxed);
            }
            // op 落地审计日志（与 dismiss 同格式，reason 来自工具调用的可选参数）
            let reason = args
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("（未给理由）");
            (ctx.log)(&format!("{name} [{kind_label}] {log_id}: {reason}"));
            Ok(SuspectOutcome::Applied {
                op_name: name.to_string(),
                removed_spans,
            })
        }
        ApplyResult::Rejected { reason, kind } => {
            if kind == RejectKind::FidelityViolation {
                *violation_count += 1;
                (ctx.log)(&format!("保真闸回滚 {name}({args}): {reason}"));
            }
            Err(format!(
                "op 被拒绝（{}）: {reason}。请观察后换 op 或 dismiss。",
                if kind == RejectKind::FidelityViolation {
                    "保真闸门回滚"
                } else {
                    "参数非法"
                }
            ))
        }
    }
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
    /// 标题结构版本号：promote/demote 落地即自增。并行对话在 dismiss 前比对它，
    /// 发现结构已被别的对话改过就驳回一次、回灌最新 outline 重裁（时序竞争守卫）。
    outline_version: AtomicU64,
}

enum SuspectOutcome {
    Applied {
        op_name: String,
        removed_spans: Vec<RemovedSpan>,
    },
    Dismissed {
        reason: &'static str,
        /// 自由文本依据（LLM 的一句话理由 / 视觉裁决依据）；无则空串。
        detail: String,
        violations: u64,
    },
}

pub async fn run_loop(
    initial: Vec<RefItem>,
    next_id: IdGen,
    opts: LoopOptions,
) -> Result<LoopResult, LlmError> {
    let max_iterations = opts.max_iterations;
    let progress = opts.progress.clone();
    let input_suspects = opts.input_suspects;
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
    let mut dismissed_suspects: Vec<DismissedSuspect> = Vec::new(); // 上集的逐条明细（顺序即裁决序）
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
        outline_version: AtomicU64::new(0),
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
        // 进度：每轮迭代开始时吐出当前剩余可处理疑点数（actionable 为空时也吐一帧
        // worklist_remaining=0，作为「清洗到底」的终点信号）。
        if let Some(sink) = &progress {
            sink(Progress {
                iterations,
                max_iterations,
                worklist_remaining: actionable.len(),
                input_suspects,
            });
        }
        if actionable.is_empty() {
            break;
        }

        // 组归并：编号兄弟组 / 同文 artifact 组合成联合裁决单元，只占一个 iteration 槽位
        let units = {
            let items = state.lock().unwrap();
            assemble_units(actionable, &items)
        };
        // 一批最多 concurrency 个单元并行裁决（不同位置的块相互独立，这是主要提速来源）
        let batch_size = concurrency.min((max_iterations - iterations) as usize);
        let batch: Vec<Unit> = units.into_iter().take(batch_size).collect();
        iterations += batch.len() as u64;

        let worklist = Arc::new(worklist);
        let futures = batch
            .into_iter()
            .map(|unit| {
                let state = state.clone();
                let worklist = worklist.clone();
                let ctx = ctx.clone();
                async move {
                    match unit {
                        Unit::Single(target) => {
                            let outcome = handle_suspect(&target, &state, &worklist, &ctx).await;
                            vec![(target, outcome)]
                        }
                        Unit::Group(members) => {
                            match handle_group(&members, &state, &worklist, &ctx).await {
                                Ok(v) => v.into_iter().map(|(w, o)| (w, Ok(o))).collect(),
                                // 整组 LLM 故障：全员搁置（与单疑点故障处理一致）
                                Err(e) => members
                                    .into_iter()
                                    .map(|w| (w, Err(LlmError(e.0.clone()))))
                                    .collect(),
                            }
                        }
                    }
                }
            })
            .collect::<Vec<_>>();
        let results = futures::future::join_all(futures).await;

        let mut llm_errors: Vec<LlmError> = Vec::new();
        for (target, outcome) in results.into_iter().flatten() {
            match outcome {
                Err(e) => {
                    // 单疑点 LLM 故障（重试耗尽）：搁置该疑点，不毁全局（其它并行对话照常收尾）
                    log(&format!(
                        "疑点 {} LLM 调用失败，搁置: {e}",
                        suspect_key(&target)
                    ));
                    if dismissed_keys.insert(suspect_key(&target)) {
                        dismissed_suspects.push(DismissedSuspect {
                            kind: target.kind.as_str().into(),
                            item_id: target.item_id.clone(),
                            reason: "llm_error".into(),
                            detail: e.0.clone(),
                            evidence: target.evidence.clone(),
                        });
                    }
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
                            detail,
                            violations: v,
                        } => {
                            // dismiss（LLM 主动 / 轮数耗尽 / op 被闸门回滚后放弃）→ 计入裁决集，重探测不再标记
                            if dismissed_keys.insert(suspect_key(&target)) {
                                dismissed_suspects.push(DismissedSuspect {
                                    kind: target.kind.as_str().into(),
                                    item_id: target.item_id.clone(),
                                    reason: reason.into(),
                                    detail,
                                    evidence: target.evidence.clone(),
                                });
                            }
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
        dismissed_suspects,
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
            detail: v.reason.clone(),
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
            droppable_caption_ids: None,
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
        SuspectKind::ExtraChar => {
            "读上下文判断该字是否 OCR 衍字：删掉后语句更通顺 → deleteChar（offset 用证据中的值）；属正常语法（「目的+的」「但是+是」「不甚了了」）或正文在讨论偏旁本身 → dismiss"
        }
        SuspectKind::CaptionHeading => {
            "用 getItems/outline 判断该 caption 条目是否被吞的小节标题：是 → extractCaption（captionIndex/level 用证据中的值，position 按内容归属判断——表格属于该标题之前的小节 → after，表格是该小节首个内容 → before）；是真表格题注 → dismiss"
        }
        SuspectKind::CaptionArtifact => {
            "确认该 caption 条目是被吞进表格题注的页眉/页脚家具（证据给出「同文已分类为 header/footer 佐证」或「全文高频重复」）→ dropCaption（captionIndex 用证据中的值）；确认是真表格题注 → dismiss"
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
            detail: String::new(),
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
    // 时序竞争守卫基线：对话开始时的标题结构版本
    let outline_v0 = ctx.outline_version.load(Ordering::Relaxed);
    let mut outline_challenged = false;

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
                detail: String::new(),
                violations: violation_count,
            });
        };
        messages.push(Message::Assistant {
            content: r.message.content.clone(),
            tool_calls: Some(calls.clone()),
        });

        // 决策矛盾守卫：同一响应同时调用 dismiss 与变更 op（实测 LLM 会把「应 drop」的
        // 完整分析写进 dismiss.reason 却落 dismiss）。顺序采纳任一方都可能错——
        // 全部驳回，回灌错误显式重裁，而非静默采纳先到的那个。
        let contradictory = calls.iter().any(|c| c.function.name == "dismiss")
            && calls
                .iter()
                .any(|c| OP_NAMES.contains(&c.function.name.as_str()));
        if contradictory {
            (ctx.log)(&format!(
                "决策矛盾 [{}] {}: 同响应同时调用 dismiss 与变更 op，全部驳回重裁",
                target.kind.as_str(),
                target.item_id
            ));
        }

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
                if contradictory {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: CONTRADICTION_FEEDBACK.into(),
                    });
                    continue;
                }
                // 时序竞争守卫：本对话期间并行裁决改过标题结构（实测两例 dismiss 理由
                // 引用了 promote 落地前的过期 outline）→ 驳回一次，回灌最新骨架重裁。
                if !outline_challenged
                    && matches!(
                        target.kind,
                        SuspectKind::MissedHeading | SuspectKind::PseudoHeading
                    )
                    && ctx.outline_version.load(Ordering::Relaxed) != outline_v0
                {
                    outline_challenged = true;
                    let outline = {
                        let items = state.lock().unwrap();
                        exec_observe("outline", &serde_json::json!({}), &items, worklist)
                            .unwrap_or_else(|e| format!("（outline 获取失败: {e}）"))
                    };
                    (ctx.log)(&format!(
                        "dismiss 暂缓 [{}] {}: 标题结构已被并行裁决修改，回灌最新 outline 重裁",
                        target.kind.as_str(),
                        target.item_id
                    ));
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: format!(
                            "暂缓采纳：本对话进行期间，并行裁决已修改文档标题结构，你的判断可能基于过期信息。最新标题骨架：\n{outline}\n请基于最新结构重新裁决（仍判误报就再次 dismiss，将被直接采纳）。"
                        ),
                    });
                    continue;
                }
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
                    detail: reason.to_string(),
                    violations: violation_count,
                });
            }

            if OP_NAMES.contains(&name) {
                if contradictory {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: CONTRADICTION_FEEDBACK.into(),
                    });
                    continue;
                }
                match apply_op_call(
                    name,
                    &args,
                    target.kind.as_str(),
                    &target.item_id,
                    state,
                    worklist,
                    ctx,
                    &mut violation_count,
                ) {
                    Ok(outcome) => return Ok(outcome),
                    Err(feedback) => {
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: feedback,
                        });
                        continue;
                    }
                }
            }

            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: format!("未知工具 {name}。"),
            });
        }
    }

    Ok(SuspectOutcome::Dismissed {
        reason: "max_rounds_exhausted",
        detail: String::new(),
        violations: violation_count,
    })
}

/// 组联合裁决：一次对话裁决整组同类成员，要求组内一致。两种组形态：
/// - missed_heading 编号兄弟组（结构平行的同级编号要么都是标题，要么都是正文）；
/// - page_artifact 同文组（同一文本的多处疑点要删都删，不删都留）。
///
/// 逐成员以变更 op/dismiss 收尾，全部裁完或轮数耗尽（剩余成员搁置）才结束。
async fn handle_group(
    members: &[WorkItem],
    state: &Arc<Mutex<Vec<RefItem>>>,
    worklist: &[WorkItem],
    ctx: &SuspectCtx,
) -> Result<Vec<(WorkItem, SuspectOutcome)>, LlmError> {
    let kind = members[0].kind;
    let kind_label = kind.as_str();
    let group_label = match kind {
        SuspectKind::PageArtifact => "同文组联合裁决",
        _ => "兄弟组联合裁决",
    };
    let ids: Vec<&str> = members.iter().map(|w| w.item_id.as_str()).collect();
    (ctx.log)(&format!(
        "{group_label} [{kind_label}] {} 个成员: {}",
        members.len(),
        ids.join(", ")
    ));

    let (member_block, preload) = {
        let items = state.lock().unwrap();
        let block = members
            .iter()
            .enumerate()
            .map(|(n, w)| {
                // 同文组的成员文本全同，逐成员给 ±1 邻居上下文（裁决「正文引导语还是
                // 页面家具」靠的是各处的落点环境）；兄弟组给成员自身全文即可。
                let current = if kind == SuspectKind::PageArtifact {
                    exec_observe(
                        "getItems",
                        &serde_json::json!({ "id": w.item_id, "before": 1, "after": 1 }),
                        &items,
                        worklist,
                    )
                    .unwrap_or_else(|_| "（已不存在）".into())
                } else {
                    index_of_id(&items, &w.item_id)
                        .map(|i| fmt_item(&items[i], 600))
                        .unwrap_or_else(|| "（已不存在）".into())
                };
                format!(
                    "{}. {} 证据：{}\n   {}",
                    n + 1,
                    w.item_id,
                    w.evidence,
                    current.replace('\n', "\n   ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let preload = match kind {
            SuspectKind::PageArtifact => String::new(), // 同文组用不上标题骨架
            _ => format!(
                "\n\n全文标题骨架（outline 预载）：\n{}",
                exec_observe("outline", &serde_json::json!({}), &items, worklist)
                    .unwrap_or_else(|e| format!("（outline 获取失败: {e}）"))
            ),
        };
        (block, preload)
    };

    let instruction = match kind {
        SuspectKind::PageArtifact => format!(
            "当前疑点组：[page_artifact] 同文组联合裁决（同一文本的 {} 处疑点）。\n\
             同一文本的多处疑点裁决必须一致：确认是页眉/页脚/页码/水印 → 逐个 drop；是正文（如各小节反复出现的引导语、模板句式）→ 逐个 dismiss。仅当有明确证据某成员确属例外（如首页真标题 vs 其余页漏标页眉）才允许分歧。\n\
             请对每个成员各调用一次 drop 或 dismiss（可在同一条回复里并行多个调用），绝不对同一成员同时调用两者。",
            members.len()
        ),
        _ => format!(
            "当前疑点组：[missed_heading] 编号兄弟组联合裁决（{} 个结构平行的同级编号块）。\n\
             组内裁决必须一致：要么全部 promote 为标题（level 一致，与 outline 中同深度编号标题对齐），要么全部 dismiss 为正文；除非有明确证据某成员确属例外。\n\
             请对每个成员各调用一次 promote 或 dismiss（可在同一条回复里并行多个调用），绝不对同一成员同时调用两者。",
            members.len()
        ),
    };
    let mut messages: Vec<Message> = vec![
        Message::System {
            content: SYSTEM_PROMPT.to_string(),
        },
        Message::User {
            content: format!(
                "{instruction}\n\n成员列表：\n{member_block}{preload}\n\n若以上信息已足够请直接逐成员裁决；不够再调观察工具。"
            ),
        },
    ];

    let mut pending: Vec<WorkItem> = members.to_vec();
    let mut resolved: Vec<(WorkItem, SuspectOutcome)> = Vec::new();
    let mut violation_count: u64 = 0;

    for round in 0..ctx.max_rounds {
        if round + 2 == ctx.max_rounds {
            messages.push(Message::User {
                content: format!(
                    "观察轮数即将用完。请基于已有信息【现在就】对剩余成员逐一裁决（变更 op 或 dismiss）：{}。不要再调用观察工具。",
                    pending
                        .iter()
                        .map(|w| w.item_id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        let r = ctx.chat.chat(&messages, tools()).await?;
        ctx.tokens
            .prompt
            .fetch_add(r.usage.prompt_tokens, Ordering::Relaxed);
        ctx.tokens
            .completion
            .fetch_add(r.usage.completion_tokens, Ordering::Relaxed);

        let Some(calls) = r.message.tool_calls.filter(|c| !c.is_empty()) else {
            break; // 不再发工具调用 → 剩余成员搁置
        };
        messages.push(Message::Assistant {
            content: r.message.content.clone(),
            tool_calls: Some(calls.clone()),
        });

        let contradicted = contradicted_ids(&calls);
        if !contradicted.is_empty() {
            (ctx.log)(&format!(
                "决策矛盾 [{kind_label}] {}: 同响应对同一成员同时调用 dismiss 与变更 op，相关调用全部驳回重裁",
                contradicted.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }

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
                    let items = state.lock().unwrap();
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
                let id = arg_str(&args, "id");
                if contradicted.contains(&id) {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: CONTRADICTION_FEEDBACK.into(),
                    });
                    continue;
                }
                let Some(k) = pending.iter().position(|w| w.item_id == id) else {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: format!("{id} 不在本组待裁决成员中（或已裁决）。"),
                    });
                    continue;
                };
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("（未给理由）");
                (ctx.log)(&format!("dismiss [{kind_label}] {id}: {reason}"));
                resolved.push((
                    pending.remove(k),
                    SuspectOutcome::Dismissed {
                        reason: "llm_dismiss",
                        detail: reason.to_string(),
                        violations: std::mem::take(&mut violation_count),
                    },
                ));
                messages.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: format!("已采纳 dismiss（{id}）。"),
                });
                continue;
            }

            if OP_NAMES.contains(&name) {
                let pid = primary_id(&args);
                if pid.as_deref().is_some_and(|id| contradicted.contains(id)) {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: CONTRADICTION_FEEDBACK.into(),
                    });
                    continue;
                }
                // 组对话只许动组内成员：避免落了 op 却无成员可归账（op_counts 漏记）
                let Some(k) = pid
                    .as_deref()
                    .and_then(|id| pending.iter().position(|w| w.item_id == id))
                else {
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: "本组对话只允许对组内待裁决成员执行变更 op。".into(),
                    });
                    continue;
                };
                let log_id = pending[k].item_id.clone();
                match apply_op_call(
                    name,
                    &args,
                    kind_label,
                    &log_id,
                    state,
                    worklist,
                    ctx,
                    &mut violation_count,
                ) {
                    Ok(outcome) => {
                        resolved.push((pending.remove(k), outcome));
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: format!("已执行（{log_id}）。"),
                        });
                    }
                    Err(feedback) => {
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: feedback,
                        });
                    }
                }
                continue;
            }

            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: format!("未知工具 {name}。"),
            });
        }

        if pending.is_empty() {
            return Ok(resolved);
        }
    }

    for w in pending {
        resolved.push((
            w,
            SuspectOutcome::Dismissed {
                reason: "max_rounds_exhausted",
                detail: String::new(),
                violations: std::mem::take(&mut violation_count),
            },
        ));
    }
    Ok(resolved)
}
