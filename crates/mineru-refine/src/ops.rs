// 9 个削减/重组 op（纯削减全集，不加字）。纯函数：(items, args) -> 新 items，绝不突变入参。
// apply_op_checked 是唯一对外执行入口：执行 + 保真闸 + 几何派生，违反即回滚（丢弃副本）。
// 参数一律稳定 ID。op 自身参数非法（ID 不存在 / 不相邻 / 不在白名单）直接报错。

use crate::id::{IdGen, must_index_of_id};
use crate::invariant::{check_fidelity, is_js_whitespace, non_ws_len, table_rows};
use crate::types::{MineruItem, OpCall, RefItem, RemovedSpan, StripPattern, is_page_furniture};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

struct OpOutcome {
    items: Vec<RefItem>,
    removed_spans: Vec<RemovedSpan>,
}

type OpResult = Result<OpOutcome, String>;

/// 合并类 op 共用的相邻性检查：idB 须在 idA 之后，两者之间只允许隔页面家具（原位保留）。
fn adjacent_pair(
    op_name: &str,
    items: &[RefItem],
    id_a: &str,
    id_b: &str,
) -> Result<(usize, usize, Vec<RefItem>), String> {
    let ia = must_index_of_id(items, id_a)?;
    let ib = must_index_of_id(items, id_b)?;
    if ib <= ia {
        return Err(format!(
            "{op_name} 要求 {id_b} 在 {id_a} 之后（实际位置 {ia} / {ib}）"
        ));
    }
    let between: Vec<RefItem> = items[ia + 1..ib].to_vec();
    if let Some(blocker) = between
        .iter()
        .find(|r| !is_page_furniture(r.item.item_type()))
    {
        return Err(format!(
            "{op_name} 被拒：{id_a} 与 {id_b} 之间隔着内容块 {}（type={}），仅允许隔页面家具",
            blocker.id,
            blocker.item.item_type()
        ));
    }
    Ok((ia, ib, between))
}

/// JS 数字语义：整数值序列化为整数（10 而非 10.0），保证输出逐字节稳定。
fn js_num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() <= i64::MAX as f64 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// 合并类 op 共用的 bbox 并集（跨页时取并集是近似，但保证仍能回指源块区域）。
fn union_bbox(merged: &mut MineruItem, a: &MineruItem, b: &MineruItem) {
    if let (Some(ba), Some(bb)) = (a.bbox(), b.bbox()) {
        merged.set(
            "bbox",
            Value::Array(vec![
                js_num(ba[0].min(bb[0])),
                js_num(ba[1].min(bb[1])),
                js_num(ba[2].max(bb[2])),
                js_num(ba[3].max(bb[3])),
            ]),
        );
    }
}

fn trim_end_ws(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

fn trim_start_ws(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

// 英文断词处补一个空格（空白符在保真白名单内，不计入 C 比对）；中文直接拼。
fn glue_for(head: &str, tail: &str) -> &'static str {
    let head_ok = head
        .chars()
        .last()
        .map(|c| c.is_ascii_alphanumeric() || c == ',' || c == ';')
        .unwrap_or(false);
    let tail_ok = tail
        .chars()
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false);
    if head_ok && tail_ok { " " } else { "" }
}

/// 合并类 op 共用的收尾：用 merged 替换 [ia..=ib]，A/B 之间的页面家具原位保留在合并块之后。
fn splice_merged(
    items: &[RefItem],
    ia: usize,
    ib: usize,
    merged: MineruItem,
    between: Vec<RefItem>,
    next_id: &IdGen,
) -> Vec<RefItem> {
    let mut out: Vec<RefItem> = Vec::with_capacity(items.len());
    out.extend_from_slice(&items[..ia]);
    out.push(RefItem {
        id: next_id.next(),
        item: merged,
    });
    out.extend(between);
    out.extend_from_slice(&items[ib + 1..]);
    out
}

