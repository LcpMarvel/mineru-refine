// OCR 字符混淆修正层（opt-in，fix_ocr_confusion=true 才运行）：
// 核心 refine 出口闸门【之后】的独立后处理。核心层承诺（C_out ⊆ C_in）不变，
// 本层叠加第二份契约：所有修改都是稀疏的一换一定点替换，每条要么在混淆表内，
// 要么通过独立二次裁决；全量进 report/provenance，可审计可撤销。
//
// 权力结构：LLM 只有提案权，没有写入权——
//   闸门 1（结构）：恰好 1 字符、与原字符不同、密度上限内（OCR 混淆是稀疏的）；
//   闸门 2（准入）：(before, after) 同属混淆等价类 → 直接落地；
//                   定点频率投票候选命中多数派写法 → 直接落地（source=frequency_vote）；
//                   其余表外提案 → 对抗式二次裁决，通过才落地（source=second_opinion）。
// 层内任何 LLM 故障 → 搁置对应批次（漏修，不毁全局）；层级 panic 由调用方兜（丢弃整层）。
//
// table_body（Phase 2）：标签骨架与文本节点词法分离，只有 td/th 单元格内的文本
// 可成为候选——标记字符（colspan=1 的 1）在构造上就不可能被替换，HTML 实体当黑盒跳过。
// 表格候选用行列结构化上下文裁决（标题/表头/所在行），并多一道每表聚合密度闸门
//（乱码表会诱发大量"修复"，整表按住——乱码表的归宿是整表裁决，不是逐字替换）。
//
// 频率投票（Phase 3）：全文频率做动态准入/排误——
//   排误（加白）：候选字与邻字构成的高频词全文一致出现（≥5 次）且任何类内替代写法
//   从未出现 → 大概率真术语（实证：「烟感」产品线 ×5），跳过送审；
//   召回（拉丁 token 投票）：OGSTM×2 vs OGSMT×20 → 少数派写法生成定点候选，
//   命中多数派写法的修复免二次裁决（差异本身就是全文实证）。
//
// observations 闭环（Phase 3）：本轮裁决顺带报告的「X 应为 Y」表外观察，解析出
// 单字形近替换后生成定点候选做第二轮裁决（三道机械闸门照旧）——回收已花掉的 token。
// 防循环：最多一轮回灌，第二轮的 observations 只记录不再回灌。

use crate::agent_loop::Logger;
use crate::llm::{ChatClient, ChatResult, LlmError, Message, Usage, parse_json_safe};
use crate::types::{ConfusionFix, RefItem, TokenUsage};
use futures::StreamExt;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::LazyLock;

/// 混淆层独立的 prompt 版本（与核心层 PROMPT_VERSION 分开演进，进缓存 key）。
/// c2：table_body 进入处理范围 + 表格行列上下文 + 白名单实证扩充（扞杆/亭亨）。
/// c3：中文形近对扩充（校较/酒源/军率）+ 全文频率投票 + observations 闭环回灌。
pub const CONFUSION_PROMPT_VERSION: &str = "c3";

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
    // 实证扩充（Phase 3 真实文档 observations）：比校→比较、数据来酒→来源、合格军→合格率
    "校较",
    "酒源",
    "军率",
];

/// 单次裁决调用最多带多少个候选（再多就分批，避免撑爆单次输出）。
const MAX_CANDIDATES_PER_CALL: usize = 40;
/// 候选两侧各取多少字符做上下文窗口。
const CONTEXT_WINDOW: usize = 40;
/// observations 上限（LLM 顺带报告的表外问题，只记不改）。
const MAX_OBSERVATIONS: usize = 50;
/// observations 回灌生成的定点候选上限（防观察噪声撑爆第二轮）。
const MAX_FEEDBACK_CANDS: usize = 40;
/// 频率加白门槛：候选字所在词全文一致出现 ≥ 该次数（且无任何变体写法）→ 跳过送审。
const VOTE_WHITELIST_MIN: u32 = 5;

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
    /// 与 BUILTIN_CONFUSION_CLASSES 一一对应（alternatives 的确定性迭代源）。
    classes: Vec<Vec<char>>,
    extra: HashSet<(char, char)>,
    /// 候选字符全集（class_of 的键 ∪ extra 两侧字符）：扫描热路径上每字符查一次。
    candidates: HashSet<char>,
}

