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
// 段尾粘连的节标记（跨页 merge 把「[相关文件]」这类独立结构块吸进了上一段的结尾）
re!(TRAILING_MARKER, r"[\[【]相关[^\]】\n]{0,6}[\]】]\s*$");
// 编号前缀（漏标标题探测用，按数制分三类，括号编号「(1)」是列表标记不参与）
re!(
    NUM_CHAPTER,
    r"^\s*第\s*([0-9一二三四五六七八九十百]+)\s*[章节条款部分篇]"
);
re!(NUM_CHINESE, r"^\s*([一二三四五六七八九十]+)\s*[、.．]");
re!(NUM_ARABIC, r"^\s*([0-9]+(?:[.．][0-9]+)*)");

// ── 编号解析（missed_heading / separated_caption 用）──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumStyle {
    Chapter,
    Chinese,
    Arabic,
}

fn cn_digit(c: char) -> Option<u64> {
    Some(match c {
        '一' => 1,
        '二' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '七' => 7,
        '八' => 8,
        '九' => 9,
        _ => return None,
    })
}

/// 中文数字 → 数值（一 ~ 九十九，编号场景够用）。
fn cn_value(s: &str) -> Option<u64> {
    let cs: Vec<char> = s.chars().collect();
    match cs.as_slice() {
        ['十'] => Some(10),
        [c] => cn_digit(*c),
        ['十', b] => Some(10 + cn_digit(*b)?),
        [a, '十'] => Some(cn_digit(*a)? * 10),
        [a, '十', b] => Some(cn_digit(*a)? * 10 + cn_digit(*b)?),
        _ => None,
    }
}

/// 解析行首编号 → (编号路径, 数制, 编号在 text 中的字节终点)。
/// 「4.6核心…」→ ([4,6], Arabic)；「二、范围」→ ([2], Chinese)；「第3章」→ ([3], Chapter)。
/// 防误判：阿拉伯编号各段 ≤99（排除年份/日期），后随 % 的是数值不是编号。
pub(crate) fn parse_numbering(text: &str) -> Option<(Vec<u64>, NumStyle, usize)> {
    if let Some(c) = NUM_CHAPTER.captures(text) {
        let raw = &c[1];
        let v = raw.parse::<u64>().ok().or_else(|| cn_value(raw))?;
        return Some((vec![v], NumStyle::Chapter, c.get(0).unwrap().end()));
    }
    if let Some(c) = NUM_CHINESE.captures(text) {
        return Some((
            vec![cn_value(&c[1])?],
            NumStyle::Chinese,
            c.get(0).unwrap().end(),
        ));
    }
    if let Some(c) = NUM_ARABIC.captures(text) {
        let m = c.get(1).unwrap();
        if matches!(text[m.end()..].chars().next(), Some('%') | Some('％')) {
            return None;
        }
        let mut path = Vec::new();
        for part in c[1].split(['.', '．']) {
            let v = part.parse::<u64>().ok()?;
            if v > 99 {
                return None;
            }
            path.push(v);
        }
        return Some((path, NumStyle::Arabic, m.end()));
    }
    None
}

/// 去掉编号及紧随的分隔符/空白后的正文。
fn numbering_body(text: &str, num_end: usize) -> &str {
    text[num_end..]
        .trim_start_matches(|c: char| matches!(c, '、' | '.' | '．') || c.is_whitespace())
}

/// 标题候选的表面特征：去编号后正文短（≤30 内容字）、无逗号/分号、无句末标点。
fn promote_candidate(item: &MineruItem) -> Option<(Vec<u64>, NumStyle)> {
    if item.item_type() != "text" || item.text_level().is_some() {
        return None;
    }
    let text = item.text()?;
    let (path, style, num_end) = parse_numbering(text)?;
    let body = numbering_body(text, num_end);
    let blen = non_ws_len(body);
    if blen == 0 || blen > 30 || COMMA_SEMI.is_match(text) || SENTENCE_END.is_match(text) {
        return None;
    }
    Some((path, style))
}