// ── merge(idA, idB)：两 text 块拼成一块（修跨页断句）。bbox=并集，page_idx 取首块。──
fn op_merge(items: &[RefItem], next_id: &IdGen, id_a: &str, id_b: &str) -> OpResult {
    let (ia, ib, between) = adjacent_pair("merge", items, id_a, id_b)?;
    let a = &items[ia].item;
    let b = &items[ib].item;
    if a.item_type() != "text" || b.item_type() != "text" {
        return Err(format!(
            "merge 仅限 text 块（实际 {} + {}）",
            a.item_type(),
            b.item_type()
        ));
    }
    let (Some(at), Some(bt)) = (a.text(), b.text()) else {
        return Err("merge 的两块都必须有 text".into());
    };

    let head = trim_end_ws(at);
    let tail = trim_start_ws(bt);
    let mut merged = a.clone();
    merged.set(
        "text",
        Value::String(format!("{head}{}{tail}", glue_for(head, tail))),
    );
    union_bbox(&mut merged, a, b);
    Ok(OpOutcome {
        items: splice_merged(items, ia, ib, merged, between, next_id),
        removed_spans: vec![],
    })
}

/// b"</tr>" 在 body 中最后一次出现（ASCII 大小写不敏感）之后的字节位置。
fn last_tr_close_end(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    bytes
        .windows(5)
        .rposition(|w| w.eq_ignore_ascii_case(b"</tr>"))
        .map(|pos| pos + 5)
}

// ── mergeTable(idA, idB)：跨页被拆的两个表合成一个。B 的 <tr> 行按原字节追加到 A 的
// 末行之后（A 的外壳字节原样保留）；caption/footnote 数组拼接；bbox=并集，page_idx 取首块。
// 唯一允许的削减：B 首行与 A 首行逐字节相等（每页重印的表头）→ 去掉并留痕。──
fn op_merge_table(items: &[RefItem], next_id: &IdGen, id_a: &str, id_b: &str) -> OpResult {
    let (ia, ib, between) = adjacent_pair("mergeTable", items, id_a, id_b)?;
    let a = &items[ia].item;
    let b = &items[ib].item;
    if a.item_type() != "table" || b.item_type() != "table" {
        return Err(format!(
            "mergeTable 仅限 table 块（实际 {} + {}）",
            a.item_type(),
            b.item_type()
        ));
    }
    let a_body = a.table_body().unwrap_or("");
    let b_body = b.table_body().unwrap_or("");
    let a_rows = table_rows(a_body);
    let mut b_rows = table_rows(b_body);
    if a_rows.is_empty() || b_rows.is_empty() {
        return Err(format!(
            "mergeTable 被拒：{} 没有表格行（空壳表应 drop，不是 merge）",
            if a_rows.is_empty() { id_a } else { id_b }
        ));
    }

    let mut removed_spans: Vec<RemovedSpan> = Vec::new();
    if b_rows[0] == a_rows[0] {
        removed_spans.push(RemovedSpan {
            item_id: id_b.to_string(),
            text: b_rows[0].to_string(),
            reason: "mergeTable:dup_header".into(),
        });
        b_rows.remove(0);
    }
    if b_rows.is_empty() {
        return Err(format!(
            "mergeTable 被拒：{id_b} 去掉重复表头后没有剩余行，应 drop"
        ));
    }

    // B 的行插在 A 末行的 </tr> 之后：A 外壳逐字节不动（出口行级保真闸依赖这一点）
    let last_row_end = last_tr_close_end(a_body).expect("a_rows 非空则必有 </tr>");
    let merged_body = format!(
        "{}{}{}",
        &a_body[..last_row_end],
        b_rows.concat(),
        &a_body[last_row_end..]
    );

    let mut merged = a.clone();
    merged.set("table_body", Value::String(merged_body));
    // caption/footnote 拼接（B 的字符不许丢：caption 在 C 比对内，footnote 虽不计也必须带走）
    for field in ["table_caption", "table_footnote"] {
        if let Some(bv) = b.0.get(field).and_then(Value::as_array)
            && !bv.is_empty()
        {
            let mut joined: Vec<Value> =
                a.0.get(field)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            joined.extend(bv.iter().cloned());
            merged.set(field, Value::Array(joined));
        }
    }
    union_bbox(&mut merged, a, b);
    Ok(OpOutcome {
        items: splice_merged(items, ia, ib, merged, between, next_id),
        removed_spans,
    })
}