impl ConfusionTable {
    /// extra_pairs 每项必须恰好 2 个不同字符（如 "0D"，表示 0↔D 互换可直接落地）。
    /// 非法配置早抛——这是调用方的配置错误，不静默吞。
    pub fn build(extra_pairs: &[String]) -> Result<Self, String> {
        let mut class_of = HashMap::new();
        let mut classes: Vec<Vec<char>> = Vec::new();
        for (idx, class) in BUILTIN_CONFUSION_CLASSES.iter().enumerate() {
            let members: Vec<char> = class.chars().collect();
            for c in &members {
                class_of.insert(*c, idx);
            }
            classes.push(members);
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
            classes,
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

    /// c 的全部形近替代字符（类内成员 + 补充对伙伴），确定性顺序。
    fn alternatives(&self, c: char) -> Vec<char> {
        let mut out: Vec<char> = Vec::new();
        if let Some(&idx) = self.class_of.get(&c) {
            out.extend(self.classes[idx].iter().copied().filter(|&x| x != c));
        }
        let mut partners: Vec<char> = self
            .extra
            .iter()
            .filter(|(a, _)| *a == c)
            .map(|(_, b)| *b)
            .collect();
        partners.sort_unstable();
        for p in partners {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
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
    /// HTML 实体的字符区间（仅 table 节点非空）：实体内字符不可成为候选
    entities: Vec<(usize, usize)>,
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
    /// 附给裁决 prompt 的辅助证据（频率投票/前轮观察）
    note: Option<String>,
    /// 频率投票建议的多数派字符：LLM 提案与之一致时免二次裁决直接落地
    vote_after: Option<char>,
}

/// LLM 提案（已过结构闸门的原始形态）。
struct Proposal {
    cand: usize,
    after: char,
    reason: String,
}

// ── 频率统计 ──

fn is_hanzi(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}')
}

/// 拉丁/数字 token（连续 [A-Za-z0-9] 段）及其起始字符偏移。
/// 只收 ≥4 字符、至多 1 个数字的 token：纯数字/短编号（年份、序号）天然多变体，
/// 频率投票对它们就是灾难（2026 少数 ≠ 2025 写错）。
fn latin_tokens(chars: &[char]) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                i += 1;
            }
            let tok: String = chars[start..i].iter().collect();
            let len = i - start;
            let alpha = tok.chars().filter(|c| c.is_ascii_alphabetic()).count();
            if len >= 4 && alpha >= len - 1 {
                out.push((tok, start));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// 全文频率统计：CJK 双字词频 + 拉丁 token 频次（候选筛选前对全部单元构建）。
struct FreqStats {
    bigrams: HashMap<(char, char), u32>,
    tokens: HashMap<String, u32>,
}

impl FreqStats {
    fn build(units: &[Unit]) -> Self {
        let mut bigrams: HashMap<(char, char), u32> = HashMap::new();
        let mut tokens: HashMap<String, u32> = HashMap::new();
        for u in units {
            for w in u.chars.windows(2) {
                if is_hanzi(w[0]) && is_hanzi(w[1]) {
                    *bigrams.entry((w[0], w[1])).or_insert(0) += 1;
                }
            }
            for (tok, _) in latin_tokens(&u.chars) {
                *tokens.entry(tok).or_insert(0) += 1;
            }
        }
        Self { bigrams, tokens }
    }

    fn bigram(&self, a: char, b: char) -> u32 {
        self.bigrams.get(&(a, b)).copied().unwrap_or(0)
    }

    /// 频率加白（排误）：候选字与紧邻汉字构成的词全文一致出现 ≥ VOTE_WHITELIST_MIN 次，
    /// 且任何替代写法（alts 替换该字后的变体词）从未出现 → 大概率真术语，跳过送审。
    fn whitelisted(&self, chars: &[char], offset: usize, alts: &[char]) -> bool {
        let c = chars[offset];
        let prev = (offset > 0)
            .then(|| chars[offset - 1])
            .filter(|p| is_hanzi(*p));
        let next = chars.get(offset + 1).copied().filter(|n| is_hanzi(*n));
        let own_l = prev.map(|p| self.bigram(p, c)).unwrap_or(0);
        let own_r = next.map(|n| self.bigram(c, n)).unwrap_or(0);
        if own_l < VOTE_WHITELIST_MIN && own_r < VOTE_WHITELIST_MIN {
            return false;
        }
        for &a in alts {
            if let Some(p) = prev
                && self.bigram(p, a) > 0
            {
                return false;
            }
            if let Some(n) = next
                && self.bigram(a, n) > 0
            {
                return false;
            }
        }
        true
    }

    /// 少数派注记（召回）：候选字所在词是全文少数派写法、且存在类内替代字构成的
    /// 高频多数派写法 → 给裁决 prompt 附频率证据。
    fn minority_note(&self, chars: &[char], offset: usize, alts: &[char]) -> Option<String> {
        let c = chars[offset];
        let sides = [
            (offset > 0)
                .then(|| chars[offset - 1])
                .filter(|p| is_hanzi(*p))
                .map(|p| (p, true)),
            chars
                .get(offset + 1)
                .copied()
                .filter(|n| is_hanzi(*n))
                .map(|n| (n, false)),
        ];
        for side in sides.into_iter().flatten() {
            let (nb, nb_is_prev) = side;
            let word = |x: char| -> (String, u32) {
                if nb_is_prev {
                    (format!("{nb}{x}"), self.bigram(nb, x))
                } else {
                    (format!("{x}{nb}"), self.bigram(x, nb))
                }
            };
            let (own_word, own) = word(c);
            for &a in alts {
                let (rival_word, rival) = word(a);
                if rival >= 3 && rival >= 3 * own.max(1) {
                    return Some(format!(
                        "频率投票：全文「{own_word}」×{own}、「{rival_word}」×{rival}，少数派写法可疑"
                    ));
                }
            }
        }
        None
    }
}

/// 少数派 token 内的单字替换：(token 内字符位置, 多数派字符)。
type TokenSubs = Vec<(usize, char)>;

/// 拉丁 token 频率投票：少数派 token 与高频多数派 token 仅差一处单字替换或
/// 一处相邻换位（OGSTM vs OGSMT）→ 差异位生成定点候选，建议改成多数派字符。
/// 返回 (unit, offset) → (建议字符, 注记)。
fn latin_token_votes(units: &[Unit], stats: &FreqStats) -> HashMap<(usize, usize), (char, String)> {
    // token 排序保证确定性输出
    let mut sorted: Vec<(&str, u32)> = stats.tokens.iter().map(|(t, n)| (t.as_str(), *n)).collect();
    sorted.sort_unstable();

    // 少数派 token → 各差异位的 (位置, 多数派字符) + 注记
    let mut corrections: HashMap<&str, (TokenSubs, String)> = HashMap::new();
    for &(t, ct) in &sorted {
        let tc: Vec<char> = t.chars().collect();
        let mut best: Option<(&str, u32, TokenSubs)> = None;
        for &(u, cu) in &sorted {
            if u == t || cu < 4 || cu < 3 * ct {
                continue;
            }
            let uc: Vec<char> = u.chars().collect();
            if uc.len() != tc.len() {
                continue;
            }
            let diffs: Vec<usize> = (0..tc.len()).filter(|&i| tc[i] != uc[i]).collect();
            let subs: Option<TokenSubs> = match diffs.as_slice() {
                [i] => Some(vec![(*i, uc[*i])]),
                // 相邻换位：两处单字替换可恢复（每条仍是一换一）
                [i, j] if *j == *i + 1 && tc[*i] == uc[*j] && tc[*j] == uc[*i] => {
                    Some(vec![(*i, uc[*i]), (*j, uc[*j])])
                }
                _ => None,
            };
            if let Some(subs) = subs
                && best.as_ref().map(|(_, bc, _)| cu > *bc).unwrap_or(true)
            {
                best = Some((u, cu, subs));
            }
        }
        if let Some((u, cu, subs)) = best {
            let note = format!("频率投票：全文「{u}」×{cu}、「{t}」×{ct}，疑为「{u}」的形误");
            corrections.insert(t, (subs, note));
        }
    }
    if corrections.is_empty() {
        return HashMap::new();
    }

    let mut out: HashMap<(usize, usize), (char, String)> = HashMap::new();
    for (ui, unit) in units.iter().enumerate() {
        for (tok, start) in latin_tokens(&unit.chars) {
            if let Some((subs, note)) = corrections.get(tok.as_str()) {
                for (pos, after) in subs {
                    let offset = start + pos;
                    if !in_spans(offset, &unit.entities) {
                        out.insert((ui, offset), (*after, note.clone()));
                    }
                }
            }
        }
    }
    out
}

// ── observations 闭环 ──

/// 观察文本里的「X 应为 Y」修正结构（兼容 「」/“”/" 三种引号与 →/-> 箭头）。
static OBS_CORRECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"[「“"]([^「」“”"]{2,20})[」”"]\s*(?:疑?似?应该?[为是]|实[为是]|当为|应?改为|→|->)\s*[「“"]([^「」“”"]{1,20})[」”"]"#,
    )
    .unwrap()
});

