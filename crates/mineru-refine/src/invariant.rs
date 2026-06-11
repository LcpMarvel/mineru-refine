// 保真不变式：C_out ⊆ C_in（非空白内容字符多重集），
// table_body 逐字节相等（多重集包含；mergeTable 产物降级为行级逐字节），几何可定位。
// 每个 op 后调用一次（违反→回滚），出口处对整篇再调用一次（违反→fail-open）。

use crate::types::{MineruItem, RefItem};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// JS 正则 `\s` 的语义（TS 版的保真白名单按它定义）：Unicode 空白 + U+FEFF（BOM），
/// 但不含 U+0085（NEL，Rust is_whitespace 含它而 JS \s 不含）。
pub fn is_js_whitespace(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{0085}' && c.is_whitespace())
}

/// 去掉全部 JS \s 空白（TS nonWs 的等价物）。
pub fn non_ws(s: &str) -> String {
    s.chars().filter(|c| !is_js_whitespace(*c)).collect()
}

pub fn non_ws_len(s: &str) -> usize {
    s.chars().filter(|c| !is_js_whitespace(*c)).count()
}

/// "内容字符" = text + list_items 拼接 + table_caption 拼接，仅计非空白字符。
pub fn content_chars(items: &[&MineruItem]) -> HashMap<char, u64> {
    let mut counts: HashMap<char, u64> = HashMap::new();
    for item in items {
        let mut count_str = |s: &str| {
            for ch in s.chars() {
                if is_js_whitespace(ch) {
                    continue; // 空白符在可削减白名单内，不计
                }
                *counts.entry(ch).or_insert(0) += 1;
            }
        };
        if let Some(t) = item.text() {
            count_str(t);
        }
        for key in ["list_items", "table_caption"] {
            if let Some(parts) = item.str_array(key) {
                for p in parts {
                    count_str(p);
                }
            }
        }
    }
    counts
}

pub type FidelityResult = Result<(), String>;

fn items_of(entries: &[RefItem]) -> Vec<&MineruItem> {
    entries.iter().map(|r| &r.item).collect()
}

/// C_out ⊆ C_in：输出不得包含任何输入里没有的非空白内容字符。
pub fn check_char_subset(before: &[&MineruItem], after: &[&MineruItem]) -> FidelityResult {
    let cin = content_chars(before);
    let cout = content_chars(after);
    for (ch, n) in &cout {
        let avail = cin.get(ch).copied().unwrap_or(0);
        if *n > avail {
            return Err(format!(
                "C_out ⊄ C_in：字符 {} 输出 {} 次 > 输入 {} 次",
                serde_json::to_string(&ch.to_string()).unwrap_or_default(),
                n,
                avail
            ));
        }
    }
    Ok(())
}

static TR_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<tr[\s\S]*?</tr>").expect("TR_ROW 正则非法"));

/// table_body 的 `<tr>…</tr>` 行序列（MinerU 表格不嵌套，非贪婪匹配安全）。
pub fn table_rows(body: &str) -> Vec<&str> {
    TR_ROW.find_iter(body).map(|m| m.as_str()).collect()
}

/// table_body 去掉所有行后的"外壳"（`<table>`/`<tbody>` 包装等行外字节）。
pub fn table_shell(body: &str) -> String {
    TR_ROW.replace_all(body, "").into_owned()
}

fn take_from_pool(pool: &mut HashMap<String, u64>, key: &str) -> bool {
    match pool.get_mut(key) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

fn char_prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 未被 drop 的 table_body 逐字节相等（多重集 ⊆）；唯一例外是 mergeTable 产物——
/// 它必须能被行级证明：每个 `<tr>` 行逐字节来自输入行池、行外"外壳"逐字节命中某个输入外壳
/// （即除"把若干输入行按原字节拼进某个输入表"外，没有任何字节被改动）。
pub fn check_table_bodies(before: &[&MineruItem], after: &[&MineruItem]) -> FidelityResult {
    let bodies = |entries: &[&MineruItem]| -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| e.table_body().map(str::to_string))
            .collect()
    };

    // 第一遍：整表逐字节撮合，被命中的输入表视为已消费
    let mut input_pool: HashMap<String, u64> = HashMap::new();
    for b in bodies(before) {
        *input_pool.entry(b).or_insert(0) += 1;
    }
    let mut unmatched: Vec<String> = Vec::new();
    for b in bodies(after) {
        if !take_from_pool(&mut input_pool, &b) {
            unmatched.push(b);
        }
    }
    if unmatched.is_empty() {
        return Ok(());
    }

    // 第二遍（mergeTable 产物）：行/外壳池只从【未被消费】的输入表构建——
    // 防止同一输入行被"整表命中"和"行级命中"双重消费
    let mut row_pool: HashMap<String, u64> = HashMap::new();
    let mut shell_pool: HashMap<String, u64> = HashMap::new();
    for (body, n) in &input_pool {
        for _ in 0..*n {
            for row in table_rows(body) {
                *row_pool.entry(row.to_string()).or_insert(0) += 1;
            }
            *shell_pool.entry(table_shell(body)).or_insert(0) += 1;
        }
    }
    for body in &unmatched {
        if !take_from_pool(&mut shell_pool, &table_shell(body)) {
            return Err(format!(
                "table_body 被篡改：行外字节与所有输入表外壳都不符（前 80 字: {}）",
                char_prefix(body, 80)
            ));
        }
        for row in table_rows(body) {
            if !take_from_pool(&mut row_pool, row) {
                return Err(format!(
                    "table_body 被篡改：输出中存在输入里没有的表格行（前 80 字: {}）",
                    char_prefix(row, 80)
                ));
            }
        }
    }
    Ok(())
}

/// 几何可定位（软检查的硬化版）：bbox 为 4 个有限数、page_idx 落在输入页集合内。
pub fn check_geometry(after: &[RefItem], valid_pages: &HashSet<i64>) -> FidelityResult {
    for r in after {
        if r.item.bbox().is_none() {
            return Err(format!(
                "几何失效：{} 的 bbox 非法 ({})",
                r.id,
                r.item
                    .0
                    .get("bbox")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "undefined".into())
            ));
        }
        match r.item.page_idx() {
            Some(p) if valid_pages.contains(&p) => {}
            other => {
                return Err(format!(
                    "几何失效：{} 的 page_idx={} 不在输入页范围内",
                    r.id,
                    other
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "undefined".into())
                ));
            }
        }
    }
    Ok(())
}

pub fn input_pages(items: &[&MineruItem]) -> HashSet<i64> {
    items.iter().filter_map(|i| i.page_idx()).collect()
}

fn has_valid_geometry(item: &MineruItem) -> bool {
    item.bbox().is_some() && item.page_idx().is_some()
}

/// 完整保真闸门：字符子集 + table_body + 几何，任一不过即 fail。
/// 几何检查仅在输入本身全量带几何信息时执行——某些 MinerU 版本的 content_list
/// 不含 bbox，此时强检几何会把所有 op 误判回滚。
pub fn check_fidelity(
    before: &[RefItem],
    after: &[RefItem],
    valid_pages: Option<&HashSet<i64>>,
) -> FidelityResult {
    let b = items_of(before);
    let a = items_of(after);
    check_char_subset(&b, &a)?;
    check_table_bodies(&b, &a)?;
    if before.iter().all(|r| has_valid_geometry(&r.item)) {
        let fallback;
        let pages = match valid_pages {
            Some(p) => p,
            None => {
                fallback = input_pages(&b);
                &fallback
            }
        };
        check_geometry(after, pages)?;
    }
    Ok(())
}
