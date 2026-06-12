// 重度乱码表的视觉重转写层（opt-in，rewrite_garbled_tables=true 才运行）：
// 核心 refine 出口闸门【之后】、混淆层【之前】的独立后处理。
//
// 动机（实证）：个别表格被 OCR 整体认废（ZBZ-047 一表 13+ 处乱码：代格/目择值/
// 数据来酒/Midhuel…），逐字符混淆修正救不动——但其 img_path 截图完全清晰可读。
// 乱码表的归宿是对照图像整格重转写，不是逐字"修复"（混淆层的每表聚合密度闸门
// 正是按住这种表等本层接手）。
//
// 权力结构：目标选定 100% 由机械检测器定（LLM 无提名权），视觉 LLM 只有
// 单元格级提案权，落地由机械闸门裁决：
//   闸门 1（资格）：原格必须有"乱码已毁"的证据（空格/纯数值格/短编号格/词覆盖率
//                   正常的格一律不许动）——实测视觉模型在宽乱码表上会行列错位，
//                   对这类格的"修正"几乎全是把别格内容张冠李戴过来；
//   闸门 2（结构）：行列号必须命中现存单元格、无标签字符/控制字符、长度上限、
//                   提案不得是纯数值、长度量级与原格可比（均为错位特征）；
//   闸门 3（整表回归）：重转写后的词典覆盖率必须严格高于重转写前——
//                       视觉模型在"修复"以外做的任何事都会被这道闸门按住，整表回退。
// 每条替换进 report.tableRewrites（含撤销凭据 before）与 provenance，可程序化撤销。
// 层内任何视觉故障 → 搁置该表（漏修，不毁全局）；层级 panic 由调用方兜（丢弃整层）。
//
// 机械检测器（字典命中率）：对单元格文本的汉字段做正向最大匹配（内嵌 6 万常用词，
// 见 data/cn_words.txt），算被词覆盖的字符比例。乱码词的特征是"常用字的非词组合"
//（代格/目择/来酒），覆盖率塌方；正常表即便满是专名（股票代码/公司名）也明显更高。
// 阈值按 test_data 三份真实文档标定：乱码表 0.46，最差正常表 0.61，取 0.55。

use crate::agent_loop::Logger;
use crate::llm::{LoadImage, VisionClient};
use crate::types::{RefItem, TableCellRewrite, TokenUsage};
use futures::StreamExt;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::LazyLock;

/// 重转写层独立的 prompt 版本（进缓存 key）。
pub const GARBLED_PROMPT_VERSION: &str = "g1";

/// 覆盖率判废阈值（标定见模块头注释）。
const COVERAGE_THRESHOLD: f64 = 0.55;
/// 样本下限：汉字段总字符数不足时不判（小表/数字表没有判别信息）。
const MIN_SAMPLE_CHARS: usize = 24;
/// 单元格重转写文本长度上限（图片单元格不会有小作文，超长 = 模型跑偏）。
const MAX_CELL_TEXT_CHARS: usize = 256;
/// 送审渲染体积上限（字符）：超大表不送审（成本闸，宁可漏修）。
const MAX_RENDER_CHARS: usize = 8000;

// ── 字典命中率 ──

static CN_WORDS_RAW: &str = include_str!("../data/cn_words.txt");

/// 常用词集合（2~4 字纯汉字，jieba 词频 top 6 万，生成脚本 scripts/gen_cn_words.py）。
static CN_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    CN_WORDS_RAW
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
});

fn is_hanzi(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}')
}

/// 对文本里长度 ≥2 的汉字段做正向最大匹配（4→3→2），
/// 返回 (汉字段总字符数, 被词典词覆盖的字符数)。
fn word_coverage<'a>(texts: impl Iterator<Item = &'a str>) -> (usize, usize) {
    let mut total = 0usize;
    let mut covered = 0usize;
    let mut run: Vec<char> = Vec::new();
    let mut consume = |run: &mut Vec<char>| {
        if run.len() < 2 {
            run.clear();
            return;
        }
        total += run.len();
        let mut i = 0;
        while i < run.len() {
            let mut matched = 0;
            for len in (2..=4.min(run.len() - i)).rev() {
                let w: String = run[i..i + len].iter().collect();
                if CN_WORDS.contains(w.as_str()) {
                    matched = len;
                    break;
                }
            }
            if matched > 0 {
                covered += matched;
                i += matched;
            } else {
                i += 1;
            }
        }
        run.clear();
    };
    for t in texts {
        for c in t.chars() {
            if is_hanzi(c) {
                run.push(c);
            } else {
                consume(&mut run);
            }
        }
        consume(&mut run);
    }
    (total, covered)
}

