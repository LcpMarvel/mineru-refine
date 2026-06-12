// OCR 字符混淆修正层（opt-in，fix_ocr_confusion=true 才运行）：
// 核心 refine 出口闸门【之后】的独立后处理。核心层承诺（C_out ⊆ C_in）不变，
// 本层叠加第二份契约：所有修改都是稀疏的一换一定点替换，每条要么在混淆表内，
// 要么通过独立二次裁决；全量进 report/provenance，可审计可撤销。
//
// 权力结构：LLM 只有提案权，没有写入权——
//   闸门 1（结构）：恰好 1 字符、与原字符不同、密度上限内（OCR 混淆是稀疏的）；
//   闸门 2（准入）：(before, after) 同属混淆等价类 → 直接落地；
//                   表外提案 → 对抗式二次裁决，通过才落地（source=second_opinion）。
// 层内任何 LLM 故障 → 搁置对应批次（漏修，不毁全局）；层级 panic 由调用方兜（丢弃整层）。
//
// table_body（Phase 2）：标签骨架与文本节点词法分离，只有 td/th 单元格内的文本
// 可成为候选——标记字符（colspan=1 的 1）在构造上就不可能被替换，HTML 实体当黑盒跳过。
// 表格候选用行列结构化上下文裁决（标题/表头/所在行），并多一道每表聚合密度闸门
//（乱码表会诱发大量"修复"，整表按住——乱码表的归宿是整表裁决，不是逐字替换）。

use crate::agent_loop::Logger;
use crate::llm::{ChatClient, ChatResult, LlmError, Message, Usage, parse_json_safe};
use crate::types::{ConfusionFix, RefItem, TokenUsage};
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::LazyLock;

/// 混淆层独立的 prompt 版本（与核心层 PROMPT_VERSION 分开演进，进缓存 key）。
/// c2：table_body 进入处理范围 + 表格行列上下文 + 白名单实证扩充（扞杆/亭亨）。
pub const CONFUSION_PROMPT_VERSION: &str = "c2";

/// 内置混淆等价类：每个字符串是一组互为 OCR 形近的字符，类内任意方向可换。
/// rn↔m 这类多字符混淆超出一换一定点约束的能力范围，不收。
pub const BUILTIN_CONFUSION_CLASSES: &[&str] = &[
    // 拉丁/数字形近
    "0O",
    "1lI|",
    "5S",
    "8B",
    "2Z",
    "6G",
    "9gq",
    // 中文/符号形近
    "入人λ",
    "竟竞",
    "末未",
    "己已巳",
    "土士",
    "日曰",
    "戊戌戍",
    // 实证扩充（Phase 1 真实文档 observations）：业界标扞→标杆、亭德森→亨德森
    "扞杆",
    "亭亨",
];

/// 单次裁决调用最多带多少个候选（再多就分批，避免撑爆单次输出）。
const MAX_CANDIDATES_PER_CALL: usize = 40;
/// 候选两侧各取多少字符做上下文窗口。
const CONTEXT_WINDOW: usize = 40;
/// observations 上限（LLM 顺带报告的表外问题，只记不改）。
const MAX_OBSERVATIONS: usize = 50;

/// 单个字段单元内允许落地的替换密度上限：OCR 混淆是稀疏的，
/// 超标说明 LLM 在做别的事（改写/批量替换），整单元拒绝。
fn density_cap(non_ws_len: usize) -> usize {
    (non_ws_len * 3 / 100).max(2)
}

/// 每表聚合密度上限：单格各自合规但整表提案过多 = 乱码表特征，整表拒绝。
/// 阈值按 047 乱码表实测校准前先取 max(4, 2%)。
fn table_density_cap(non_ws_len: usize) -> usize {
    (non_ws_len * 2 / 100).max(4)
}

/// 单元内非空白字符数（两道密度闸门共用的分母）。
fn non_ws_len(chars: &[char]) -> usize {
    chars
        .iter()
        .filter(|c| !crate::invariant::is_js_whitespace(**c))
        .count()
}

// ── 混淆表 ──

/// 准入名单：内置等价类 + 用户补充对。只决定"裁决结果可否直接落地"，
/// 改不改本身 100% 由 LLM 判——表宽不增加误伤。
pub struct ConfusionTable {
    class_of: HashMap<char, usize>,
    extra: HashSet<(char, char)>,
    /// 候选字符全集（class_of 的键 ∪ extra 两侧字符）：扫描热路径上每字符查一次。
    candidates: HashSet<char>,
}

