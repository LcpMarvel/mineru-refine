// 机械清洗 pass：在 LLM loop 之前运行的确定性清理，只做无歧义的削减/重组，不打 LLM。
//
// 清理项：
//   1. 表格全空行删除（含 rowspan>1 的表只删尾部空行，避免错动 rowspan 覆盖关系）
//   2. 表格续行合并（跨页把一条记录的长 cell 切成了多个 <tr>：等列数 + 恰一个非空 cell
//      + 上一行同列 cell 未以句末标点收尾 → 并回上一行）
//   3. cell 内空白收紧（≥2 连续空白或含全角空格：两侧都是 CJK → 删除，否则收为单个空格）
//   4. URL 内 OCR 空格删除（`https://www. x. com` → 空格紧跟 [./?=&] 且后随 ASCII 字母数字）
//   5. markdown 转义残留删除（`\$APPEALS` → `$APPEALS`，`\*` → `*`；text/caption/list_items 同样处理）
//
// 保真：本 pass 自带逐项校验，与 op 体系的保真闸互不依赖——
//   - 字符串字段：删除的字符只能是空白或转义反斜杠，绝不新增字符
//   - 表格：shell（行外字节）逐字节不变，全体 cell 内容字符多重集不增、删除的只能是空白/反斜杠
// 任一校验不过 → 放弃该项变更、保留原值并记 mechReverted（防实现 bug，正常永不触发）。
// 在 refine_inner 中先于基线快照执行，出口闸门以"机械清洗后的 items"为基准。

use crate::agent_loop::Logger;
use crate::invariant::{is_js_whitespace, table_shell};
use crate::types::{RefItem, RemovedSpan};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

static TR_ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<tr[\s\S]*?</tr>").unwrap());
static CELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)(<t[dh][^>]*>)([\s\S]*?)(</t[dh]>)").unwrap());
static ROWSPAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)rowspan\s*=\s*"?([0-9]+)"#).unwrap());
static ESCAPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\([$*])").unwrap());
// 句末标点（含分号）：上一行 cell 以它收尾说明记录完整，不做续行合并
static CELL_SENTENCE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[。．.!！?？;；…]\s*$").unwrap());

pub struct MechOutcome {
    pub counts: BTreeMap<String, u64>,
    pub removed_spans: Vec<RemovedSpan>,
}

// ── 字符串清理件 ──

/// `\$` / `\*` → `$` / `*`。返回 (新串, 命中的转义串列表)。
fn unescape(s: &str) -> (String, Vec<String>) {
    let mut hits: Vec<String> = Vec::new();
    let out = ESCAPED
        .replace_all(s, |c: &regex::Captures| {
            hits.push(c[0].to_string());
            c[1].to_string()
        })
        .into_owned();
    (out, hits)
}

fn at_url_start(chars: &[char], i: usize) -> bool {
    let probe = |pat: &str| {
        chars[i..]
            .iter()
            .take(pat.chars().count())
            .collect::<String>()
            == pat
    };
    chars[i..].len() >= 7 && (probe("http://") || probe("https://"))
}

/// URL 内的 OCR 空格：空白段紧跟在 [./?=&] 之后且后随 ASCII 字母数字时删除。
fn fix_url_ws(s: &str) -> (String, u64) {
    if !s.contains("http") {
        return (s.to_string(), 0);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut fixes = 0u64;
    let mut i = 0;
    while i < chars.len() {
        if at_url_start(&chars, i) {
            let mut last = '\0';
            while i < chars.len() {
                let c = chars[i];
                if is_js_whitespace(c) {
                    let mut j = i;
                    while j < chars.len() && is_js_whitespace(chars[j]) {
                        j += 1;
                    }
                    if matches!(last, '.' | '/' | '?' | '=' | '&')
                        && chars
                            .get(j)
                            .map(|c| c.is_ascii_alphanumeric())
                            .unwrap_or(false)
                    {
                        fixes += 1;
                        i = j; // 删掉这段空白，URL 继续
                        continue;
                    }
                    break; // URL 结束，空白留给外层原样输出
                }
                out.push(c);
                last = c;
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, fixes)
}

/// CJK 及全角标点（空白收紧时两侧是它们就直接删空白）。
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}'   // 汉字
        | '\u{3001}'..='\u{303f}' // CJK 标点（、。《》【】…）
        | '\u{ff00}'..='\u{ffef}' // 全角形式（（）：；，等）
        | '\u{2018}'..='\u{201d}' // 弯引号
    )
}