/// 对齐「错写 → 正写」为单字替换：返回 (错写中的差异位, 多数派字符) 列表。
/// 等长 → 恰差 1 位才认；正写更短 → 在错写上滑窗找恰差 1 位的窗口（可能多个，
/// 全部生成候选交 LLM 结合上下文裁决，闸门兜底）；正写更长（要加字）→ 超能力范围。
fn align_corrections(wrong: &[char], right: &[char]) -> Vec<(usize, char)> {
    let mut out = Vec::new();
    if wrong.len() == right.len() {
        let diffs: Vec<usize> = (0..wrong.len()).filter(|&i| wrong[i] != right[i]).collect();
        if let [i] = diffs.as_slice() {
            out.push((*i, right[*i]));
        }
        return out;
    }
    if right.len() >= 2 && right.len() < wrong.len() {
        for start in 0..=(wrong.len() - right.len()) {
            let diffs: Vec<usize> = (0..right.len())
                .filter(|&k| wrong[start + k] != right[k])
                .collect();
            if let [k] = diffs.as_slice() {
                out.push((start + *k, right[*k]));
                if out.len() >= 3 {
                    break; // 歧义窗口截断：再多就是噪声
                }
            }
        }
    }
    out
}

/// 从第一轮 observations 解析修正对，搜索全文生成定点候选（第二轮裁决用）。
/// 排误：频率加白的位置（「烟感」反例）与第一轮已送审的位置都跳过。
fn feedback_candidates(
    observations: &[String],
    units: &[Unit],
    stats: &FreqStats,
    judged: &HashSet<(usize, usize)>,
) -> Vec<Cand> {
    let mut out: Vec<Cand> = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for obs in observations {
        for caps in OBS_CORRECTION.captures_iter(obs) {
            let wrong: Vec<char> = caps[1].chars().collect();
            let right: Vec<char> = caps[2].chars().collect();
            let subs = align_corrections(&wrong, &right);
            if subs.is_empty() {
                continue;
            }
            for (ui, unit) in units.iter().enumerate() {
                if unit.chars.len() < wrong.len() {
                    continue;
                }
                for start in 0..=(unit.chars.len() - wrong.len()) {
                    if unit.chars[start..start + wrong.len()] != wrong[..] {
                        continue;
                    }
                    for &(di, after) in &subs {
                        let offset = start + di;
                        if unit.chars[offset] == after
                            || in_spans(offset, &unit.entities)
                            || judged.contains(&(ui, offset))
                            || !seen.insert((ui, offset))
                        {
                            continue;
                        }
                        // 频率加白排误：该位置所在词是全文一致的高频写法 → 真术语，不回灌
                        if stats.whitelisted(&unit.chars, offset, &[after]) {
                            continue;
                        }
                        if out.len() >= MAX_FEEDBACK_CANDS {
                            return out;
                        }
                        out.push(Cand {
                            unit: ui,
                            offset,
                            note: Some(format!(
                                "前轮观察：「{}」疑应为「{}」（当时不在候选范围）",
                                &caps[1], &caps[2]
                            )),
                            vote_after: None,
                        });
                    }
                }
            }
        }
    }
    out
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
                "description": "候选之外观察到的其他 OCR 质量问题（只记录，不会被应用）。若能给出修正，用「错写」应为「正写」的格式" },
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
部分候选附有「频率投票」（全文多数派/少数派写法统计）或「前轮观察」辅助证据，可作为判断依据，但仍须结合上下文确认。
只做字符级一换一修正，绝不润色、增删、改写。
若在候选之外观察到其他 OCR 质量问题（乱码、明显错字），写进 observations（仅记录，系统不会应用）；能给出修正的用「错写」应为「正写」格式。
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

