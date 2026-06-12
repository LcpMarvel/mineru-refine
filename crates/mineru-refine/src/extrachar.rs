// 赘字/衍字共享判定件：纯删除，完全在核心层「只删不增」契约内。
// 不做机械删除——所有疑点统一进 LLM 裁决队列（extra_char 疑点 → deleteChar op），
// LLM 判不了就不改。两方共用同一口径：
//   - 探测器（detect.rs）：scan 产出疑点；
//   - deleteChar op（ops.rs）：validate_delete 是结构闸门——LLM 只能删本模块
//     认可的字符，删别的直接拒；成语保护构造性兜底。
//
// 两类病例（真实文档实证）：
//   1. 孤立偏旁部首：「3）亻」——部件形码位只作构字部件存在，正常文本不单独成字；
//   2. 功能词叠字：「基本治理理念的的变化情况」——的/地/是/了 的紧邻重复。
//      但「目的+的」「但是+是」「不甚了了」是合法语法，必须由 LLM 结合语境裁决。

/// 偏旁部首的"部件形"码位：只作构字部件，正常中文文本中不应单独成字。
/// 注意不收能独立成词的形近字（彳在「彳亍」里合法、卩厶有罕见用法——宁缺勿滥）。
pub const RADICAL_COMPONENTS: &[char] = &[
    '亻', '刂', '冫', '氵', '扌', '犭', '纟', '讠', '钅', '饣', '忄', '衤', '礻', '灬', '丬', '阝',
    '宀', '疒', '辶', '廴',
];

/// 高频功能词叠字白名单：仅这些字的紧邻重复会被视作疑似衍字。
pub const DUP_FUNCTION_CHARS: &[char] = &['的', '地', '是', '了'];

/// 合法叠词（成语/惯用语）：命中即不是衍字，连疑点都不报。
/// i 是叠字对第一个字的下标。
fn in_legit_reduplication(chars: &[char], i: usize) -> bool {
    let follow: [char; 2] = match chars[i] {
        '的' => ['确', '确'], // 的的确确
        '地' => ['道', '道'], // 地地道道
        '是' => ['非', '非'], // 是是非非
        _ => return false,
    };
    chars.len() > i + 3 && chars[i + 2] == follow[0] && chars[i + 3] == follow[1]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtraKind {
    /// 功能词叠字（删叠字对的第二个字）
    DupWord,
    /// 孤立偏旁部首
    Radical,
}

impl ExtraKind {
    pub fn reason(&self) -> &'static str {
        match self {
            ExtraKind::DupWord => "dup_char",
            ExtraKind::Radical => "radical",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ExtraHit {
    /// 建议删除的字符下标
    pub offset: usize,
    pub ch: char,
    pub kind: ExtraKind,
}

/// 扫描一段文本中的疑似赘字/衍字（产出疑点，不产出结论）。
pub fn scan(chars: &[char]) -> Vec<ExtraHit> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];

        // 孤立偏旁部首
        if RADICAL_COMPONENTS.contains(&c) {
            out.push(ExtraHit {
                offset: i,
                ch: c,
                kind: ExtraKind::Radical,
            });
            i += 1;
            continue;
        }

        // 功能词叠字：只报每段重复的第一对（删一个后若仍有剩余，下一轮重探测再看）
        if DUP_FUNCTION_CHARS.contains(&c)
            && i + 1 < chars.len()
            && chars[i + 1] == c
            && (i == 0 || chars[i - 1] != c)
            && !in_legit_reduplication(chars, i)
        {
            out.push(ExtraHit {
                offset: i + 1,
                ch: c,
                kind: ExtraKind::DupWord,
            });
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// deleteChar op 的结构闸门：offset 处的字符必须是本模块认可的衍字形态，
/// 成语保护构造性兜底（即便 LLM 误判也删不动「的的确确」）。
/// 通过返回 reason 标签（"dup_char" / "radical"）。
pub fn validate_delete(chars: &[char], offset: usize) -> Result<&'static str, String> {
    let Some(&c) = chars.get(offset) else {
        return Err(format!("offset {offset} 越界（文本长 {}）", chars.len()));
    };
    if RADICAL_COMPONENTS.contains(&c) {
        return Ok(ExtraKind::Radical.reason());
    }
    if DUP_FUNCTION_CHARS.contains(&c) {
        let dup_prev = offset > 0 && chars[offset - 1] == c;
        let dup_next = offset + 1 < chars.len() && chars[offset + 1] == c;
        if !dup_prev && !dup_next {
            return Err(format!(
                "「{c}」在 offset {offset} 处不与紧邻字符重复，不是叠字衍字"
            ));
        }
        // 成语保护：该字符所在的叠字对不许属于合法叠词
        let pair_start = if dup_prev { offset - 1 } else { offset };
        if in_legit_reduplication(chars, pair_start) {
            return Err(format!(
                "「{c}」属于合法叠词（的的确确/地地道道/是是非非），拒绝删除"
            ));
        }
        return Ok(ExtraKind::DupWord.reason());
    }
    Err(format!(
        "「{c}」不在 deleteChar 白名单内（仅限功能词叠字 的/地/是/了 或孤立偏旁部首）"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn dup_de_detected() {
        let hits = scan(&chars("基本治理理念的的变化情况"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 7);
        assert_eq!(hits[0].ch, '的');
        assert_eq!(hits[0].kind, ExtraKind::DupWord);
    }

    #[test]
    fn legit_reduplication_not_flagged() {
        assert!(scan(&chars("的的确确如此")).is_empty());
        assert!(scan(&chars("地地道道的本地人")).is_empty());
        assert!(scan(&chars("是是非非说不清")).is_empty());
    }

    #[test]
    fn ambiguous_dups_are_flagged_for_llm() {
        for s in ["确保目的的实现", "大地地价上涨", "但是是因为", "心里了了"]
        {
            assert_eq!(scan(&chars(s)).len(), 1, "{s} 应产生 1 个疑点");
        }
    }

    #[test]
    fn isolated_radical_detected() {
        let hits = scan(&chars("3）亻"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ch, '亻');
        assert_eq!(hits[0].kind, ExtraKind::Radical);
    }

    #[test]
    fn validate_delete_gates() {
        let c = chars("理念的的变化");
        assert_eq!(validate_delete(&c, 3), Ok("dup_char"));
        assert_eq!(validate_delete(&c, 2), Ok("dup_char")); // 对内任一字都行
        assert!(validate_delete(&c, 4).is_err()); // 变：不在白名单
        assert!(validate_delete(&c, 99).is_err()); // 越界

        let idiom = chars("的的确确");
        assert!(validate_delete(&idiom, 1).is_err(), "成语构造性保护");

        let rad = chars("3）亻");
        assert_eq!(validate_delete(&rad, 2), Ok("radical"));
    }

    #[test]
    fn triple_dup_reports_first_pair_only() {
        let hits = scan(&chars("理念的的的变化"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].offset, 3);
    }
}