// ── mergeList(idA, idB, joinSeam)：跨页被拆的两个 list 合成一个。list_items 拼接；
// joinSeam=true 时把 A 尾项与 B 首项缝成一项（跨页断句发生在列表项中间）。──
fn op_merge_list(
    items: &[RefItem],
    next_id: &IdGen,
    id_a: &str,
    id_b: &str,
    join_seam: bool,
) -> OpResult {
    let (ia, ib, between) = adjacent_pair("mergeList", items, id_a, id_b)?;
    let a = &items[ia].item;
    let b = &items[ib].item;
    if a.item_type() != "list" || b.item_type() != "list" {
        return Err(format!(
            "mergeList 仅限 list 块（实际 {} + {}）",
            a.item_type(),
            b.item_type()
        ));
    }
    let a_items = a.str_array("list_items").unwrap_or_default();
    let b_items = b.str_array("list_items").unwrap_or_default();
    if a_items.is_empty() || b_items.is_empty() {
        return Err("mergeList 的两块都必须有非空 list_items".into());
    }

    let merged_items: Vec<String> = if join_seam {
        let head = trim_end_ws(a_items[a_items.len() - 1]);
        let tail = trim_start_ws(b_items[0]);
        let seam = format!("{head}{}{tail}", glue_for(head, tail));
        a_items[..a_items.len() - 1]
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once(seam))
            .chain(b_items[1..].iter().map(|s| s.to_string()))
            .collect()
    } else {
        a_items
            .iter()
            .chain(b_items.iter())
            .map(|s| s.to_string())
            .collect()
    };

    let mut merged = a.clone();
    merged.set("list_items", json!(merged_items));
    union_bbox(&mut merged, a, b);
    Ok(OpOutcome {
        items: splice_merged(items, ia, ib, merged, between, next_id),
        removed_spans: vec![],
    })
}

// ── split(id, offset)：text 在字符 offset 处切两块。两子块继承父 bbox/page_idx（一期从简）。──
fn op_split(items: &[RefItem], next_id: &IdGen, id: &str, offset: i64) -> OpResult {
    let i = must_index_of_id(items, id)?;
    let it = &items[i].item;
    if it.item_type() != "text" || it.text().is_none() {
        return Err(format!("split 仅限 text 块（{id} 是 {}）", it.item_type()));
    }
    let text = it.text().unwrap();
    let char_len = text.chars().count() as i64;
    if offset <= 0 || offset >= char_len {
        return Err(format!(
            "split offset 越界：{offset}（text 长 {char_len}，须在开区间内）"
        ));
    }
    let byte_off = text
        .char_indices()
        .nth(offset as usize)
        .map(|(b, _)| b)
        .expect("offset 已验证在区间内");
    let head_text = trim_end_ws(&text[..byte_off]).to_string();
    let tail_text = trim_start_ws(&text[byte_off..]).to_string();
    if non_ws_len(&head_text) == 0 || non_ws_len(&tail_text) == 0 {
        return Err(format!(
            "split 产生空块：offset={offset} 切出的某一半无内容字符"
        ));
    }
    let mut head = it.clone();
    head.set("text", Value::String(head_text));
    let mut tail = it.clone();
    tail.set("text", Value::String(tail_text));
    tail.remove("text_level"); // 切出的后块默认正文；若实为小标题，由后续 promote 处理

    let mut out: Vec<RefItem> = Vec::with_capacity(items.len() + 1);
    out.extend_from_slice(&items[..i]);
    out.push(RefItem {
        id: next_id.next(),
        item: head,
    }); // split 产两个新 ID
    out.push(RefItem {
        id: next_id.next(),
        item: tail,
    });
    out.extend_from_slice(&items[i + 1..]);
    Ok(OpOutcome {
        items: out,
        removed_spans: vec![],
    })
}