/// 对 items 的 text/list_items/table_caption/table_body 做混淆修正。
/// 取得 items 所有权：调用方在 panic 时（catch_unwind）保留原件，天然整层丢弃。
pub async fn fix_confusions(
    mut items: Vec<RefItem>,
    chat: Arc<dyn ChatClient>,
    concurrency: usize,
    table: &ConfusionTable,
    log: &Logger,
) -> (Vec<RefItem>, ConfusionOutcome) {
    let mut outcome = ConfusionOutcome::default();
    let concurrency = concurrency.max(1);

    // 1. 字段单元快照（全量，先于候选筛选——频率统计要看全文）
    let mut units: Vec<Unit> = Vec::new();
    let mut table_ctx: HashMap<usize, TableCtx> = HashMap::new();
    for (item_idx, r) in items.iter().enumerate() {
        let mut push_plain = |field: Field, s: &str| {
            units.push(Unit {
                item_idx,
                field,
                chars: s.chars().collect(),
                table: None,
                entities: Vec::new(),
            });
        };
        if let Some(t) = r.item.text() {
            push_plain(Field::Text, t);
        }
        for (key, make) in [
            ("list_items", Field::ListItem as fn(usize) -> Field),
            ("table_caption", Field::TableCaption as fn(usize) -> Field),
        ] {
            if let Some(parts) = r.item.str_array(key) {
                for (i, p) in parts.iter().enumerate() {
                    push_plain(make(i), p);
                }
            }
        }
        if let Some(tb) = r.item.table_body() {
            let (nodes, rows) = parse_table_nodes(tb);
            if nodes.is_empty() {
                continue;
            }
            for node in nodes {
                let entities = entity_spans(&node.chars);
                units.push(Unit {
                    item_idx,
                    field: Field::TableBody,
                    chars: node.chars,
                    table: Some(node.pos),
                    entities,
                });
            }
            let caption = r
                .item
                .str_array("table_caption")
                .map(|c| c.join("；"))
                .unwrap_or_default();
            table_ctx.insert(item_idx, TableCtx { caption, rows });
        }
    }

    // 2. 全文频率统计 + 拉丁 token 投票
    let stats = FreqStats::build(&units);
    let votes = latin_token_votes(&units, &stats);
    let mut votes_by_unit: HashMap<usize, BTreeMap<usize, (char, String)>> = HashMap::new();
    for ((ui, off), v) in votes {
        votes_by_unit.entry(ui).or_default().insert(off, v);
    }

    // 3. 候选扫描（文档序，输出确定性的根）：
    //    准入名单字符（频率加白的跳过、少数派写法附注记）∪ 拉丁投票定点候选
    let mut cands: Vec<Cand> = Vec::new();
    let mut whitelisted = 0u64;
    for (ui, unit) in units.iter().enumerate() {
        let unit_votes = votes_by_unit.remove(&ui).unwrap_or_default();
        let mut offsets: BTreeMap<usize, Cand> = BTreeMap::new();
        for (i, &c) in unit.chars.iter().enumerate() {
            if !table.is_candidate(c) || in_spans(i, &unit.entities) {
                continue;
            }
            let alts = table.alternatives(c);
            if stats.whitelisted(&unit.chars, i, &alts) {
                whitelisted += 1;
                continue;
            }
            offsets.insert(
                i,
                Cand {
                    unit: ui,
                    offset: i,
                    note: stats.minority_note(&unit.chars, i, &alts),
                    vote_after: None,
                },
            );
        }
        for (off, (after, note)) in unit_votes {
            let cand = offsets.entry(off).or_insert(Cand {
                unit: ui,
                offset: off,
                note: None,
                vote_after: None,
            });
            cand.note = Some(note);
            cand.vote_after = Some(after);
        }
        cands.extend(offsets.into_values());
    }
    if cands.is_empty() {
        return (items, outcome);
    }
    if whitelisted > 0 {
        log(&format!(
            "混淆层：频率投票加白 {whitelisted} 个候选位置（全文一致的高频写法，跳过送审）"
        ));
    }

    // 4. 第一轮裁决
    let round1 = 0..cands.len();
    log(&format!(
        "混淆层：{} 个候选字符，分 {} 次裁决",
        cands.len(),
        round1.len().div_ceil(MAX_CANDIDATES_PER_CALL)
    ));
    let mut proposals: Vec<Proposal> = Vec::new();
    let round1_obs = judge_range(
        round1.clone(),
        &cands,
        &units,
        &table_ctx,
        &chat,
        concurrency,
        &mut outcome,
        &mut proposals,
        log,
    )
    .await;
    outcome.observations.extend(round1_obs.iter().cloned());

    // 4b. observations 闭环回灌（最多一轮）：解析「X 应为 Y」→ 定点候选 → 第二轮裁决。
    // 第二轮的 observations 只记录不再回灌（防循环）。
    let judged: HashSet<(usize, usize)> = cands.iter().map(|c| (c.unit, c.offset)).collect();
    let feedback = feedback_candidates(&round1_obs, &units, &stats, &judged);
    if !feedback.is_empty() {
        log(&format!(
            "混淆层：observations 回灌生成 {} 个定点候选，第二轮裁决",
            feedback.len()
        ));
        let start = cands.len();
        cands.extend(feedback);
        let round2_obs = judge_range(
            start..cands.len(),
            &cands,
            &units,
            &table_ctx,
            &chat,
            concurrency,
            &mut outcome,
            &mut proposals,
            log,
        )
        .await;
        outcome.observations.extend(round2_obs);
    }

    // 5. 结构闸门：去重（同一位置只认第一条提案）+ 与原字符不同
    let mut seen_pos: HashSet<(usize, usize)> = HashSet::new();
    proposals.retain(|p| {
        let cand = &cands[p.cand];
        let before = units[cand.unit].chars[cand.offset];
        if p.after == before {
            return false; // replace 成同字符 = 无操作，静默丢弃不计 rejected
        }
        if !seen_pos.insert((cand.unit, cand.offset)) {
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

    // 6. 密度闸门：单元内提案数超过稀疏上限 → 整单元拒绝（先于二次裁决，省调用）
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

    // 6b. 每表聚合密度闸门：单格各自合规但整表提案过多 = 乱码表特征，整表拒绝
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

    // 7. 准入闸门：表内直接落地；频率投票定点候选命中多数派写法也直接落地；
    //    其余表外提案走对抗式二次裁决（附辅助证据）。
    let mut accepted: Vec<(Proposal, &'static str)> = Vec::new();
    let mut to_verify: Vec<Proposal> = Vec::new();
    for p in proposals {
        let cand = &cands[p.cand];
        let before = units[cand.unit].chars[cand.offset];
        if table.allowed(before, p.after) {
            accepted.push((p, "table"));
        } else if cand.vote_after == Some(p.after) {
            accepted.push((p, "frequency_vote"));
        } else {
            to_verify.push(p);
        }
    }

    let verify_futs: Vec<_> = to_verify
        .iter()
        .map(|p| {
            let cand = &cands[p.cand];
            let unit = &units[cand.unit];
            let before = unit.chars[cand.offset];
            let context = context_window(&unit.chars, cand.offset);
            verify_out_of_table(
                chat.clone(),
                before,
                p.after,
                context,
                &p.reason,
                cand.note.as_deref(),
            )
        })
        .collect();
    let verify_results: Vec<_> = futures::stream::iter(verify_futs)
        .buffered(concurrency)
        .collect()
        .await;
    for (p, result) in to_verify.into_iter().zip(verify_results) {
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

    // 8. 落地：按文档序排序 → 改字符 → 写回字段。
    // 普通字段逐单元重组；table_body 按 item 聚合成全串定点替换（base+局部偏移），
    // 标签骨架从未进过单元，逐字节原样保留是构造性保证。
    accepted.sort_by_key(|(p, _)| {
        let cand = &cands[p.cand];
        (cand.unit, cand.offset)
    });
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

/// 对 cands[range] 做一轮批量裁决：提案推进 proposals（结构非法的在解析期拒），
/// usage 记入 outcome，返回该轮收集到的 observations（是否回灌由调用方定）。
#[allow(clippy::too_many_arguments)]
async fn judge_range(
    range: Range<usize>,
    cands: &[Cand],
    units: &[Unit],
    table_ctx: &HashMap<usize, TableCtx>,
    chat: &Arc<dyn ChatClient>,
    concurrency: usize,
    outcome: &mut ConfusionOutcome,
    proposals: &mut Vec<Proposal>,
    log: &Logger,
) -> Vec<String> {
    let batches: Vec<Range<usize>> = range
        .clone()
        .step_by(MAX_CANDIDATES_PER_CALL)
        .map(|s| s..(s + MAX_CANDIDATES_PER_CALL).min(range.end))
        .collect();
    // 先把 future 收集成 Vec（绕开 rustc 对惰性迭代器 + async 块的高阶生命周期误判）
    let judge_futs: Vec<_> = batches
        .iter()
        .map(|batch| judge_batch(batch.clone(), cands, units, table_ctx, chat.clone()))
        .collect();
    let judge_results: Vec<_> = futures::stream::iter(judge_futs)
        .buffered(concurrency.max(1))
        .collect()
        .await;

    let mut observations: Vec<String> = Vec::new();
    let mut shelved_batches = 0u64;
    for (batch, result) in batches.iter().zip(judge_results) {
        match result {
            Ok(reply) => {
                outcome.usage.prompt += reply.usage.prompt_tokens;
                outcome.usage.completion += reply.usage.completion_tokens;
                observations.extend(reply.observations);
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
    observations
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
            let note = cand
                .note
                .as_ref()
                .map(|n| format!("（{n}）"))
                .unwrap_or_default();
            match &unit.table {
                None => format!(
                    "候选{local}（字符「{ch}」）：{}{note}",
                    context_window(&unit.chars, cand.offset)
                ),
                Some(pos) => format!(
                    "候选{local}（字符「{ch}」）：〔表格单元格〕{}{note}",
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
/// extra_evidence：候选携带的频率投票/前轮观察注记——非形近但全文实证充分的
/// 替换（「全2预测」→「全年预测」）靠它过审，无证据时仍按字形从严。
async fn verify_out_of_table(
    chat: Arc<dyn ChatClient>,
    before: char,
    after: char,
    context: String,
    proposer_reason: &str,
    extra_evidence: Option<&str>,
) -> Result<(bool, String, Usage), LlmError> {
    let evidence_line = extra_evidence
        .map(|e| format!("辅助证据：{e}\n"))
        .unwrap_or_default();
    let messages = vec![
        Message::System {
            content: "你是 OCR 修正提案的对抗式审查者。你的职责是否决可疑提案，不是配合提案者。"
                .into(),
        },
        Message::User {
            content: format!(
                "有人主张把下文中 «» 标出的「{before}」改成「{after}」，理由：{proposer_reason}。\n\
                 该字符对不在已知 OCR 形近混淆表内。\n\n上下文：{context}\n{evidence_line}\n\
                 严格审查：仅当确属 OCR 误认（「{before}」与「{after}」字形高度相似，或辅助证据——\
                 全文频率投票/前轮观察——充分支持）且替换不改变语义时才 approve；\
                 任何怀疑（语义改动、证据不足、上下文不支持）一律 reject。调用 verifyConfusion 提交。"
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
        // c3 实证扩充
        assert!(t.allowed('校', '较') && t.allowed('酒', '源') && t.allowed('军', '率'));
    }

    #[test]
    fn alternatives_are_deterministic() {
        let t = ConfusionTable::build(&["0D".into()]).unwrap();
        assert_eq!(t.alternatives('0'), vec!['O', 'D']);
        assert_eq!(t.alternatives('校'), vec!['较']);
        assert!(t.alternatives('好').is_empty());
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

    #[test]
    fn latin_tokens_skip_numbers_and_shorts() {
        let chars: Vec<char> = "OGSMT 与 2025 年的 CE0 与 MN001X".chars().collect();
        let toks: Vec<String> = latin_tokens(&chars).into_iter().map(|(t, _)| t).collect();
        // 2025 纯数字、CE0 过短、MN001X 数字超 1 个 → 全部排除
        assert_eq!(toks, vec!["OGSMT"]);
    }

    #[test]
    fn align_corrections_cases() {
        let c = |s: &str| s.chars().collect::<Vec<char>>();
        // 等长单字差
        assert_eq!(
            align_corrections(&c("数据来酒"), &c("数据来源")),
            vec![(3, '源')]
        );
        // 正写更短：滑窗
        assert_eq!(
            align_corrections(&c("2025年全2预测偏差"), &c("全年"))
                .iter()
                .map(|(i, a)| (*i, *a))
                .collect::<Vec<_>>(),
            vec![(3, '全'), (6, '年')] // 「5年」「全2」两个歧义窗口都出候选，交 LLM 裁决
        );
        // 等长多字差 / 要加字 → 不认
        assert!(align_corrections(&c("甲乙"), &c("丙丁")).is_empty());
        assert!(align_corrections(&c("全年"), &c("全年预测")).is_empty());
    }

    #[test]
    fn obs_correction_regex_matches_quote_styles() {
        for s in [
            "表格中「数据来酒」应为「数据来源」",
            "“数据来酒”应该是“数据来源”之误",
            "「数据来酒」→「数据来源」",
        ] {
            let caps = OBS_CORRECTION.captures(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!(&caps[1], "数据来酒");
            assert_eq!(&caps[2], "数据来源");
        }
    }
}