/// caption 表面特征：短（≤30 内容字）、无句末标点、以「表/图」收尾。
fn caption_like(text: &str) -> bool {
    let n = non_ws_len(text);
    (2..=30).contains(&n)
        && !SENTENCE_END.is_match(text)
        && matches!(text.trim_end().chars().last(), Some('表') | Some('图'))
}

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

    // 全文编号块索引（missed_heading 的兄弟查找用）：text 块且行首可解析出编号。
    struct Numbered {
        idx: usize,
        path: Vec<u64>,
        style: NumStyle,
        heading: bool,
    }
    let numbered: Vec<Numbered> = items
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let it = &r.item;
            if it.item_type() != "text" {
                return None;
            }
            let (path, style, _) = parse_numbering(it.text()?)?;
            Some(Numbered {
                idx: i,
                path,
                style,
                heading: it.text_level().is_some(),
            })
        })
        .collect();
    let numbered_at: HashMap<usize, usize> = numbered
        .iter()
        .enumerate()
        .map(|(k, n)| (n.idx, k))
        .collect();

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

        // ── 漏标标题（→ promote）：两路结构证据，命中任一即标。
        // 表面闸（共用）：去编号后 ≤30 内容字、无逗号/分号、无句末标点（正文段落天然不命中）。
        // ① 兄弟证据：同级编号兄弟（同数制、同深度、同父编号）的最近一个是标题且编号恰好相邻（±1）。
        // ② 子项证据：下一个内容块的编号以本块编号为真前缀（同数制）。整组同级兄弟都被
        //    MinerU 漏标时 ① 永不触发，子项前缀是唯一可靠的结构证据（真实数据：JZY-001
        //    「2.1确定竞争对手：」「2.2竞争情报收集：」等 15 处全因此漏网）。──
        if let Some((path, style)) = promote_candidate(item) {
            let depth = path.len();
            let parent = &path[..depth - 1];
            let same_group = |n: &Numbered| {
                n.style == style && n.path.len() == depth && &n.path[..depth - 1] == parent
            };
            let k = numbered_at[&i];
            let prev = numbered[..k].iter().rev().find(|n| same_group(n));
            let next = numbered[k + 1..].iter().find(|n| same_group(n));
            let last = path[depth - 1];
            let sibling = prev
                .filter(|n| n.heading && n.path[depth - 1] + 1 == last)
                .or_else(|| next.filter(|n| n.heading && last + 1 == n.path[depth - 1]));
            if let Some(sib) = sibling {
                let sib_ref = &items[sib.idx];
                let level = sib_ref.item.text_level().unwrap_or(1);
                out.push(suspect(
                    SuspectKind::MissedHeading,
                    id,
                    format!(
                        "同级编号兄弟 {}「{}」是标题（level={level}），而本块是正文 → 疑似漏标标题。text=「{}」",
                        sib_ref.id,
                        char_prefix(sib_ref.item.text().unwrap_or(""), 40),
                        char_prefix(text, 60)
                    ),
                    true,
                ));
            } else {
                let mut j = i + 1;
                while j < items.len() && is_page_furniture(items[j].item.item_type()) {
                    j += 1;
                }
                let child = items.get(j).filter(|n| {
                    n.item.item_type() == "text"
                        && n.item.text().and_then(parse_numbering).is_some_and(
                            |(cpath, cstyle, _)| {
                                cstyle == style && cpath.len() > depth && cpath[..depth] == path[..]
                            },
                        )
                });
                if let Some(ch) = child {
                    let dotted = |p: &[u64]| {
                        p.iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(".")
                    };
                    out.push(suspect(
                        SuspectKind::MissedHeading,
                        id,
                        format!(
                            "下一个内容块 {}「{}」的编号以本块编号 {} 为前缀（子项），而本块是短编号正文 → 疑似漏标标题。text=「{}」",
                            ch.id,
                            char_prefix(ch.item.text().unwrap_or(""), 40),
                            dotted(&path),
                            char_prefix(text, 60)
                        ),
                        true,
                    ));
                }
            }
        }

        // ── 疑似赘字/衍字（→ deleteChar）：功能词叠字（的的/地地/是是/了了）与孤立
        // 偏旁部首。「目的+的」「但是+是」类合法语法必须由 LLM 结合语境裁决，判不了
        // 就 dismiss 不改。多处命中合并为一个疑点（一轮删一处，loop 收敛）。──
        if item.item_type() == "text" && !text.is_empty() {
            let chars: Vec<char> = text.chars().collect();
            let hits = crate::extrachar::scan(&chars);
            if !hits.is_empty() {
                let detail = hits
                    .iter()
                    .map(|h| {
                        let lo = h.offset.saturating_sub(10);
                        let hi = (h.offset + 11).min(chars.len());
                        let ctx: String = chars[lo..hi].iter().collect();
                        format!(
                            "「{}」中 offset={} 的「{}」（{}）",
                            ctx,
                            h.offset,
                            h.ch,
                            match h.kind {
                                crate::extrachar::ExtraKind::DupWord => "功能词叠字",
                                crate::extrachar::ExtraKind::Radical => "孤立偏旁部首",
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("；");
                out.push(suspect(
                    SuspectKind::ExtraChar,
                    id,
                    format!(
                        "疑似 OCR 赘字/衍字: {detail}。确认是衍字 → deleteChar(offset=证据中的值)；属正常语法（如「目的+的」「但是+是」「不甚了了」）或在讨论偏旁本身 → dismiss"
                    ),
                    true,
                ));
            }
        }

        // ── 段尾粘连节标记（→ split）：跨页 merge 把「[相关文件]」类独立结构块
        // 吸进了上一段结尾。建议 offset 直接给出，split 可一步到位。──
        if item.item_type() == "text"
            && item.text_level().is_none()
            && let Some(m) = TRAILING_MARKER.find(text)
            && non_ws_len(&text[..m.start()]) >= 10
        {
            let offset = text[..m.start()].chars().count();
            out.push(suspect(
                SuspectKind::TrailingMarker,
                id,
                format!(
                    "段尾粘连节标记「{}」，疑似被跨页合并进上一段，建议 split(offset={offset}) 拆成独立块。段尾=「…{}」",
                    m.as_str().trim(),
                    char_suffix(text, 50)
                ),
                true,
            ));
        }

        // ── 被吞进 table_caption 的小节标题（→ extractCaption）：caption 条目过标题
        // 表面闸且行首编号可解析，且存在同数制同深度同父编号的相邻（±1）标题兄弟
        //（真实数据：MinerU 把「4.6核心组织绩效的应用」「4.7公司十大核心指标」塞进了
        // 相邻表格的 caption，渲染后貌似漏 promote，实为结构错位）。
        // 每表只标首个命中条目：抽出后下一轮迭代再标剩余的，loop 自然收敛。──
        if item.item_type() == "table"
            && let Some(captions) = item.str_array("table_caption")
        {
            for (ci, cap) in captions.iter().enumerate() {
                let Some((path, style, num_end)) = parse_numbering(cap) else {
                    continue;
                };
                let body = numbering_body(cap, num_end);
                let blen = non_ws_len(body);
                if blen == 0 || blen > 30 || COMMA_SEMI.is_match(cap) || SENTENCE_END.is_match(cap)
                {
                    continue;
                }
                let depth = path.len();
                let parent = &path[..depth - 1];
                let last = path[depth - 1];
                let sibling = numbered.iter().find(|n| {
                    n.heading
                        && n.style == style
                        && n.path.len() == depth
                        && &n.path[..depth - 1] == parent
                        && (n.path[depth - 1] + 1 == last || last + 1 == n.path[depth - 1])
                });
                if let Some(sib) = sibling {
                    let sib_ref = &items[sib.idx];
                    let level = sib_ref.item.text_level().unwrap_or(1);
                    out.push(suspect(
                        SuspectKind::CaptionHeading,
                        id,
                        format!(
                            "table_caption[{ci}]「{}」疑似被吞进 caption 的小节标题：行首编号与相邻标题兄弟 {}「{}」（level={level}）同级。\
                             确认是标题 → extractCaption(captionIndex={ci}, level={level}, position 按内容归属判断：\
                             表格属于该标题【之前】的小节（标题是表格之后内容的开头）→ after；\
                             表格是该标题小节的首个内容 → before）；确认是真表格题注 → dismiss",
                            cap.trim(),
                            sib_ref.id,
                            char_prefix(sib_ref.item.text().unwrap_or(""), 40),
                        ),
                        true,
                    ));
                    break;
                }
            }
        }

        // ── 被吞进 table_caption 的页眉/页脚家具（→ dropCaption）：caption 条目的非空白文本
        // 与已正确分类的 header/footer 同文（≥2 处佐证），或全文高频重复（≥3 页）。MinerU 把
        // 跑马灯页眉/页脚塞进了跨页表片段的 caption，mergeTable 又忠实保留 → 渲染成残留。
        // 真实数据：JZY-001「附件3：…」页眉 + 「编制人：张威」页脚被吞进「细分市场」表 caption。
        // page_artifact 探测器只扫 item 文本、不扫 caption 数组，caption_heading 只认编号标题，
        // 二者都漏。每表只标首个命中条目：删掉后下一轮迭代再标剩余的，loop 自然收敛。──
        if item.item_type() == "table"
            && let Some(captions) = item.str_array("table_caption")
        {
            for (ci, cap) in captions.iter().enumerate() {
                let key = non_ws(cap);
                if key.is_empty() {
                    continue;
                }
                let corrob = corroborated.contains(&key.as_str());
                let repeated = repeated_texts.contains(cap.trim());
                if !corrob && !repeated {
                    continue;
                }
                let evidence = if corrob {
                    format!(
                        "table_caption[{ci}]「{}」与已分类页眉/页脚同文（{}×{}处家具佐证），\
                         疑似被吞进 caption 的页面家具。确认是家具 → dropCaption(captionIndex={ci})；\
                         确认是真表格题注 → dismiss",
                        cap.trim(),
                        key,
                        furniture_counts.get(&key).copied().unwrap_or(0),
                    )
                } else {
                    format!(
                        "table_caption[{ci}]「{}」是高频重复短文本（出现于 {} 个不同页），\
                         疑似被吞进 caption 的页眉/页脚/水印。确认是家具 → dropCaption(captionIndex={ci})；\
                         确认是真表格题注 → dismiss",
                        cap.trim(),
                        text_pages.get(cap.trim()).map(|p| p.len()).unwrap_or(0),
                    )
                };
                out.push(suspect(SuspectKind::CaptionArtifact, id, evidence, true));
                break;
            }
        }

        // ── caption 与表格被标题隔开（→ reorder）：caption 样短文本后紧跟一个标题
        //（或漏标标题候选），标题后紧跟有体表格 → 三块顺序疑似错排。──
        if item.item_type() == "text" && item.text_level().is_none() && caption_like(text) {
            let next_content = |from: usize| -> Option<usize> {
                (from..items.len()).find(|&j| !is_page_furniture(items[j].item.item_type()))
            };
            if let Some(jh) = next_content(i + 1)
                && items[jh].item.item_type() == "text"
                && (items[jh].item.text_level().is_some()
                    || promote_candidate(&items[jh].item).is_some())
                && let Some(jt) = next_content(jh + 1)
                && items[jt].item.item_type() == "table"
                && !is_empty_table_husk(&items[jt].item)
            {
                let (h, t) = (&items[jh], &items[jt]);
                out.push(suspect(
                    SuspectKind::SeparatedCaption,
                    id,
                    format!(
                        "短文本「{}」疑似表格 caption，但与表格 {} 之间隔着标题块 {}「{}」。\
                         若表格属于 caption 所在小节 → reorder([{}, {}, {}])；\
                         若 caption 与表格都属于新小节 → reorder([{}, {}, {}])；拿不准 → dismiss。标题={} 表格={}",
                        text.trim(),
                        t.id,
                        h.id,
                        char_prefix(h.item.text().unwrap_or(""), 40),
                        id, t.id, h.id,
                        h.id, id, t.id,
                        h.id, t.id
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
                "image_caption"
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

/// 当前被标为 caption_artifact 的表格 id 集（dropCaption 白名单的第二道保险）。
pub fn droppable_caption_ids(worklist: &[WorkItem]) -> HashSet<String> {
    worklist
        .iter()
        .filter(|w| matches!(w.kind, SuspectKind::CaptionArtifact))
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