// ── demote(id)：伪标题降为正文（清 text_level）。继承原 ID。──
fn op_demote(items: &[RefItem], id: &str) -> OpResult {
    let i = must_index_of_id(items, id)?;
    if items[i].item.text_level().is_none() {
        return Err(format!("demote：{id} 本就没有 text_level"));
    }
    let mut item = items[i].item.clone();
    item.remove("text_level");
    let mut out = items.to_vec();
    out[i] = RefItem {
        id: id.to_string(),
        item,
    };
    Ok(OpOutcome {
        items: out,
        removed_spans: vec![],
    })
}

// ── promote(id, level)：text 升为 header。继承原 ID。──
fn op_promote(items: &[RefItem], id: &str, level: i64) -> OpResult {
    let i = must_index_of_id(items, id)?;
    let it = &items[i].item;
    if it.item_type() != "text" || it.text().is_none() {
        return Err(format!(
            "promote 仅限 text 块（{id} 是 {}）",
            it.item_type()
        ));
    }
    if !(1..=6).contains(&level) {
        return Err(format!("promote level 非法：{level}"));
    }
    let mut item = it.clone();
    item.set("text_level", json!(level));
    let mut out = items.to_vec();
    out[i] = RefItem {
        id: id.to_string(),
        item,
    };
    Ok(OpOutcome {
        items: out,
        removed_spans: vec![],
    })
}

// ── reorder(idsInOrder)：仅允许对一个【连续区间】内的块重排（修跨页错序），各块 ID/bbox/page_idx 不变。──
fn op_reorder(items: &[RefItem], ids_in_order: &[String]) -> OpResult {
    if ids_in_order.len() < 2 {
        return Err("reorder 至少需要 2 个 ID".into());
    }
    if ids_in_order.iter().collect::<HashSet<_>>().len() != ids_in_order.len() {
        return Err("reorder ID 重复".into());
    }
    let mut indices: Vec<usize> = ids_in_order
        .iter()
        .map(|id| must_index_of_id(items, id))
        .collect::<Result<_, _>>()?;
    indices.sort_unstable();
    let lo = indices[0];
    let hi = indices[indices.len() - 1];
    if hi - lo != ids_in_order.len() - 1 {
        return Err(format!(
            "reorder 的 ID 必须构成连续区间（实际散布在 [{lo}, {hi}]）"
        ));
    }
    let mut out = items.to_vec();
    for (k, id) in ids_in_order.iter().enumerate() {
        out[lo + k] = items[must_index_of_id(items, id)?].clone();
    }
    Ok(OpOutcome {
        items: out,
        removed_spans: vec![],
    })
}

// ── drop(id)：删页码/页眉/页脚/水印/空壳表。白名单：type=page_number、
// 短 text/header（≤120 内容字符）、或零内容空壳表。──
const DROP_MAX_CHARS: usize = 120;

/// 零内容空壳表：无表格行、无 caption/footnote 字符、无图（MinerU 跨页合并后留下的占位）。
pub fn is_empty_table_husk(it: &MineruItem) -> bool {
    if it.item_type() != "table" {
        return false;
    }
    if let Some(body) = it.table_body()
        && !table_rows(body).is_empty()
    {
        return false;
    }
    if let Some(p) = it.img_path()
        && !p.is_empty()
    {
        return false;
    }
    for field in ["table_caption", "table_footnote"] {
        if let Some(v) = it.str_array(field)
            && v.iter().any(|s| non_ws_len(s) > 0)
        {
            return false;
        }
    }
    true
}

