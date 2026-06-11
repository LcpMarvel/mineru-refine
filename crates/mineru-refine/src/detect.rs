// 异常探测器：确定性启发式 → worklist。产出"疑点"非"结论"，
// 由 LLM 在 loop 中裁决。判定逻辑借鉴 docfuse _SANITIZE_PASSES 的思想原型。

use crate::invariant::{non_ws, non_ws_len, table_rows};
use crate::ops::is_empty_table_husk;
use crate::types::{MineruItem, RefItem, SuspectKind, WorkItem, is_page_furniture};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        pub(crate) static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($pat).expect(concat!(stringify!($name), " 正则非法")));
    };
}

re!(SENTENCE_END, r"[。．.!！?？;；…]\s*$");
// 列表项特征开头：-、•、①…⑳、(1)等。列表行天然无句末标点，跨页相邻也绝不能 merge（会把多个列表项粘成一行）
re!(BULLET_START, r"^\s*[-–—•·●○◦▪‣*①-⑳]");
// 标题/编号特征开头：第X章/节/条、1. / 1.2 / (一) / 一、 等
re!(
    HEADING_START,
    r"^\s*(第\s*[0-9一二三四五六七八九十百]+\s*[章节条款部分篇]|[0-9]+(\.[0-9]+)*[.、\s]|[(（][0-9一二三四五六七八九十]+[)）]|[一二三四五六七八九十]+[、.])"
);
re!(
    LEADING_NUMBERING,
    r"^\s*(第\s*[0-9一二三四五六七八九十百]+\s*[章节条款部分篇]\s*|[0-9]+(\.[0-9]+)*[.、\s]+|[(（][0-9一二三四五六七八九十]+[)）]\s*|[一二三四五六七八九十]+[、.]\s*)"
);

const GIANT_BLOCK_CHARS: usize = 1200;
const ARTIFACT_MAX_CHARS: usize = 30;
const ARTIFACT_MIN_REPEATS: usize = 3;

re!(MD_LINK, r"\[[^\]]*\]\([^)]*\)");
// `\\[a-zA-Z]+\s*\{` 兜住 \mathsf { … } 这类去掉 $ 定界符后的命令残骸（真实数据踩过：
// strip:latex_dollar 只去定界符时，残骸不再命中关键词列表，形成探测盲区）
re!(
    LATEX,
    r"\$[^$\n]+\$|\\(frac|sqrt|begin|end|cdot|times|alpha|beta|lambda|sum|mathsf|mathrm|mathbf|mathit|geqslant|leqslant|operatorname)\b|\\[a-zA-Z]+\s*\{"
);
// 孤立的 \$ 转义（如「\$APPEALS」工具名）：成对 $ 的 LATEX 规则不命中，需单独探测
re!(ESCAPED_DOLLAR, r"\\\$");
// 只认已知 HTML 标签名：宽泛的 /<[^>]+>/ 会把正文里的「<MB-ZZ-155 部门OGSMT>」这类表单引用误判成标签（真实数据踩过）
re!(
    HTML_TAG,
    r"(?i)</?(?:br|hr|b|i|u|s|em|strong|sub|sup|span|div|p|a|img|font|center|small|big|del|ins|mark|code|pre|table|tbody|thead|tr|td|th)(?:\s[^<>]*)?/?>"
);
re!(
    GIANT_HEADING_HIT,
    r"(?:^|\n)\s*(?:[0-9]+(?:\.[0-9]+)*[.、\s]|第\s*[0-9一二三四五六七八九十百]+\s*[章节条款])"
);
re!(COMMA_SEMI, r"[，,；;]");
re!(
    PAGE_NUMBER_CHARS,
    r"^[\s\d\-–—一二三四五六七八九十之/\\.()（）页第共]+$"
);

fn suspect(kind: SuspectKind, item_id: &str, evidence: String, has_op: bool) -> WorkItem {
    WorkItem {
        kind,
        item_id: item_id.to_string(),
        evidence,
        has_op,
    }
}