/// cell 内空白收紧：≥2 连续空白或含全角空格的空白段——两侧都是 CJK → 删除，
/// 否则收为单个空格；cell 首尾空白段直接删除。单个半角空白原样保留。
fn tighten_cell_ws(s: &str) -> (String, u64) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut hits = 0u64;
    let mut i = 0;
    while i < chars.len() {
        if is_js_whitespace(chars[i]) {
            let start = i;
            let mut has_wide = false;
            while i < chars.len() && is_js_whitespace(chars[i]) {
                if chars[i] == '\u{3000}' {
                    has_wide = true;
                }
                i += 1;
            }
            if i - start >= 2 || has_wide {
                hits += 1;
                let prev = out.chars().last();
                let next = chars.get(i).copied();
                match (prev, next) {
                    (None, _) | (_, None) => {}                        // 首尾：删
                    (Some(p), Some(n)) if is_cjk(p) && is_cjk(n) => {} // CJK 之间：删
                    _ => out.push(' '),
                }
            } else {
                out.push(chars[start]);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    (out, hits)
}

// ── 校验件 ──

fn char_counts(s: &str) -> HashMap<char, i64> {
    let mut m = HashMap::new();
    for c in s.chars() {
        *m.entry(c).or_insert(0) += 1;
    }
    m
}

/// 变更合法性：与全局保真闸同口径——空白在白名单内（续行合并的英文 glue 空格、
/// 空白收紧都不计），内容字符不得新增；被删除的内容字符只能是转义反斜杠 `\`。
fn diff_ok(old: &str, new: &str) -> bool {
    let non_ws = |s: &str| -> String { s.chars().filter(|c| !is_js_whitespace(*c)).collect() };
    let co = char_counts(&non_ws(old));
    let cn = char_counts(&non_ws(new));
    for (ch, n) in &cn {
        if *n > co.get(ch).copied().unwrap_or(0) {
            return false; // 新增内容字符
        }
    }
    for (ch, n) in &co {
        if *n > cn.get(ch).copied().unwrap_or(0) && *ch != '\\' {
            return false; // 删掉了内容字符
        }
    }
    true
}

// ── 表格处理 ──

struct ParsedRow {
    /// cell 之间/外的字节段，len == cells.len() + 1（含 <tr> 开闭标签）
    skeleton: Vec<String>,
    /// (open_tag, inner, close_tag)
    cells: Vec<(String, String, String)>,
}

fn parse_row(row: &str) -> ParsedRow {
    let mut skeleton: Vec<String> = Vec::new();
    let mut cells: Vec<(String, String, String)> = Vec::new();
    let mut last = 0usize;
    for caps in CELL.captures_iter(row) {
        let m = caps.get(0).unwrap();
        skeleton.push(row[last..m.start()].to_string());
        cells.push((
            caps[1].to_string(),
            caps[2].to_string(),
            caps[3].to_string(),
        ));
        last = m.end();
    }
    skeleton.push(row[last..].to_string());
    ParsedRow { skeleton, cells }
}

fn rebuild_row(p: &ParsedRow) -> String {
    let mut out = String::new();
    for (i, seg) in p.skeleton.iter().enumerate() {
        out.push_str(seg);
        if let Some((open, inner, close)) = p.cells.get(i) {
            out.push_str(open);
            out.push_str(inner);
            out.push_str(close);
        }
    }
    out
}

fn has_rowspan_gt1(s: &str) -> bool {
    ROWSPAN
        .captures_iter(s)
        .any(|c| c[1].parse::<u64>().map(|v| v > 1).unwrap_or(false))
}

/// 英文断词处补空格（与 ops::glue_for 同逻辑），中文直接拼。
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

struct TableStats {
    row_merges: u64,
    empty_rows: u64,
    cell_ws: u64,
    url_ws: u64,
    unescapes: Vec<String>,
}

/// 表格清理：续行合并 → 空行删除 → cell 内容清理。返回 None = 无变更。
fn clean_table_body(body: &str) -> Option<(String, TableStats)> {
    // 切分为 gap/row 交替段（gap 全保留 → shell 逐字节不变）
    let mut gaps: Vec<&str> = Vec::new();
    let mut rows: Vec<ParsedRow> = Vec::new();
    let mut last = 0usize;
    for m in TR_ROW.find_iter(body) {
        gaps.push(&body[last..m.start()]);
        rows.push(parse_row(m.as_str()));
        last = m.end();
    }
    gaps.push(&body[last..]);
    if rows.is_empty() {
        return None;
    }

    let mut stats = TableStats {
        row_merges: 0,
        empty_rows: 0,
        cell_ws: 0,
        url_ws: 0,
        unescapes: Vec::new(),
    };
    let table_has_rowspan = has_rowspan_gt1(body);
    let mut removed = vec![false; rows.len()];

    // 1) 续行合并：等列数（≥2）+ 恰一个非空 cell 且必须是【末列】+ 上一行同列 cell
    //    内容较长（≥20 内容字，溢出只发生在装满的 cell）且未以句末标点收尾。
    //    末列 + 长度门槛挡住"标签列 + 空列"的模板表（真实数据踩过：SWOT 空表的
    //    S/W/O/T 标签行恰好满足"恰一个非空 cell"）。
    //    rowspan>1 的表整体跳过（rowspan 行的"少列"与续行难以区分，宁可不动）。
    if !table_has_rowspan {
        let mut prev_alive: Option<usize> = None;
        for r in 0..rows.len() {
            let merged = if let Some(p) = prev_alive {
                let (cur_len, prev_len) = (rows[r].cells.len(), rows[p].cells.len());
                let non_empty: Vec<usize> = rows[r]
                    .cells
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !c.1.trim().is_empty())
                    .map(|(k, _)| k)
                    .collect();
                if cur_len == prev_len
                    && cur_len >= 2
                    && non_empty.len() == 1
                    && non_empty[0] == cur_len - 1
                {
                    let k = non_empty[0];
                    let prev_inner = rows[p].cells[k].1.trim_end().to_string();
                    if crate::invariant::non_ws_len(&prev_inner) >= 20
                        && !CELL_SENTENCE_END.is_match(&prev_inner)
                    {
                        let tail = rows[r].cells[k].1.trim_start().to_string();
                        let glue = glue_for(&prev_inner, &tail);
                        rows[p].cells[k].1 = format!("{prev_inner}{glue}{tail}");
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if merged {
                removed[r] = true;
                stats.row_merges += 1;
            } else {
                prev_alive = Some(r);
            }
        }
    }

    // 2) 空行删除：全 cell 空（无 cell 也算空）。表内有 rowspan>1 时只删尾部空行。
    let is_empty_row = |p: &ParsedRow| -> bool { p.cells.iter().all(|c| c.1.trim().is_empty()) };
    if table_has_rowspan {
        for r in (0..rows.len()).rev() {
            if removed[r] {
                continue;
            }
            if is_empty_row(&rows[r]) {
                removed[r] = true;
                stats.empty_rows += 1;
            } else {
                break;
            }
        }
    } else {
        for r in 0..rows.len() {
            if !removed[r] && is_empty_row(&rows[r]) {
                removed[r] = true;
                stats.empty_rows += 1;
            }
        }
    }

    // 3) cell 内容清理：转义残留 → URL 空格 → 空白收紧
    for (r, row) in rows.iter_mut().enumerate() {
        if removed[r] {
            continue;
        }
        for cell in &mut row.cells {
            let (s, hits) = unescape(&cell.1);
            stats.unescapes.extend(hits);
            let (s, url_fixes) = fix_url_ws(&s);
            stats.url_ws += url_fixes;
            let (s, ws_hits) = tighten_cell_ws(&s);
            stats.cell_ws += ws_hits;
            cell.1 = s;
        }
    }

    if stats.row_merges == 0
        && stats.empty_rows == 0
        && stats.cell_ws == 0
        && stats.url_ws == 0
        && stats.unescapes.is_empty()
    {
        return None;
    }

    // 重建：gap 全保留（shell 不变），removed 行只少行字节
    let mut out = String::with_capacity(body.len());
    for (r, gap) in gaps.iter().enumerate() {
        out.push_str(gap);
        if r < rows.len() && !removed[r] {
            out.push_str(&rebuild_row(&rows[r]));
        }
    }
    Some((out, stats))
}

/// 表格变更校验：shell 逐字节不变 + 全体 cell 内容字符多重集合法（不新增、只删空白/反斜杠）。
fn verify_table(old: &str, new: &str) -> bool {
    if table_shell(old) != table_shell(new) {
        return false;
    }
    let inners = |body: &str| -> String {
        CELL.captures_iter(body)
            .map(|c| c[2].to_string())
            .collect::<Vec<_>>()
            .concat()
    };
    diff_ok(&inners(old), &inners(new))
}

// ── 入口 ──

pub fn mechanical_clean(items: &mut [RefItem], log: &Logger) -> MechOutcome {
    fn bump(counts: &mut BTreeMap<String, u64>, key: &str, n: u64) {
        if n > 0 {
            *counts.entry(key.to_string()).or_insert(0) += n;
        }
    }
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut removed_spans: Vec<RemovedSpan> = Vec::new();

    for r in items.iter_mut() {
        let id = r.id.clone();

        // 字符串字段：text + list_items/table_caption/table_footnote
        let clean_str = |s: &str,
                         counts: &mut BTreeMap<String, u64>,
                         spans: &mut Vec<RemovedSpan>|
         -> Option<String> {
            let (t, escapes) = unescape(s);
            let (t, url_fixes) = fix_url_ws(&t);
            if escapes.is_empty() && url_fixes == 0 {
                return None;
            }
            if !diff_ok(s, &t) {
                log(&format!("机械清洗校验不过（{id} 字符串字段），保留原值"));
                bump(counts, "mechReverted", 1);
                return None;
            }
            bump(counts, "mechUnescape", escapes.len() as u64);
            bump(counts, "mechUrlWs", url_fixes);
            for e in escapes {
                spans.push(RemovedSpan {
                    item_id: id.clone(),
                    text: e,
                    reason: "mech:unescape".into(),
                });
            }
            Some(t)
        };

        if let Some(text) = r.item.text()
            && let Some(t) = clean_str(text, &mut counts, &mut removed_spans)
        {
            r.item.set("text", Value::String(t));
        }
        for field in ["list_items", "table_caption", "table_footnote"] {
            let Some(arr) = r.item.0.get(field).and_then(Value::as_array).cloned() else {
                continue;
            };
            let mut changed = false;
            let new_arr: Vec<Value> = arr
                .into_iter()
                .map(|v| match v.as_str() {
                    Some(s) => match clean_str(s, &mut counts, &mut removed_spans) {
                        Some(t) => {
                            changed = true;
                            Value::String(t)
                        }
                        None => v,
                    },
                    None => v,
                })
                .collect();
            if changed {
                r.item.set(field, Value::Array(new_arr));
            }
        }

        // 表格
        if let Some(body) = r.item.table_body().map(str::to_string)
            && let Some((new_body, stats)) = clean_table_body(&body)
        {
            if !verify_table(&body, &new_body) {
                log(&format!("机械清洗校验不过（{id} table_body），保留原值"));
                bump(&mut counts, "mechReverted", 1);
                continue;
            }
            bump(&mut counts, "mechRowMerge", stats.row_merges);
            bump(&mut counts, "mechEmptyRow", stats.empty_rows);
            bump(&mut counts, "mechCellWs", stats.cell_ws);
            bump(&mut counts, "mechUrlWs", stats.url_ws);
            bump(&mut counts, "mechUnescape", stats.unescapes.len() as u64);
            for e in stats.unescapes {
                removed_spans.push(RemovedSpan {
                    item_id: id.clone(),
                    text: e,
                    reason: "mech:unescape".into(),
                });
            }
            r.item.set("table_body", Value::String(new_body));
        }
    }

    if !counts.is_empty() {
        let summary = counts
            .iter()
            .map(|(k, v)| format!("{k}×{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        log(&format!("机械清洗: {summary}"));
    }
    MechOutcome {
        counts,
        removed_spans,
    }
}