fn op_drop(items: &[RefItem], id: &str, droppable: Option<&HashSet<String>>) -> OpResult {
    let i = must_index_of_id(items, id)?;
    let it = &items[i].item;
    let is_page_number = it.item_type() == "page_number";
    let is_short_text = (it.item_type() == "text" || it.item_type() == "header")
        && it
            .text()
            .map(|t| non_ws_len(t) <= DROP_MAX_CHARS)
            .unwrap_or(false);
    let is_husk = is_empty_table_husk(it);
    if !is_page_number && !is_short_text && !is_husk {
        return Err(format!(
            "drop 白名单不命中：{id}（type={}）只允许删页码、≤{DROP_MAX_CHARS} 字的短文本或零内容空壳表",
            it.item_type()
        ));
    }
    if let Some(allowed) = droppable
        && !allowed.contains(id)
    {
        return Err(format!(
            "drop 被拒：{id} 未被探测器标记为 page_artifact/empty_table 疑点"
        ));
    }
    let removed = match it.text() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => format!("[{}]", it.item_type()),
    };
    let mut out = items.to_vec();
    out.remove(i);
    Ok(OpOutcome {
        items: out,
        removed_spans: vec![RemovedSpan {
            item_id: id.to_string(),
            text: removed,
            reason: "drop".into(),
        }],
    })
}

// ── strip(id, pattern)：去残留符号，pattern 仅限白名单（不收任意 regex）。继承原 ID。──
// 把公式体里的 LaTeX 命令残骸剥成内容字符：\mathsf { A i j } { = } 1 → A i j = 1。
// 只删不增（命令名/花括号被移除，内容字符与空白保留），C_out ⊆ C_in 天然成立。
static LATEX_CMD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[a-zA-Z]+").unwrap());
static BRACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[{}]").unwrap());
static MULTI_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn strip_latex_commands(body: &str) -> String {
    let s = LATEX_CMD.replace_all(body, " ");
    let s = BRACES.replace_all(&s, " ");
    MULTI_WS.replace_all(&s, " ").trim().to_string()
}

static STRIP_MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\(([^)]*)\)").unwrap());
static STRIP_LATEX_DOLLAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$([^$\n]+)\$").unwrap());
static STRIP_LATEX_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$[^$\n]+\$").unwrap());
static STRIP_LATEX_COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\[a-zA-Z]+|[{}]").unwrap());
static STRIP_ESCAPED_DOLLAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\\$").unwrap());
static STRIP_HTML_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)</?(?:br|hr|b|i|u|s|em|strong|sub|sup|span|div|p|a|img|font|center|small|big|del|ins|mark|code|pre|table|tbody|thead|tr|td|th)(?:\s[^<>]*)?/?>",
    )
    .unwrap()
});

fn strip_spec(pattern: StripPattern) -> (&'static Regex, fn(&regex::Captures) -> String) {
    match pattern {
        // [t](url) → t
        StripPattern::MdLink => (&STRIP_MD_LINK, |m| {
            m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default()
        }),
        // $\mathsf{x}$ → x（去定界符+命令残骸）
        StripPattern::LatexDollar => (&STRIP_LATEX_DOLLAR, |m| {
            strip_latex_commands(m.get(1).map(|g| g.as_str()).unwrap_or(""))
        }),
        // 整段公式删除（内容进 removedSpans 审计）
        StripPattern::LatexBlock => (&STRIP_LATEX_BLOCK, |_| String::new()),
        // 无 $ 定界符的裸命令残骸（latex_dollar 旧版只去定界符留下的，或 MinerU 直接吐出的）
        StripPattern::LatexCommand => (&STRIP_LATEX_COMMAND, |_| String::new()),
        // \$APPEALS → $APPEALS（去转义反斜杠）
        StripPattern::EscapedDollar => (&STRIP_ESCAPED_DOLLAR, |_| "$".to_string()),
        // 只删已知 HTML 标签名：宽泛匹配会误删正文里的「<表单编号 …>」类引用（真实数据踩过）
        StripPattern::HtmlTag => (&STRIP_HTML_TAG, |_| String::new()),
    }
}

