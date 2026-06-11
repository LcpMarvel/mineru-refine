// 数据模型。MineruItem 镜像 MinerU content_list item 真实字段，
// 未知字段原样保留（schema 透明性）——底层就是 preserve_order 的 JSON 对象，
// 键序、未知字段与 JS 对象语义完全一致。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// MinerU content_list 的单个 item：保序 JSON 对象 + 类型化访问器。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MineruItem(pub Map<String, Value>);

impl MineruItem {
    pub fn item_type(&self) -> &str {
        self.0.get("type").and_then(Value::as_str).unwrap_or("")
    }

    pub fn text(&self) -> Option<&str> {
        self.0.get("text").and_then(Value::as_str)
    }

    pub fn text_level(&self) -> Option<i64> {
        self.0.get("text_level").and_then(Value::as_i64)
    }

    pub fn page_idx(&self) -> Option<i64> {
        self.0.get("page_idx").and_then(Value::as_i64)
    }

    pub fn table_body(&self) -> Option<&str> {
        self.0.get("table_body").and_then(Value::as_str)
    }

    pub fn img_path(&self) -> Option<&str> {
        self.0.get("img_path").and_then(Value::as_str)
    }

    /// 字符串数组字段（list_items / table_caption / …）：仅当字段是数组时返回，
    /// 数组内非字符串元素被忽略（真实数据全是字符串）。
    pub fn str_array(&self, key: &str) -> Option<Vec<&str>> {
        self.0
            .get(key)
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
    }

    /// 该字段是否为数组（不要求元素是字符串）。
    pub fn is_array(&self, key: &str) -> bool {
        self.0.get(key).map(Value::is_array).unwrap_or(false)
    }

    pub fn bbox(&self) -> Option<[f64; 4]> {
        let arr = self.0.get("bbox")?.as_array()?;
        if arr.len() != 4 {
            return None;
        }
        let mut out = [0.0; 4];
        for (i, v) in arr.iter().enumerate() {
            let n = v.as_f64()?;
            if !n.is_finite() {
                return None;
            }
            out[i] = n;
        }
        Some(out)
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.0.insert(key.to_string(), value);
    }

    pub fn remove(&mut self, key: &str) {
        self.0.shift_remove(key);
    }
}

/// 内部表示：item + 稳定 ID。ID 出口前剥除，绝不进输出 schema。
#[derive(Clone, Debug)]
pub struct RefItem {
    pub id: String,
    pub item: MineruItem,
}

/// MinerU 已正确分类的"页面家具"：不是 quirk、不进 worklist；
/// 跨页连续性判断/merge 时可跳过。
pub const PAGE_FURNITURE_TYPES: [&str; 3] = ["page_number", "header", "footer"];

pub fn is_page_furniture(item_type: &str) -> bool {
    PAGE_FURNITURE_TYPES.contains(&item_type)
}

// ── 探测器疑点 ──

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspectKind {
    // 可处理（有对应 op）
    PseudoHeading,
    CrossPageBreak,
    GiantBlock,
    PageArtifact,
    ResidualMarkup,
    /// 跨页两个有体表格 → mergeTable / dismiss
    SplitTable,
    /// 跨页相邻两个列表 → mergeList / dismiss
    SplitList,
    /// 零内容空壳表（MinerU 自行跨页合并后留下的占位）→ drop
    EmptyTable,
    /// 同级编号兄弟是标题而本块是正文（漏标标题）→ promote / dismiss
    MissedHeading,
    /// 段尾粘连了「[相关文件]」类节标记 → split / dismiss
    TrailingMarker,
    /// caption 与其表格之间隔了一个标题块（跨页/排版错序）→ reorder / dismiss
    SeparatedCaption,
    // 只标记、无 op（标记后不做处理）
    CaptionIssue,
}

impl SuspectKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SuspectKind::PseudoHeading => "pseudo_heading",
            SuspectKind::CrossPageBreak => "cross_page_break",
            SuspectKind::GiantBlock => "giant_block",
            SuspectKind::PageArtifact => "page_artifact",
            SuspectKind::ResidualMarkup => "residual_markup",
            SuspectKind::SplitTable => "split_table",
            SuspectKind::SplitList => "split_list",
            SuspectKind::EmptyTable => "empty_table",
            SuspectKind::MissedHeading => "missed_heading",
            SuspectKind::TrailingMarker => "trailing_marker",
            SuspectKind::SeparatedCaption => "separated_caption",
            SuspectKind::CaptionIssue => "caption_issue",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkItem {
    pub kind: SuspectKind,
    #[serde(rename = "itemId")]
    pub item_id: String,
    pub evidence: String,
    #[serde(rename = "hasOp")]
    pub has_op: bool,
}

// ── op 调用。参数一律稳定 ID，不用 index ──

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StripPattern {
    MdLink,
    LatexDollar,
    LatexBlock,
    LatexCommand,
    EscapedDollar,
    HtmlTag,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum OpCall {
    Merge {
        id_a: String,
        id_b: String,
    },
    Split {
        id: String,
        offset: i64,
    },
    Demote {
        id: String,
    },
    Promote {
        id: String,
        level: i64,
    },
    Reorder {
        ids_in_order: Vec<String>,
    },
    Drop {
        id: String,
    },
    Strip {
        id: String,
        pattern: StripPattern,
    },
    MergeTable {
        id_a: String,
        id_b: String,
    },
    MergeList {
        id_a: String,
        id_b: String,
        join_seam: Option<bool>,
    },
}

impl OpCall {
    pub fn op_name(&self) -> &'static str {
        match self {
            OpCall::Merge { .. } => "merge",
            OpCall::Split { .. } => "split",
            OpCall::Demote { .. } => "demote",
            OpCall::Promote { .. } => "promote",
            OpCall::Reorder { .. } => "reorder",
            OpCall::Drop { .. } => "drop",
            OpCall::Strip { .. } => "strip",
            OpCall::MergeTable { .. } => "mergeTable",
            OpCall::MergeList { .. } => "mergeList",
        }
    }
}

// ── provenance（纯削减模式不加字，恒为空，结构保留备用）──

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceEntry {
    pub item_id: String,
    pub field: String,
    pub char_start: usize,
    pub char_end: usize,
    pub origin: String,
    pub op: String,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemovedSpan {
    #[serde(rename = "itemId")]
    pub item_id: String,
    pub text: String,
    pub reason: String,
}

// ── 报告 ──

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefineReport {
    pub iterations: u64,
    /// BTreeMap：序列化键序确定（幂等输出逐字节可比）。
    pub op_counts: BTreeMap<String, u64>,
    pub dismissed: u64,
    pub removed_spans: Vec<RemovedSpan>,
    /// 保真闸回滚次数
    pub violations: u64,
    pub token_usage: TokenUsage,
    pub fail_open: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefineResult {
    pub items: Vec<MineruItem>,
    pub provenance: Vec<ProvenanceEntry>,
    pub report: RefineReport,
}