impl ConfusionTable {
    /// extra_pairs 每项必须恰好 2 个不同字符（如 "0D"，表示 0↔D 互换可直接落地）。
    /// 非法配置早抛——这是调用方的配置错误，不静默吞。
    pub fn build(extra_pairs: &[String]) -> Result<Self, String> {
        let mut class_of = HashMap::new();
        for (idx, class) in BUILTIN_CONFUSION_CLASSES.iter().enumerate() {
            for c in class.chars() {
                class_of.insert(c, idx);
            }
        }
        let mut extra = HashSet::new();
        for pair in extra_pairs {
            let chars: Vec<char> = pair.chars().collect();
            if chars.len() != 2 || chars[0] == chars[1] {
                return Err(format!(
                    "混淆对「{pair}」非法：必须恰好 2 个不同字符（如 \"0D\"）"
                ));
            }
            extra.insert((chars[0], chars[1]));
            extra.insert((chars[1], chars[0]));
        }
        let mut candidates: HashSet<char> = class_of.keys().copied().collect();
        candidates.extend(extra.iter().map(|(a, _)| *a));
        Ok(Self {
            class_of,
            extra,
            candidates,
        })
    }

    /// 该字符是否值得送审（候选漏斗：只决定问不问，不决定改不改）。
    fn is_candidate(&self, c: char) -> bool {
        self.candidates.contains(&c)
    }

    /// (before, after) 是否在准入名单内（可免二次裁决直接落地）。
    fn allowed(&self, before: char, after: char) -> bool {
        if self.extra.contains(&(before, after)) {
            return true;
        }
        matches!((self.class_of.get(&before), self.class_of.get(&after)),
                 (Some(a), Some(b)) if a == b)
    }
}

// ── 字段单元 ──

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Text,
    ListItem(usize),
    TableCaption(usize),
    /// table_body 内的一个单元格文本节点（标签骨架不可见、不可改）
    TableBody,
}

impl Field {
    fn name(&self) -> String {
        match self {
            Field::Text => "text".into(),
            Field::ListItem(i) => format!("list_items[{i}]"),
            Field::TableCaption(i) => format!("table_caption[{i}]"),
            Field::TableBody => "table_body".into(),
        }
    }
}

/// 表格文本节点的定位信息：base 是节点首字符在整个 table_body 字符串中的
/// 字符偏移（report/provenance 的 charOffset 用 base+局部偏移，下游拿原串可直接定位）；
/// row/col/prefix_in_cell 用于构造行列上下文（一个单元格可能含多个文本节点，如被 <br> 分隔）。
#[derive(Clone, Copy)]
struct TablePos {
    base: usize,
    row: usize,
    col: usize,
    prefix_in_cell: usize,
}

/// 一个可替换的字符串字段快照。table_body 以单元格文本节点为单元进入，
/// 标签骨架在扫描阶段就不可见，"标记被替换"在构造上不可能。
struct Unit {
    item_idx: usize,
    field: Field,
    chars: Vec<char>,
    /// 仅 Field::TableBody 有值
    table: Option<TablePos>,
}

/// 表格的行列文本（构造裁决上下文用）。
struct TableCtx {
    caption: String,
    rows: Vec<Vec<String>>,
}

/// table_body 里解析出的单元格文本节点。
struct TableNode {
    pos: TablePos,
    chars: Vec<char>,
}

/// 对 MinerU 生成的表格 HTML 做词法切分：标签骨架与文本节点分离。
/// 只认 td/th 单元格内的文本；行列号随 tr/td 推进。畸形 HTML（乱码表常见）
/// 不 panic：单元格外的散落文本直接忽略（不扫描 = 不替换 = 原样保留）。
fn parse_table_nodes(s: &str) -> (Vec<TableNode>, Vec<Vec<String>>) {
    let chars: Vec<char> = s.chars().collect();
    let mut nodes: Vec<TableNode> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut in_cell = false;
    let mut run: Vec<char> = Vec::new();
    let mut run_start = 0usize;

    let mut flush_run = |run: &mut Vec<char>, run_start: usize, rows: &mut Vec<Vec<String>>| {
        if run.is_empty() {
            return;
        }
        let row = rows.len() - 1;
        let col = rows[row].len() - 1;
        let cell = &mut rows[row][col];
        let prefix_in_cell = cell.chars().count();
        cell.push_str(&run.iter().collect::<String>());
        nodes.push(TableNode {
            pos: TablePos {
                base: run_start,
                row,
                col,
                prefix_in_cell,
            },
            chars: std::mem::take(run),
        });
    };

    for (i, &c) in chars.iter().enumerate() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                // 标签名：开头的 '/'+字母段（"td rowspan=1" → "td"，"/td" → "/td"）
                let name: String = tag
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '/')
                    .collect::<String>()
                    .to_ascii_lowercase();
                match name.as_str() {
                    "tr" => {
                        rows.push(Vec::new());
                        in_cell = false;
                    }
                    "td" | "th" => {
                        if rows.is_empty() {
                            rows.push(Vec::new()); // 缺 <tr> 的畸形表
                        }
                        rows.last_mut().unwrap().push(String::new());
                        in_cell = true;
                    }
                    "/td" | "/th" => in_cell = false,
                    _ => {} // table/tbody/br/… 不影响行列结构（br 只是切断文本节点）
                }
            } else {
                tag.push(c);
            }
            continue;
        }
        if c == '<' {
            if in_cell {
                flush_run(&mut run, run_start, &mut rows);
            }
            run.clear();
            in_tag = true;
            tag.clear();
            continue;
        }
        if in_cell {
            if run.is_empty() {
                run_start = i;
            }
            run.push(c);
        }
    }
    if in_cell {
        flush_run(&mut run, run_start, &mut rows); // 没闭合的畸形表也收尾
    }
    (nodes, rows)
}