fn op_strip(items: &[RefItem], id: &str, pattern: StripPattern) -> OpResult {
    let i = must_index_of_id(items, id)?;
    let it = &items[i].item;
    let Some(text) = it.text() else {
        return Err(format!("strip：{id} 没有 text 字段"));
    };

    let (re, keep) = strip_spec(pattern);
    let mut removed_spans: Vec<RemovedSpan> = Vec::new();
    let mut new_text = String::with_capacity(text.len());
    let mut last = 0usize;
    for caps in re.captures_iter(text) {
        let m = caps.get(0).unwrap();
        new_text.push_str(&text[last..m.start()]);
        new_text.push_str(&keep(&caps));
        removed_spans.push(RemovedSpan {
            item_id: id.to_string(),
            text: m.as_str().to_string(),
            reason: format!(
                "strip:{}",
                serde_json::to_value(pattern).unwrap().as_str().unwrap()
            ),
        });
        last = m.end();
    }
    new_text.push_str(&text[last..]);

    if removed_spans.is_empty() {
        return Err(format!(
            "strip：{id} 中未匹配到 pattern {pattern:?}，拒绝空操作"
        ));
    }
    if non_ws_len(&new_text) == 0 {
        return Err(format!("strip 会把 {id} 掏空，应改用 drop"));
    }

    let mut item = it.clone();
    item.set("text", Value::String(new_text));
    let mut out = items.to_vec();
    out[i] = RefItem {
        id: id.to_string(),
        item,
    };
    Ok(OpOutcome {
        items: out,
        removed_spans,
    })
}

// ── 调度 + 保真闸 ──

pub struct ApplyContext<'a> {
    pub next_id: &'a IdGen,
    /// 探测器当前标记为 page_artifact/empty_table 的 id 集；提供时 drop 必须命中（双保险）。
    pub droppable_ids: Option<&'a HashSet<String>>,
    /// 输入文档的页集合（几何校验基准）。
    pub valid_pages: &'a HashSet<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectKind {
    InvalidArgs,
    FidelityViolation,
}

pub enum ApplyResult {
    Ok {
        items: Vec<RefItem>,
        removed_spans: Vec<RemovedSpan>,
        new_ids: Vec<String>,
    },
    Rejected {
        reason: String,
        kind: RejectKind,
    },
}

/// 执行单个 op：参数非法 → InvalidArgs；保真闸不过 → FidelityViolation（回滚，原 items 不动）。
pub fn apply_op_checked(items: &[RefItem], call: &OpCall, ctx: &ApplyContext) -> ApplyResult {
    let before_ids: HashSet<&str> = items.iter().map(|r| r.id.as_str()).collect();
    let outcome = match call {
        OpCall::Merge { id_a, id_b } => op_merge(items, ctx.next_id, id_a, id_b),
        OpCall::Split { id, offset } => op_split(items, ctx.next_id, id, *offset),
        OpCall::Demote { id } => op_demote(items, id),
        OpCall::Promote { id, level } => op_promote(items, id, *level),
        OpCall::Reorder { ids_in_order } => op_reorder(items, ids_in_order),
        OpCall::Drop { id } => op_drop(items, id, ctx.droppable_ids),
        OpCall::Strip { id, pattern } => op_strip(items, id, *pattern),
        OpCall::MergeTable { id_a, id_b } => op_merge_table(items, ctx.next_id, id_a, id_b),
        OpCall::MergeList {
            id_a,
            id_b,
            join_seam,
        } => op_merge_list(items, ctx.next_id, id_a, id_b, join_seam.unwrap_or(false)),
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(reason) => {
            return ApplyResult::Rejected {
                reason,
                kind: RejectKind::InvalidArgs,
            };
        }
    };

    // 保真闸：drop/strip 是有意削减，字符子集天然成立；闸门防的是 op 实现 bug 与几何破坏。
    if let Err(reason) = check_fidelity(items, &outcome.items, Some(ctx.valid_pages)) {
        return ApplyResult::Rejected {
            reason,
            kind: RejectKind::FidelityViolation,
        };
    }
    let new_ids = outcome
        .items
        .iter()
        .filter(|r| !before_ids.contains(r.id.as_str()))
        .map(|r| r.id.clone())
        .collect();
    ApplyResult::Ok {
        items: outcome.items,
        removed_spans: outcome.removed_spans,
        new_ids,
    }
}