fn char_prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn char_suffix(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

pub fn detect(items: &[RefItem]) -> Vec<WorkItem> {
    let mut out: Vec<WorkItem> = Vec::new();

    // 高频重复短文本（页眉/页脚/水印候选）：同文本出现在 ≥3 个不同页。
    // 注意：MinerU 的 type=header/footer/page_number 是【已正确分类】的页面家具，不是 quirk
    // （探测器针对的是被误标为 text 的混入正文），那些类型一律不进 worklist。
    let mut text_pages: HashMap<String, HashSet<i64>> = HashMap::new();
    for r in items {
        let item = &r.item;
        if item.item_type() == "text"
            && let Some(text) = item.text()
        {
            let key = text.trim();
            if !key.is_empty() && non_ws_len(key) <= ARTIFACT_MAX_CHARS {
                let entry = text_pages.entry(key.to_string()).or_default();
                if let Some(p) = item.page_idx() {
                    entry.insert(p);
                }
            }
        }
    }
    let repeated_texts: HashSet<&str> = text_pages
        .iter()
        .filter(|(_, pages)| pages.len() >= ARTIFACT_MIN_REPEATS)
        .map(|(t, _)| t.as_str())
        .collect();

    // 家具同文佐证：已被 MinerU 正确分类的 header/footer/page_number 文本（≥2 处佐证）。
    // 同文却被标成 type=text 的就是漏网的跑马灯页眉/页脚——这类泄漏往往只出现 1~2 页
    // （其余页被正确分类），到不了 ARTIFACT_MIN_REPEATS 阈值，必须靠家具佐证抓
    // （真实数据：附件标题 8 处、公司名 3 处全因此漏网）。
    let mut furniture_counts: HashMap<String, usize> = HashMap::new();
    for r in items {
        let item = &r.item;
        if is_page_furniture(item.item_type())
            && let Some(text) = item.text()
        {
            let key = non_ws(text);
            if !key.is_empty() {
                *furniture_counts.entry(key).or_insert(0) += 1;
            }
        }
    }
    // 长的优先，处理「公司名 + 文档编号」拼成一个 text 块的泄漏形态
    let mut corroborated: Vec<&str> = furniture_counts
        .iter()
        .filter(|(_, n)| **n >= 2)
        .map(|(t, _)| t.as_str())
        .collect();
    corroborated.sort_by_key(|x| std::cmp::Reverse(x.chars().count()));

    // text 是否完全由已分类家具文本拼成；是则返回命中的家具文本列表。
    let furniture_leak = |text: &str| -> Option<Vec<&str>> {
        let mut rest = non_ws(text);
        if rest.is_empty() || rest.chars().count() > 200 {
            return None;
        }
        let mut hits: Vec<&str> = Vec::new();
        for f in &corroborated {
            if rest.contains(f) {
                hits.push(f);
                rest = rest.split(f).collect::<Vec<_>>().join("");
            }
        }
        if !hits.is_empty() && rest.is_empty() {
            Some(hits)
        } else {
            None
        }
    };

    for (i, r) in items.iter().enumerate() {
        let id = &r.id;
        let item = &r.item;
        let text = item.text().unwrap_or("");

        // ── 伪 HEADING（→ demote/merge）──
        if item.item_type() == "text" && item.text_level().is_some() && !text.is_empty() {
            let body = LEADING_NUMBERING.replace(text, "");
            let mut reasons: Vec<String> = Vec::new();
            if COMMA_SEMI.is_match(text) {
                reasons.push("含逗号/分号".into());
            }
            if SENTENCE_END.is_match(text) {
                reasons.push("以句末标点收尾".into());
            }
            if non_ws_len(&body) > 40 {
                reasons.push(format!("去编号后正文过长({}字)", non_ws_len(&body)));
            }
            if !reasons.is_empty() {
                out.push(suspect(
                    SuspectKind::PseudoHeading,
                    id,
                    format!(
                        "疑似伪标题: {}。text=「{}」",
                        reasons.join("、"),
                        char_prefix(text, 80)
                    ),
                    true,
                ));
            }
        }

        // ── 跨页断句（→ merge）。页边界处隔着 header/page_number 等页面家具，跳过它们找下一个内容块。──
        {
            let mut j = i + 1;
            while j < items.len() && is_page_furniture(items[j].item.item_type()) {
                j += 1;
            }
            if let Some(next) = items.get(j) {
                let ntext = next.item.text().unwrap_or("");
                if item.item_type() == "text"
                    && next.item.item_type() == "text"
                    && item.text_level().is_none()
                    && next.item.text_level().is_none()
                    && let (Some(page), Some(npage)) = (item.page_idx(), next.item.page_idx())
                    && npage == page + 1
                    && !text.is_empty()
                    && !ntext.is_empty()
                    && !SENTENCE_END.is_match(text)
                    && !HEADING_START.is_match(ntext)
                    && !BULLET_START.is_match(text) // 前块是列表项：行尾无标点是常态，不是断句
                    && !BULLET_START.is_match(ntext)
                // 后块是列表项：是新条目，不是前句的延续
                {
                    out.push(suspect(
                        SuspectKind::CrossPageBreak,
                        id,
                        format!(
                            "疑似跨页断句: 第{}页块尾「…{}」无句末标点，第{}页块首「{}…」非标题特征。后块={}",
                            page,
                            char_suffix(text, 40),
                            npage,
                            char_prefix(ntext, 40),
                            next.id
                        ),
                        true,
                    ));
                }
            }
        }

        // ── 巨型块（→ split）──
        if item.item_type() == "text" && non_ws_len(text) > GIANT_BLOCK_CHARS {
            let heading_hits = GIANT_HEADING_HIT.find_iter(text).count();
            if heading_hits >= 2 {
                out.push(suspect(
                    SuspectKind::GiantBlock,
                    id,
                    format!(
                        "巨型块: {} 字且含 {} 个疑似小标题编号",
                        non_ws_len(text),
                        heading_hits
                    ),
                    true,
                ));
            }
        }

        // ── 混入正文的页码/页眉页脚（→ drop）。仅看 type=text：已被 MinerU 正确分类的
        // page_number/header/footer 不是异常，消费方自会处理。──
        let leak_hits = if item.item_type() == "text" {
            furniture_leak(text)
        } else {
            None
        };
        if let Some(hits) = leak_hits {
            let detail = hits
                .iter()
                .map(|h| {
                    format!(
                        "「{}」×{}处家具佐证",
                        h,
                        furniture_counts.get(*h).copied().unwrap_or(0)
                    )
                })
                .collect::<Vec<_>>()
                .join("、");
            out.push(suspect(
                SuspectKind::PageArtifact,
                id,
                format!(
                    "与已分类页眉/页脚同文（{detail}），疑似漏标的页面家具。text=「{}」",
                    text.trim()
                ),
                true,
            ));
        } else if item.item_type() == "text" && repeated_texts.contains(text.trim()) {
            out.push(suspect(
                SuspectKind::PageArtifact,
                id,
                format!(
                    "高频重复短文本（出现于 {} 个不同页），疑似页眉/页脚/水印。text=「{}」",
                    text_pages.get(text.trim()).map(|p| p.len()).unwrap_or(0),
                    text.trim()
                ),
                true,
            ));
        } else if item.item_type() == "text"
            && item.text_level().is_none()
            && !text.trim().is_empty()
            && non_ws_len(text) <= 10
            && PAGE_NUMBER_CHARS.is_match(text.trim())
        {
            out.push(suspect(
                SuspectKind::PageArtifact,
                id,
                format!("疑似混入正文的页码: text=「{}」", text.trim()),
                true,
            ));
        }

        // ── 残留符号（→ strip）──
        if item.item_type() == "text" && !text.is_empty() {
            let mut hits: Vec<&str> = Vec::new();
            if MD_LINK.is_match(text) {
                hits.push("markdown 链接");
            }
            if LATEX.is_match(text) {
                hits.push("LaTeX 残片");
            } else if ESCAPED_DOLLAR.is_match(text) {
                hits.push("孤立 \\$ 转义");
            }
            if HTML_TAG.is_match(text) {
                hits.push("HTML 标签");
            }
            if !hits.is_empty() {
                out.push(suspect(
                    SuspectKind::ResidualMarkup,
                    id,
                    format!(
                        "残留符号: {}。text=「{}」",
                        hits.join("、"),
                        char_prefix(text, 100)
                    ),
                    true,
                ));
            }
        }

        // ── 空壳表（→ drop）：零内容占位。MinerU 跨页合并表格后，续页常留下
        // {"type":"table","img_path":"","table_caption":[]} 这种无行无字的空壳（真实数据 8/11 的"续表"如此）。
        // 不要求紧跟在表后：空壳链（连续多页占位）里后面的壳只挨着前面的壳。──
        if is_empty_table_husk(item) {
            let prev = items[..i]
                .iter()
                .rev()
                .find(|r| !is_page_furniture(r.item.item_type()));
            out.push(suspect(
                SuspectKind::EmptyTable,
                id,
                format!(
                    "零内容空壳表（无表格行/caption/图），疑似 MinerU 跨页合并后留下的占位。前一个内容块={}",
                    prev.map(|p| format!(
                        "{}({}, p{})",
                        p.id,
                        p.item.item_type(),
                        p.item.page_idx().map(|n| n.to_string()).unwrap_or_else(|| "?".into())
                    ))
                    .unwrap_or_else(|| "无".into())
                ),
                true,
            ));
        }

        // ── 跨页拆表/拆列表（→ mergeTable/mergeList）。页边界处跳过页面家具找下一个内容块。
        // 页码只要求严格递增、不要求相邻：结构相邻（中间仅家具）已保证跳过的页面没有正文，
        // 而 mergeTable 产物 page_idx 取首块，链式拆表（3 页以上）合并一段后与下一段隔 ≥2 页，
        // 若要求 page_idx+1 则链条在第二段处断掉，永远合不完。──
        if (item.item_type() == "table" || item.item_type() == "list")
            && let Some(page) = item.page_idx()
        {
            let mut j = i + 1;
            while j < items.len() && is_page_furniture(items[j].item.item_type()) {
                j += 1;
            }
            if let Some(next) = items.get(j)
                && next.item.item_type() == item.item_type()
                && let Some(npage) = next.item.page_idx()
                && npage > page
            {
                let gap_note = if npage - page > 1 {
                    format!(
                        "，中间隔 {} 页且无任何正文块（常见于已合并过一段的链式拆表）",
                        npage - page - 1
                    )
                } else {
                    String::new()
                };
                if item.item_type() == "table" {
                    // 任一侧是空壳就合不了（空壳走 empty_table → drop），只标双方有体的
                    if !is_empty_table_husk(item) && !is_empty_table_husk(&next.item) {
                        let cols = |body: Option<&str>| -> usize {
                            let rows = table_rows(body.unwrap_or(""));
                            rows.first()
                                .map(|r| TD_TH.find_iter(r).count())
                                .unwrap_or(0)
                        };
                        out.push(suspect(
                            SuspectKind::SplitTable,
                            id,
                            format!(
                                "跨页两 table 间仅页面家具（{} p{} 首行{}列 + {} p{} 首行{}列{}），疑似同一表格被拆。后块={}。注意：列数不等可能是 rowspan 跨页携带，不能仅凭列数否定",
                                id,
                                page,
                                cols(item.table_body()),
                                next.id,
                                npage,
                                cols(next.item.table_body()),
                                gap_note,
                                next.id
                            ),
                            true,
                        ));
                    }
                } else {
                    let a_tail = item
                        .str_array("list_items")
                        .and_then(|v| v.last().map(|s| char_suffix(s, 40)))
                        .unwrap_or_default();
                    let b_head = next
                        .item
                        .str_array("list_items")
                        .and_then(|v| v.first().map(|s| char_prefix(s, 40)))
                        .unwrap_or_default();
                    out.push(suspect(
                        SuspectKind::SplitList,
                        id,
                        format!(
                            "跨页两 list 间仅页面家具（{} p{} 尾项「…{}」 + {} p{} 首项「{}…」{}），疑似同一列表被拆。后块={}",
                            id, page, a_tail, next.id, npage, b_head, gap_note, next.id
                        ),
                        true,
                    ));
                }
            }
        }

        // ── 以下仅标记、无 op（标记后不做处理）──
        if item.item_type() == "table" || item.item_type() == "image" {
            let caption_key = if item.item_type() == "table" {
                "table_caption"
            } else {
                "img_caption"
            };
            let empty = match item
                .0
                .get(caption_key)
                .and_then(serde_json::Value::as_array)
            {
                None => true,
                Some(arr) => {
                    arr.is_empty()
                        || arr
                            .iter()
                            .all(|c| c.as_str().map(|s| s.trim().is_empty()).unwrap_or(true))
                }
            };
            if empty {
                out.push(suspect(
                    SuspectKind::CaptionIssue,
                    id,
                    format!("{} 无 caption 或 caption 为空", item.item_type()),
                    false,
                ));
            }
        }
    }

    out
}

re!(TD_TH, r"(?i)<t[dh]");

/// 当前被标为 page_artifact / empty_table 的 id 集（drop 白名单的第二道保险）。
pub fn droppable_ids(worklist: &[WorkItem]) -> HashSet<String> {
    worklist
        .iter()
        .filter(|w| matches!(w.kind, SuspectKind::PageArtifact | SuspectKind::EmptyTable))
        .map(|w| w.item_id.clone())
        .collect()
}

/// detect 接受 &[MineruItem] 的便捷版本（仅给独立消费方统计疑点用）。
pub fn detect_items(items: &[MineruItem]) -> Vec<WorkItem> {
    let refs: Vec<RefItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| RefItem {
            id: format!("it_{:04}", i + 1),
            item: item.clone(),
        })
        .collect();
    detect(&refs)
}