/// 文本节点内 HTML 实体（&amp; / &#80; / &#x1F;）的字符区间（含 & 与 ;）。
/// 实体内字符不可替换：动一个字符就是坏实体。
fn entity_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            let mut j = i + 1;
            let mut closed = false;
            while j < chars.len() && j - i <= 12 {
                let c = chars[j];
                if c == ';' {
                    closed = j > i + 1;
                    break;
                }
                if !(c.is_ascii_alphanumeric() || c == '#') {
                    break;
                }
                j += 1;
            }
            if closed {
                spans.push((i, j + 1));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    spans
}

fn in_spans(i: usize, spans: &[(usize, usize)]) -> bool {
    spans.iter().any(|(lo, hi)| i >= *lo && i < *hi)
}

/// 候选位置：unit 内的字符偏移。
struct Cand {
    unit: usize,
    offset: usize,
}

/// LLM 提案（已过结构闸门的原始形态）。
struct Proposal {
    cand: usize,
    after: char,
    reason: String,
}

// ── 工具定义 ──

static JUDGE_TOOLS: LazyLock<Value> = LazyLock::new(|| {
    json!([{ "type": "function", "function": {
        "name": "judgeConfusions",
        "description": "提交全部候选位置的裁决。",
        "parameters": { "type": "object", "properties": {
            "verdicts": { "type": "array", "items": { "type": "object", "properties": {
                "index": { "type": "integer", "description": "候选编号" },
                "action": { "type": "string", "enum": ["keep", "replace"] },
                "replaceWith": { "type": "string", "description": "action=replace 时必填，恰好 1 个字符" },
                "reason": { "type": "string", "description": "一句话依据" },
            }, "required": ["index", "action"] } },
            "observations": { "type": "array", "items": { "type": "string" },
                "description": "候选之外观察到的其他 OCR 质量问题（只记录，不会被应用）" },
        }, "required": ["verdicts"] },
    }}])
});

static VERIFY_TOOLS: LazyLock<Value> = LazyLock::new(|| {
    json!([{ "type": "function", "function": {
        "name": "verifyConfusion",
        "description": "审查一条表外替换提案。",
        "parameters": { "type": "object", "properties": {
            "verdict": { "type": "string", "enum": ["approve", "reject"] },
            "reason": { "type": "string", "description": "一句话依据" },
        }, "required": ["verdict", "reason"] },
    }}])
});

const JUDGE_SYSTEM_PROMPT: &str = r#"你是 OCR 字符混淆修正器。文档由 PDF OCR 而来，形近字符常被认错（如 CEO→CE0、OA→0A、λ→入、竞争→竟争、81.36%→B1.36%）。
你会收到若干候选位置，每个候选的上下文里用 «X» 标出待判字符。逐一判断：该字符在此处是否是 OCR 形近误认？
- 是 → action=replace，replaceWith 给出唯一正确字符（必须恰好 1 个字符）。
- 否（本来就对）或拿不准 → action=keep。拿不准必须 keep——宁可漏修，不可错改。产品型号、代码、编号里的字符尤其要保守。
标注〔表格单元格〕的候选来自表格，附有表标题/表头/所在行——结合行列语义判断；表格里的序号列、编号列、代码列几乎都是合法数字/字母，必须 keep。
只做字符级一换一修正，绝不润色、增删、改写。
若在候选之外观察到其他 OCR 质量问题（乱码、明显错字），写进 observations（仅记录，系统不会应用）。
必须调用 judgeConfusions 工具一次性提交全部候选的裁决。"#;