/// 表格的乱码评分：(汉字段样本字符数, 词典覆盖率)。样本不足回 None（不可判）。
pub fn garbled_score(cell_texts: &[String]) -> Option<(usize, f64)> {
    let (total, covered) = word_coverage(cell_texts.iter().map(String::as_str));
    if total < MIN_SAMPLE_CHARS {
        return None;
    }
    Some((total, covered as f64 / total as f64))
}

/// 独立可调的机械检测器：table_body 判废时返回 (样本字符数, 覆盖率)，否则 None。
pub fn detect_garbled_table(table_body: &str) -> Option<(usize, f64)> {
    let cells = parse_cells(table_body);
    let texts: Vec<String> = cells.iter().map(|c| c.text.clone()).collect();
    let (sample, coverage) = garbled_score(&texts)?;
    (coverage < COVERAGE_THRESHOLD).then_some((sample, coverage))
}

// ── 单元格解析 ──

/// table_body 里的一个单元格：start/end 是内层 HTML 在整串中的字符区间
/// （开 `<td…>` 的 `>` 之后到对应闭合标签的 `<` 之前，含 `<br>` 等内层标签），
/// text 是区间内剥掉标签后的文本（实体不解码）。
#[derive(Clone, Debug)]
pub(crate) struct Cell {
    pub row: usize,
    pub col: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// 词法切分 table_body 出单元格区间。畸形 HTML 不 panic：
/// 单元格外的散落文本忽略，未闭合的末尾单元格在串尾收口。
pub(crate) fn parse_cells(s: &str) -> Vec<Cell> {
    let chars: Vec<char> = s.chars().collect();
    let mut cells: Vec<Cell> = Vec::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut tag_open = 0usize; // 当前标签 '<' 的偏移
    let mut row = 0usize;
    let mut col_in_row = 0usize;
    let mut seen_tr = false;
    // 打开中的单元格：(row, col, start, text)
    let mut open: Option<(usize, usize, usize, String)> = None;

    for (i, &c) in chars.iter().enumerate() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let name: String = tag
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '/')
                    .collect::<String>()
                    .to_ascii_lowercase();
                // td/th/tr/对应闭合/table 闭合都终结当前单元格；<br> 等内层标签不终结
                let closes = matches!(
                    name.as_str(),
                    "td" | "th" | "/td" | "/th" | "tr" | "/tr" | "/table" | "/tbody"
                );
                if closes && let Some((r, cl, start, text)) = open.take() {
                    cells.push(Cell {
                        row: r,
                        col: cl,
                        start,
                        end: tag_open,
                        text,
                    });
                }
                match name.as_str() {
                    "tr" => {
                        if seen_tr {
                            row += 1;
                        }
                        seen_tr = true;
                        col_in_row = 0;
                    }
                    "td" | "th" => {
                        open = Some((row, col_in_row, i + 1, String::new()));
                        col_in_row += 1;
                    }
                    _ => {}
                }
            } else {
                tag.push(c);
            }
            continue;
        }
        if c == '<' {
            in_tag = true;
            tag.clear();
            tag_open = i;
            continue;
        }
        if let Some((_, _, _, text)) = &mut open {
            text.push(c);
        }
    }
    if let Some((r, cl, start, text)) = open.take() {
        cells.push(Cell {
            row: r,
            col: cl,
            start,
            end: chars.len(),
            text,
        });
    }
    cells
}

/// 重转写文本落进 HTML 的转义（闸门已拒掉 <>，只剩 & 需要转）。
fn escape_cell_text(s: &str) -> String {
    s.replace('&', "&amp;")
}