// ── 结果 ──

#[derive(Default)]
pub struct ConfusionOutcome {
    pub fixes: Vec<ConfusionFix>,
    /// 被任一闸门拒绝的提案数（结构非法 / 密度超标 / 二次裁决否决）。
    pub rejected: u64,
    pub observations: Vec<String>,
    pub usage: TokenUsage,
}

// ── 主流程 ──

/// 对 items 的 text/list_items/table_caption 做混淆修正。
/// 取得 items 所有权：调用方在 panic 时（catch_unwind）保留原件，天然整层丢弃。
pub async fn fix_confusions(
    mut items: Vec<RefItem>,
    chat: Arc<dyn ChatClient>,
    concurrency: usize,
    table: &ConfusionTable,
    log: &Logger,
) -> (Vec<RefItem>, ConfusionOutcome) {
    let mut outcome = ConfusionOutcome::default();

    // 1. 字段单元快照 + 候选扫描（文档序，输出确定性的根）
    let mut units: Vec<Unit> = Vec::new();
    let mut cands: Vec<Cand> = Vec::new();
    let mut table_ctx: HashMap<usize, TableCtx> = HashMap::new();
    let add_unit =
        |units: &mut Vec<Unit>, cands: &mut Vec<Cand>, unit: Unit, offsets: Vec<usize>| {
            if offsets.is_empty() {
                return;
            }
            let unit_idx = units.len();
            cands.extend(offsets.into_iter().map(|offset| Cand {
                unit: unit_idx,
                offset,
            }));
            units.push(unit);
        };
    for (item_idx, r) in items.iter().enumerate() {
        let push_plain = |units: &mut Vec<Unit>, cands: &mut Vec<Cand>, field: Field, s: &str| {
            let chars: Vec<char> = s.chars().collect();
            let offsets: Vec<usize> = chars
                .iter()
                .enumerate()
                .filter(|(_, c)| table.is_candidate(**c))
                .map(|(i, _)| i)
                .collect();
            add_unit(
                units,
                cands,
                Unit {
                    item_idx,
                    field,
                    chars,
                    table: None,
                },
                offsets,
            );
        };
        if let Some(t) = r.item.text() {
            push_plain(&mut units, &mut cands, Field::Text, t);
        }
        for (key, make) in [
            ("list_items", Field::ListItem as fn(usize) -> Field),
            ("table_caption", Field::TableCaption as fn(usize) -> Field),
        ] {
            if let Some(parts) = r.item.str_array(key) {
                for (i, p) in parts.iter().enumerate() {
                    push_plain(&mut units, &mut cands, make(i), p);
                }
            }
        }
        if let Some(tb) = r.item.table_body() {
            let (nodes, rows) = parse_table_nodes(tb);
            let mut any = false;
            for node in nodes {
                let entities = entity_spans(&node.chars);
                let offsets: Vec<usize> = node
                    .chars
                    .iter()
                    .enumerate()
                    .filter(|(i, c)| table.is_candidate(**c) && !in_spans(*i, &entities))
                    .map(|(i, _)| i)
                    .collect();
                if offsets.is_empty() {
                    continue;
                }
                any = true;
                add_unit(
                    &mut units,
                    &mut cands,
                    Unit {
                        item_idx,
                        field: Field::TableBody,
                        chars: node.chars,
                        table: Some(node.pos),
                    },
                    offsets,
                );
            }
            if any {
                let caption = r
                    .item
                    .str_array("table_caption")
                    .map(|c| c.join("；"))
                    .unwrap_or_default();
                table_ctx.insert(item_idx, TableCtx { caption, rows });
            }
        }
    }
    if cands.is_empty() {
        return (items, outcome);
    }

    // 2. 打包成裁决调用（按文档序，每批 ≤ MAX_CANDIDATES_PER_CALL，批即 cands 的连续区间）
    let batches: Vec<Range<usize>> = (0..cands.len())
        .step_by(MAX_CANDIDATES_PER_CALL)
        .map(|s| s..(s + MAX_CANDIDATES_PER_CALL).min(cands.len()))
        .collect();
    log(&format!(
        "混淆层：{} 个候选字符，分 {} 次裁决",
        cands.len(),
        batches.len()
    ));

    // 3. 批量裁决（buffered 让 concurrency 个调用始终在飞；单调用失败 → 该批搁置）
    let concurrency = concurrency.max(1);
    let mut proposals: Vec<Proposal> = Vec::new();
    let mut shelved_batches = 0u64;
    // 先把 future 收集成 Vec（绕开 rustc 对惰性迭代器 + async 块的高阶生命周期误判）
    let judge_futs: Vec<_> = batches
        .iter()
        .map(|batch| judge_batch(batch.clone(), &cands, &units, &table_ctx, chat.clone()))
        .collect();
    let judge_results: Vec<_> = futures::stream::iter(judge_futs)
        .buffered(concurrency)
        .collect()
        .await;
    for (batch, result) in batches.iter().zip(judge_results) {
        match result {
            Ok(reply) => {
                outcome.usage.prompt += reply.usage.prompt_tokens;
                outcome.usage.completion += reply.usage.completion_tokens;
                outcome.observations.extend(reply.observations);
                if reply.invalid > 0 {
                    outcome.rejected += reply.invalid;
                    log(&format!(
                        "混淆层：{} 条结构非法的 replace 提案被解析期拒绝",
                        reply.invalid
                    ));
                }
                for (local, after, reason) in reply.replacements {
                    let cand_idx = batch.start + local;
                    if cand_idx >= batch.end {
                        outcome.rejected += 1;
                        log(&format!("混淆层：裁决引用了不存在的候选编号 {local}，拒绝"));
                        continue;
                    }
                    proposals.push(Proposal {
                        cand: cand_idx,
                        after,
                        reason,
                    });
                }
            }
            Err(e) => {
                shelved_batches += 1;
                log(&format!(
                    "混淆层：裁决调用失败，搁置该批 {} 个候选: {e}",
                    batch.len()
                ));
            }
        }
    }
    if shelved_batches > 0 {
        log(&format!(
            "混淆层：共搁置 {shelved_batches} 批候选（LLM 故障，漏修不误修）"
        ));
    }

    // 4. 结构闸门：去重（同一候选只认第一条；cand 索引与 (unit, offset) 一一对应）+ 与原字符不同
    let mut seen_cands: HashSet<usize> = HashSet::new();
    proposals.retain(|p| {
        let cand = &cands[p.cand];
        let before = units[cand.unit].chars[cand.offset];
        if p.after == before {
            return false; // replace 成同字符 = 无操作，静默丢弃不计 rejected
        }
        if !seen_cands.insert(p.cand) {
            outcome.rejected += 1;
            log(&format!(
                "混淆层：{} 字符 {} 处重复提案，拒绝",
                units[cand.unit].field.name(),
                cand.offset
            ));
            return false;
        }
        true
    });

    // 5. 密度闸门：单元内提案数超过稀疏上限 → 整单元拒绝（先于二次裁决，省调用）
    let mut per_unit: HashMap<usize, usize> = HashMap::new();
    for p in &proposals {
        *per_unit.entry(cands[p.cand].unit).or_insert(0) += 1;
    }
    let over_density: HashSet<usize> = per_unit
        .iter()
        .filter(|(unit, n)| **n > density_cap(non_ws_len(&units[**unit].chars)))
        .map(|(unit, _)| *unit)
        .collect();
    for unit in &over_density {
        log(&format!(
            "混淆层：{} 提案 {} 条超出稀疏上限，整单元拒绝（OCR 混淆应是稀疏的）",
            units[*unit].field.name(),
            per_unit[unit]
        ));
    }
    proposals.retain(|p| {
        if over_density.contains(&cands[p.cand].unit) {
            outcome.rejected += 1;
            return false;
        }
        true
    });

    // 5b. 每表聚合密度闸门：单格各自合规但整表提案过多 = 乱码表特征，整表拒绝
    //（乱码表的归宿是整表裁决/降级，不是逐字"修复"）
    let mut per_table_n: HashMap<usize, usize> = HashMap::new();
    for p in &proposals {
        let u = &units[cands[p.cand].unit];
        if u.table.is_some() {
            *per_table_n.entry(u.item_idx).or_insert(0) += 1;
        }
    }
    let mut table_nonws: HashMap<usize, usize> = HashMap::new();
    for u in &units {
        if u.table.is_some() {
            *table_nonws.entry(u.item_idx).or_insert(0) += non_ws_len(&u.chars);
        }
    }
    let over_table: HashSet<usize> = per_table_n
        .iter()
        .filter(|(item, n)| **n > table_density_cap(table_nonws[*item]))
        .map(|(item, _)| *item)
        .collect();
    for item in &over_table {
        log(&format!(
            "混淆层：item #{item} 的表格提案 {} 条超出每表聚合上限（{}），整表拒绝（疑似乱码表）",
            per_table_n[item],
            table_density_cap(table_nonws[item])
        ));
    }
    proposals.retain(|p| {
        let u = &units[cands[p.cand].unit];
        if u.table.is_some() && over_table.contains(&u.item_idx) {
            outcome.rejected += 1;
            return false;
        }
        true
    });

    // 6. 准入闸门：表内直接落地；表外走对抗式二次裁决
    let (in_table, out_of_table): (Vec<Proposal>, Vec<Proposal>) =
        proposals.into_iter().partition(|p| {
            let cand = &cands[p.cand];
            table.allowed(units[cand.unit].chars[cand.offset], p.after)
        });

    let mut accepted: Vec<(Proposal, &'static str)> =
        in_table.into_iter().map(|p| (p, "table")).collect();

    let verify_futs: Vec<_> = out_of_table
        .iter()
        .map(|p| {
            let cand = &cands[p.cand];
            let unit = &units[cand.unit];
            let before = unit.chars[cand.offset];
            let context = context_window(&unit.chars, cand.offset);
            verify_out_of_table(chat.clone(), before, p.after, context, &p.reason)
        })
        .collect();
    let verify_results: Vec<_> = futures::stream::iter(verify_futs)
        .buffered(concurrency)
        .collect()
        .await;
    for (p, result) in out_of_table.into_iter().zip(verify_results) {
        let cand = &cands[p.cand];
        let before = units[cand.unit].chars[cand.offset];
        match result {
            Ok((approved, reason, usage)) => {
                outcome.usage.prompt += usage.prompt_tokens;
                outcome.usage.completion += usage.completion_tokens;
                if approved {
                    accepted.push((p, "second_opinion"));
                } else {
                    outcome.rejected += 1;
                    log(&format!(
                        "混淆层：表外提案「{before}」→「{}」被二次裁决否决: {reason}",
                        p.after
                    ));
                }
            }
            Err(e) => {
                outcome.rejected += 1;
                log(&format!(
                    "混淆层：表外提案「{before}」→「{}」二次裁决失败，按拒绝处理: {e}",
                    p.after
                ));
            }
        }
    }

    // 7. 落地：按文档序排序 → 改字符 → 写回字段。
    // 普通字段逐单元重组；table_body 按 item 聚合成全串定点替换（base+局部偏移），
    // 标签骨架从未进过单元，逐字节原样保留是构造性保证。
    accepted.sort_by_key(|(p, _)| p.cand); // cands 本身按 (unit, offset) 文档序构造
    let mut touched: HashMap<usize, Vec<char>> = HashMap::new();
    let mut table_swaps: HashMap<usize, Vec<(usize, char, char)>> = HashMap::new();
    for (p, source) in &accepted {
        let cand = &cands[p.cand];
        let unit = &units[cand.unit];
        let before = unit.chars[cand.offset];
        let char_offset = match &unit.table {
            None => {
                touched
                    .entry(cand.unit)
                    .or_insert_with(|| unit.chars.clone())[cand.offset] = p.after;
                cand.offset
            }
            Some(pos) => {
                let global = pos.base + cand.offset;
                table_swaps
                    .entry(unit.item_idx)
                    .or_default()
                    .push((global, before, p.after));
                global
            }
        };
        outcome.fixes.push(ConfusionFix {
            item_id: items[unit.item_idx].id.clone(),
            field: unit.field.name(),
            char_offset,
            before: before.to_string(),
            after: p.after.to_string(),
            source: (*source).into(),
            note: p.reason.clone(),
        });
    }
    for (unit_idx, chars) in touched {
        let unit = &units[unit_idx];
        let s: String = chars.iter().collect();
        let item = &mut items[unit.item_idx].item;
        match unit.field {
            Field::Text => item.set("text", json!(s)),
            Field::ListItem(i) => set_array_elem(item, "list_items", i, s),
            Field::TableCaption(i) => set_array_elem(item, "table_caption", i, s),
            Field::TableBody => unreachable!("table_body 走 table_swaps 路径"),
        }
    }
    for (item_idx, swaps) in table_swaps {
        let item = &mut items[item_idx].item;
        let mut chars: Vec<char> = item
            .table_body()
            .expect("混淆层内部错误：有表格提案的 item 丢了 table_body")
            .chars()
            .collect();
        for (off, before, after) in swaps {
            // 偏移由我们自己从快照算出，错配 = 本层有 bug → panic，由调用方整层丢弃
            assert_eq!(
                chars[off], before,
                "混淆层内部错误：table_body 偏移 {off} 期望「{before}」"
            );
            chars[off] = after;
        }
        item.set("table_body", json!(chars.iter().collect::<String>()));
    }

    // observations 去重 + 截断
    let mut seen = HashSet::new();
    outcome.observations.retain(|o| seen.insert(o.clone()));
    outcome.observations.truncate(MAX_OBSERVATIONS);

    if !outcome.fixes.is_empty() || outcome.rejected > 0 {
        log(&format!(
            "混淆层：落地 {} 条替换，拒绝 {} 条提案",
            outcome.fixes.len(),
            outcome.rejected
        ));
    }
    (items, outcome)
}

fn set_array_elem(item: &mut crate::types::MineruItem, key: &str, i: usize, s: String) {
    if let Some(Value::Array(arr)) = item.0.get_mut(key)
        && let Some(slot) = arr.get_mut(i)
    {
        *slot = json!(s);
    }
}

/// 候选上下文窗口：两侧各 CONTEXT_WINDOW 字符，候选字符用 «» 标出，换行替换为 ⏎。
fn context_window(chars: &[char], offset: usize) -> String {
    let lo = offset.saturating_sub(CONTEXT_WINDOW);
    let hi = (offset + 1 + CONTEXT_WINDOW).min(chars.len());
    let mut out = String::new();
    for (i, c) in chars[lo..hi].iter().enumerate() {
        let c = if *c == '\n' { '⏎' } else { *c };
        if lo + i == offset {
            out.push('«');
            out.push(c);
            out.push('»');
        } else {
            out.push(c);
        }
    }
    out
}

struct JudgeReply {
    /// (批内候选编号, 替换字符, 理由)
    replacements: Vec<(usize, char, String)>,
    /// 结构非法的 replace 提案数（缺编号/replaceWith 不是恰好 1 字符）——解析期就拒
    invalid: u64,
    observations: Vec<String>,
    usage: Usage,
}

/// 表格候选的行列结构化上下文：标题；表头；所在行（候选单元格内 «» 标记，第 N 列）。
/// 单元格内容短，孤立判不了（"CE0" 是型号还是 CEO？），行列语义才是判别信息所在。
fn table_context(ctx: &TableCtx, pos: &TablePos, offset: usize) -> String {
    let trunc = |s: &str| -> String {
        let mut out: String = s.chars().take(30).collect();
        if s.chars().count() > 30 {
            out.push('…');
        }
        out.replace('\n', "⏎")
    };
    let mark_at = pos.prefix_in_cell + offset;
    let marked_cell: String = ctx.rows[pos.row][pos.col]
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let c = if c == '\n' { '⏎' } else { c };
            if i == mark_at {
                format!("«{c}»")
            } else {
                c.to_string()
            }
        })
        .collect();
    let row_render: String = ctx.rows[pos.row]
        .iter()
        .enumerate()
        .map(|(j, cell)| {
            if j == pos.col {
                marked_cell.clone()
            } else {
                trunc(cell)
            }
        })
        .collect::<Vec<_>>()
        .join("｜");
    let mut parts = Vec::new();
    if !ctx.caption.is_empty() {
        parts.push(format!("标题：{}", trunc(&ctx.caption)));
    }
    if pos.row > 0 {
        parts.push(format!(
            "表头：{}",
            ctx.rows[0]
                .iter()
                .map(|c| trunc(c))
                .collect::<Vec<_>>()
                .join("｜")
        ));
    }
    parts.push(format!("所在行：{row_render}（第{}列）", pos.col + 1));
    parts.join("；")
}

async fn judge_batch(
    batch: Range<usize>,
    cands: &[Cand],
    units: &[Unit],
    table_ctx: &HashMap<usize, TableCtx>,
    chat: Arc<dyn ChatClient>,
) -> Result<JudgeReply, LlmError> {
    let lines: Vec<String> = batch
        .clone()
        .enumerate()
        .map(|(local, cand_idx)| {
            let cand = &cands[cand_idx];
            let unit = &units[cand.unit];
            let ch = unit.chars[cand.offset];
            match &unit.table {
                None => format!(
                    "候选{local}（字符「{ch}」）：{}",
                    context_window(&unit.chars, cand.offset)
                ),
                Some(pos) => format!(
                    "候选{local}（字符「{ch}」）：〔表格单元格〕{}",
                    table_context(&table_ctx[&unit.item_idx], pos, cand.offset)
                ),
            }
        })
        .collect();
    let messages = vec![
        Message::System {
            content: JUDGE_SYSTEM_PROMPT.into(),
        },
        Message::User {
            content: format!(
                "以下是 {} 个候选位置，逐一裁决（绝大多数应该是 keep）：\n\n{}",
                batch.len(),
                lines.join("\n")
            ),
        },
    ];
    let r = chat.chat(&messages, &JUDGE_TOOLS).await?;
    parse_judge_reply(&r)
        .ok_or_else(|| LlmError("混淆裁决回复不含合法 judgeConfusions 调用".into()))
}