/// 结构闸门：重转写文本不得引入标签/控制字符，长度有上限。
fn structurally_valid(text: &str) -> bool {
    text.chars().count() <= MAX_CELL_TEXT_CHARS
        && !text
            .chars()
            .any(|c| c == '<' || c == '>' || (c.is_control() && c != '\t'))
}

/// 纯数值样内容（数字/百分号/区间符），不含任何文字信息。
fn is_numeric_like(t: &str) -> bool {
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_digit() || " .,，%‰/:：~±+-()（）".contains(c))
}

/// 最长连续 ASCII 字母段长度。
fn longest_alpha_run(t: &str) -> usize {
    let mut best = 0;
    let mut cur = 0;
    for c in t.chars() {
        if c.is_ascii_alphabetic() {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// 资格闸门（对原格内容机械判定）：只有"乱码已毁"证据充分的格才许被重转写。
/// 空格、纯数值格、短拉丁编号格（G1.4）一律不许——实测视觉模型在宽乱码表上
/// 会行列错位，对这类格的"修正"几乎全是把别格内容张冠李戴过来。
fn rewritable(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || is_numeric_like(t) {
        return false;
    }
    if t.chars().any(is_hanzi) {
        let (total, covered) = word_coverage(std::iter::once(t));
        // 无词样本（全是孤立单字）= 乱码弹片，可重转写；
        // 有词样本则覆盖率高的是正常内容，不许动
        return total == 0 || (covered as f64) < 0.75 * total as f64;
    }
    // 无汉字：仅词样拉丁 token（Midhuel）可重转写，编号（G1.4、B1.36%）留给混淆层
    longest_alpha_run(t) >= 3
}

/// 长度比闸门：重转写是"同一格内容的重读"，长度量级必须可比——
/// 大幅缩水/膨胀（「当年累计数据…」→「Michael」）是行列错位的特征。
fn length_ratio_ok(old_chars: usize, new_chars: usize) -> bool {
    new_chars <= 2 * old_chars + 2 && old_chars <= 2 * new_chars + 2
}

/// 送审渲染：按行列铺开当前单元格内容（行列号即提案坐标系），
/// 可重转写的格标 ✎（其余格只作对照，提案到了落地闸门也会被拒）。
fn render_cells(cells: &[Cell]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current_row = usize::MAX;
    for cell in cells {
        if cell.row != current_row {
            current_row = cell.row;
            lines.push(format!("第{}行：", cell.row));
        }
        let mark = if rewritable(&cell.text) { "✎" } else { "" };
        let body = if cell.text.trim().is_empty() {
            "〈空〉".to_string()
        } else {
            format!("「{}」", cell.text.replace('\n', "⏎"))
        };
        let line = lines.last_mut().expect("行头先于单元格压入");
        line.push_str(&format!("[{}{mark}]{body} ", cell.col));
    }
    lines.join("\n")
}

// ── 主流程 ──

#[derive(Default)]
pub struct GarbledOutcome {
    pub fixes: Vec<TableCellRewrite>,
    /// 被闸门拒绝的提案数（原格无资格 / 结构非法 / 行列不存在 / 错位特征 / 整表覆盖率回归不过）。
    pub rejected: u64,
    pub usage: TokenUsage,
}

struct Target {
    item_idx: usize,
    img_path: String,
    cells: Vec<Cell>,
    coverage: f64,
}

/// 对 items 中机械检测判废的表格做视觉重转写。
/// 取得 items 所有权：调用方在 panic 时（catch_unwind）保留原件，天然整层丢弃。
pub async fn rewrite_garbled_tables(
    mut items: Vec<RefItem>,
    vision: Arc<dyn VisionClient>,
    load_image: Arc<dyn LoadImage>,
    concurrency: usize,
    log: &Logger,
) -> (Vec<RefItem>, GarbledOutcome) {
    let mut outcome = GarbledOutcome::default();

    // 1. 机械检测：词典覆盖率塌方的表才进入本层（LLM 无目标提名权）
    let mut targets: Vec<Target> = Vec::new();
    for (item_idx, r) in items.iter().enumerate() {
        let Some(tb) = r.item.table_body() else {
            continue;
        };
        let cells = parse_cells(tb);
        let texts: Vec<String> = cells.iter().map(|c| c.text.clone()).collect();
        let Some((sample, coverage)) = garbled_score(&texts) else {
            continue;
        };
        if coverage >= COVERAGE_THRESHOLD {
            continue;
        }
        log(&format!(
            "重转写层：{} 词典覆盖率 {coverage:.2}（样本 {sample} 字）判为乱码表",
            r.id
        ));
        let Some(img_path) = r.item.img_path().filter(|p| !p.is_empty()) else {
            log(&format!(
                "重转写层：{} 无 img_path，搁置（漏修不误修）",
                r.id
            ));
            continue;
        };
        let render_len: usize = cells.iter().map(|c| c.text.chars().count() + 8).sum();
        if render_len > MAX_RENDER_CHARS {
            log(&format!(
                "重转写层：{} 渲染体积 {render_len} 超上限 {MAX_RENDER_CHARS}，搁置",
                r.id
            ));
            continue;
        }
        targets.push(Target {
            item_idx,
            img_path: img_path.to_string(),
            cells,
            coverage,
        });
    }
    if targets.is_empty() {
        return (items, outcome);
    }
    log(&format!("重转写层：{} 张乱码表送视觉重转写", targets.len()));

    // 2. 取图 + 视觉重转写（并发，失败搁置单表）
    let futs: Vec<_> = targets
        .iter()
        .map(|t| {
            let vision = vision.clone();
            let load_image = load_image.clone();
            async move {
                let Some(img) = load_image.load(&t.img_path).await else {
                    return Err(crate::llm::LlmError(format!(
                        "取不到表格截图 {}",
                        t.img_path
                    )));
                };
                vision.transcribe_table(&img, &render_cells(&t.cells)).await
            }
        })
        .collect();
    let results: Vec<_> = futures::stream::iter(futs)
        .buffered(concurrency.max(1))
        .collect()
        .await;

    // 3. 闸门 + 落地（逐表独立：一表失败不影响他表）
    for (t, result) in targets.iter().zip(results) {
        let id = items[t.item_idx].id.clone();
        let transcription = match result {
            Ok(tr) => tr,
            Err(e) => {
                log(&format!("重转写层：{id} 视觉重转写失败，搁置该表: {e}"));
                continue;
            }
        };
        outcome.usage.prompt += transcription.usage.prompt_tokens;
        outcome.usage.completion += transcription.usage.completion_tokens;
        if transcription.invalid > 0 {
            outcome.rejected += transcription.invalid;
            log(&format!(
                "重转写层：{id} 有 {} 条结构非法提案被解析期拒绝",
                transcription.invalid
            ));
        }

        // 闸门 1（结构）：行列命中现存单元格、文本干净；同格重复提案只认第一条
        let mut proposed: Vec<(usize, String)> = Vec::new(); // (cells 下标, 新文本)
        let mut taken: HashSet<usize> = HashSet::new();
        for p in &transcription.cells {
            let Some(ci) = t
                .cells
                .iter()
                .position(|c| c.row == p.row && c.col == p.col)
            else {
                outcome.rejected += 1;
                log(&format!(
                    "重转写层：{id} 提案指向不存在的单元格 r{}c{}，拒绝",
                    p.row, p.col
                ));
                continue;
            };
            let original = t.cells[ci].text.trim();
            if !rewritable(original) {
                outcome.rejected += 1;
                log(&format!(
                    "重转写层：{id} r{}c{} 原格不可重转写（空/纯数值/编号/正常内容），拒绝",
                    p.row, p.col
                ));
                continue;
            }
            // 多行格的换行归一为空格（HTML 单元格内的换行本就无语义）
            let text = p.text.replace(['\r', '\n'], " ").trim().to_string();
            if !structurally_valid(&text) {
                outcome.rejected += 1;
                log(&format!(
                    "重转写层：{id} r{}c{} 提案文本含标签/控制字符或超长，拒绝",
                    p.row, p.col
                ));
                continue;
            }
            if is_numeric_like(&text) {
                outcome.rejected += 1;
                log(&format!(
                    "重转写层：{id} r{}c{} 提案把文字格改成纯数值——行列错位特征，拒绝",
                    p.row, p.col
                ));
                continue;
            }
            if !length_ratio_ok(original.chars().count(), text.chars().count()) {
                outcome.rejected += 1;
                log(&format!(
                    "重转写层：{id} r{}c{} 提案长度量级与原格不可比——行列错位特征，拒绝",
                    p.row, p.col
                ));
                continue;
            }
            if !taken.insert(ci) {
                outcome.rejected += 1;
                log(&format!(
                    "重转写层：{id} r{}c{} 重复提案，拒绝",
                    p.row, p.col
                ));
                continue;
            }
            if escape_cell_text(&text) == t.cells[ci].text.trim() {
                continue; // 无变化 = 无操作，静默丢弃
            }
            proposed.push((ci, text));
        }
        if proposed.is_empty() {
            log(&format!("重转写层：{id} 无可落地提案，原样保留"));
            continue;
        }

        // 闸门 2（整表回归）：重转写后的词典覆盖率必须严格高于重转写前
        let new_texts: Vec<String> = t
            .cells
            .iter()
            .enumerate()
            .map(|(ci, c)| {
                proposed
                    .iter()
                    .find(|(pi, _)| *pi == ci)
                    .map(|(_, text)| text.clone())
                    .unwrap_or_else(|| c.text.clone())
            })
            .collect();
        let new_coverage = garbled_score(&new_texts).map(|(_, cov)| cov).unwrap_or(0.0);
        if new_coverage <= t.coverage {
            outcome.rejected += proposed.len() as u64;
            log(&format!(
                "重转写层：{id} 重转写后覆盖率 {new_coverage:.2} 未高于原 {:.2}，整表回退",
                t.coverage
            ));
            continue;
        }

        // 落地：按文档序拼接新 table_body，同步记录新串中的字符区间
        proposed.sort_by_key(|(ci, _)| t.cells[*ci].start);
        let body_chars: Vec<char> = items[t.item_idx]
            .item
            .table_body()
            .expect("重转写层内部错误：目标 item 丢了 table_body")
            .chars()
            .collect();
        let mut out: Vec<char> = Vec::new();
        let mut cursor = 0usize;
        for (ci, text) in &proposed {
            let cell = &t.cells[*ci];
            out.extend(&body_chars[cursor..cell.start]);
            let escaped = escape_cell_text(text);
            let char_start = out.len();
            out.extend(escaped.chars());
            outcome.fixes.push(TableCellRewrite {
                item_id: id.clone(),
                row: cell.row,
                col: cell.col,
                before: body_chars[cell.start..cell.end].iter().collect(),
                after: escaped,
                char_start,
                char_end: out.len(),
            });
            cursor = cell.end;
        }
        out.extend(&body_chars[cursor..]);
        items[t.item_idx]
            .item
            .set("table_body", json!(out.iter().collect::<String>()));
        log(&format!(
            "重转写层：{id} 落地 {} 格重转写，覆盖率 {:.2} → {new_coverage:.2}",
            proposed.len(),
            t.coverage
        ));
    }

    (items, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_loads() {
        assert!(CN_WORDS.len() > 50_000);
        assert!(CN_WORDS.contains("数据"));
        assert!(CN_WORDS.contains("目标"));
        assert!(!CN_WORDS.contains("来酒"));
    }

    #[test]
    fn coverage_separates_garbled_from_clean() {
        let clean = ["指标名称", "数据来源", "比较方式", "测试合格率不低于目标值"];
        let (_, cov) = word_coverage(clean.iter().copied());
        let total: usize = 4 + 4 + 4 + 11;
        assert!(
            cov as f64 / total as f64 > 0.7,
            "正常文本覆盖率应高: {cov}/{total}"
        );

        let garbled = [
            "代格",
            "目择值",
            "数据来酒",
            "数更来潭",
            "道术率系计",
            "境开絕螺斯试",
            "旻意项过充理",
        ];
        let (tot, cov) = word_coverage(garbled.iter().copied());
        assert!(
            (cov as f64 / tot as f64) < 0.55,
            "乱码文本覆盖率应塌方: {cov}/{tot}"
        );
    }

    #[test]
    fn garbled_score_requires_sample() {
        assert!(garbled_score(&["代格".into()]).is_none()); // 样本不足不判
        let texts: Vec<String> = (0..10).map(|_| "战略管理".to_string()).collect();
        let (sample, cov) = garbled_score(&texts).unwrap();
        assert_eq!(sample, 40);
        assert!(cov > 0.9);
    }

    #[test]
    fn parse_cells_spans_and_text() {
        let tb = "<table><tr><td rowspan=1>甲A</td><td></td></tr><tr><th>乙<br>丙&amp;丁</th></tr></table>";
        let cells = parse_cells(tb);
        assert_eq!(cells.len(), 3);
        assert_eq!(
            (cells[0].row, cells[0].col, cells[0].text.as_str()),
            (0, 0, "甲A")
        );
        assert_eq!(
            (cells[1].row, cells[1].col, cells[1].text.as_str()),
            (0, 1, "")
        );
        assert_eq!((cells[2].row, cells[2].col), (1, 0));
        assert_eq!(cells[2].text, "乙丙&amp;丁"); // <br> 剥掉、实体保留
        // span 是内层 HTML 区间：写回 before 即可还原
        let chars: Vec<char> = tb.chars().collect();
        let inner: String = chars[cells[2].start..cells[2].end].iter().collect();
        assert_eq!(inner, "乙<br>丙&amp;丁");
    }

    #[test]
    fn parse_cells_tolerates_malformed() {
        // 缺 </td>、缺 <tr>、末尾未闭合——都不 panic
        let cells = parse_cells("<table><td>甲<td>乙</table>");
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].text, "甲");
        assert_eq!(cells[1].text, "乙");
        let cells = parse_cells("<table><tr><td>未闭合");
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].text, "未闭合");
    }

    #[test]
    fn structural_gate() {
        assert!(structurally_valid("正常文本 with Michael 81.36%"));
        assert!(structurally_valid("多行\t制表符可以"));
        assert!(!structurally_valid("<script>"));
        assert!(!structurally_valid("换\n行"));
        assert!(!structurally_valid(&"长".repeat(257)));
    }

    #[test]
    fn render_cells_marks_rows_empties_and_rewritable() {
        let cells = parse_cells(
            "<table><tr><td>数据来酒</td><td></td></tr><tr><td>提交方式</td></tr></table>",
        );
        let r = render_cells(&cells);
        // 数据来酒（乱码）标 ✎；空格与正常词不标
        assert_eq!(
            r,
            "第0行：[0✎]「数据来酒」 [1]〈空〉 \n第1行：[0]「提交方式」 "
        );
    }

    #[test]
    fn rewritable_gate_cases() {
        // 乱码证据充分 → 可重转写
        assert!(rewritable("数据来酒"));
        assert!(rewritable("代格"));
        assert!(rewritable("楼心"));
        assert!(rewritable("眼")); // 孤立单字 = 乱码弹片
        assert!(rewritable("Midhuel")); // 词样拉丁 token
        assert!(rewritable("热表测试光理合格军不低于")); // 局部乱码长句
        // 无乱码证据 → 不许动
        assert!(!rewritable("")); // 空格
        assert!(!rewritable("  ")); // 空白格
        assert!(!rewritable("79.41%")); // 纯数值
        assert!(!rewritable("（2024~2025）")); // 数值+区间符
        assert!(!rewritable("G1.4")); // 短拉丁编号（留给混淆层）
        assert!(!rewritable("B1.36%"));
        assert!(!rewritable("提交方式")); // 词覆盖率正常
        assert!(!rewritable("测试合格率不低于目标值"));
    }

    #[test]
    fn misalignment_gates() {
        assert!(is_numeric_like("84.1%"));
        assert!(is_numeric_like("73.73"));
        assert!(!is_numeric_like("OK"));
        assert!(!is_numeric_like("核心"));
        // 「当年累计数据…」(20字)→「Michael」(7字)：量级不可比
        assert!(!length_ratio_ok(20, 7));
        assert!(!length_ratio_ok(1, 6));
        assert!(length_ratio_ok(2, 2));
        assert!(length_ratio_ok(3, 4));
        assert!(length_ratio_ok(7, 7));
    }
}