fn parse_judge_reply(r: &ChatResult) -> Option<JudgeReply> {
    let call = r
        .message
        .tool_calls
        .as_ref()?
        .iter()
        .find(|c| c.function.name == "judgeConfusions")?;
    let args = parse_json_safe(&call.function.arguments)?;
    let verdicts = args.get("verdicts")?.as_array()?;
    let mut replacements = Vec::new();
    let mut invalid = 0u64;
    for v in verdicts {
        let action = v.get("action").and_then(Value::as_str).unwrap_or("keep");
        if action != "replace" {
            continue;
        }
        let Some(index) = v.get("index").and_then(Value::as_u64) else {
            invalid += 1;
            continue;
        };
        // replaceWith 必须恰好 1 字符——结构闸门的第一道，在解析期就挡掉
        let rw = v.get("replaceWith").and_then(Value::as_str).unwrap_or("");
        let mut it = rw.chars();
        let (Some(c), None) = (it.next(), it.next()) else {
            invalid += 1;
            continue;
        };
        let reason = v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("（未给依据）")
            .to_string();
        replacements.push((index as usize, c, reason));
    }
    let observations = args
        .get("observations")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(JudgeReply {
        replacements,
        invalid,
        observations,
        usage: r.usage,
    })
}

/// 表外提案的对抗式二次裁决：独立对话，prompt 反着问，默认怀疑。
async fn verify_out_of_table(
    chat: Arc<dyn ChatClient>,
    before: char,
    after: char,
    context: String,
    proposer_reason: &str,
) -> Result<(bool, String, Usage), LlmError> {
    let messages = vec![
        Message::System {
            content: "你是 OCR 修正提案的对抗式审查者。你的职责是否决可疑提案，不是配合提案者。"
                .into(),
        },
        Message::User {
            content: format!(
                "有人主张把下文中 «» 标出的「{before}」改成「{after}」，理由：{proposer_reason}。\n\
                 该字符对不在已知 OCR 形近混淆表内。\n\n上下文：{context}\n\n\
                 严格审查：仅当「{before}」与「{after}」字形高度相似、确属 OCR 误认、且替换不改变语义时才 approve；\
                 任何怀疑（语义改动、字形不近、上下文不支持）一律 reject。调用 verifyConfusion 提交。"
            ),
        },
    ];
    let r = chat.chat(&messages, &VERIFY_TOOLS).await?;
    let call = r
        .message
        .tool_calls
        .as_ref()
        .and_then(|cs| cs.iter().find(|c| c.function.name == "verifyConfusion"))
        .ok_or_else(|| LlmError("二次裁决回复不含合法 verifyConfusion 调用".into()))?;
    let args = parse_json_safe(&call.function.arguments)
        .ok_or_else(|| LlmError("verifyConfusion arguments 解析失败".into()))?;
    let verdict = args.get("verdict").and_then(Value::as_str).unwrap_or("");
    if verdict != "approve" && verdict != "reject" {
        return Err(LlmError(format!("verifyConfusion verdict 非法: {verdict}")));
    }
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("（未给依据）")
        .to_string();
    Ok((verdict == "approve", reason, r.usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_builds_and_judges() {
        let t = ConfusionTable::build(&[]).unwrap();
        assert!(t.allowed('0', 'O') && t.allowed('O', '0'));
        assert!(t.allowed('入', 'λ') && t.allowed('竟', '竞'));
        assert!(!t.allowed('0', 'D'));
        assert!(t.is_candidate('B') && !t.is_candidate('好'));
    }

    #[test]
    fn extra_pairs_extend_table() {
        let t = ConfusionTable::build(&["0D".into()]).unwrap();
        assert!(t.allowed('0', 'D') && t.allowed('D', '0'));
        assert!(t.is_candidate('D'));
    }

    #[test]
    fn invalid_extra_pair_rejected() {
        assert!(ConfusionTable::build(&["abc".into()]).is_err());
        assert!(ConfusionTable::build(&["aa".into()]).is_err());
        assert!(ConfusionTable::build(&["a".into()]).is_err());
    }

    #[test]
    fn density_cap_is_sparse() {
        assert_eq!(density_cap(10), 2);
        assert_eq!(density_cap(100), 3);
        assert_eq!(density_cap(1000), 30);
    }

    #[test]
    fn context_window_marks_candidate() {
        let chars: Vec<char> = "公司CE0办公室".chars().collect();
        assert_eq!(context_window(&chars, 4), "公司CE«0»办公室");
    }
}
