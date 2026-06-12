// napi-rs 绑定：Bun/Node 直接 import 原生插件。
// items 收/发普通 JS 对象（serde-json 桥接），refine 是真 async（napi tokio runtime）。

use mineru_refine_core::RefineOptions;
use mineru_refine_core::types::MineruItem;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::Value;

fn parse_items(items: Value) -> Result<Vec<MineruItem>> {
    serde_json::from_value(items)
        .map_err(|e| Error::from_reason(format!("items 必须是 content_list（对象数组）: {e}")))
}

#[napi(object)]
#[derive(Default)]
pub struct RefineOpts {
    /// 源文件 SHA256；提供时启用进程内缓存
    pub sha256: Option<String>,
    /// 外层循环硬上限；不传则自适应（随疑点数 48~512）
    pub max_iterations: Option<u32>,
    /// 疑点并行裁决数，默认 8；1 = 严格串行
    pub concurrency: Option<u32>,
    /// MinerU 产物目录绝对路径；提供则 split_table 启用 Qwen-VL 视觉裁决
    pub image_dir: Option<String>,
    /// OCR 字符混淆修正层（opt-in，默认关）。开启后输出契约变为：
    /// 核心层只删不增 + 混淆层在准入名单内做稀疏一换一替换（全量进 report.confusionFixes）
    pub fix_ocr_confusion: Option<bool>,
    /// 混淆准入名单补充对：每项恰好 2 个不同字符（如 "0D" 表示 0↔D 互换可直接落地）
    pub extra_confusion_pairs: Option<Vec<String>>,
    /// 重度乱码表的视觉重转写层（opt-in，默认关）。机械检测（词典覆盖率塌方）选定目标，
    /// Qwen-VL 对照 img_path 截图逐单元格重转写（全量进 report.tableRewrites，可程序化撤销）。
    /// 开启时必须提供 imageDir。
    pub rewrite_garbled_tables: Option<bool>,
}

/// MinerU content_list 清洗。fail-open：任何异常原样返回输入（report.failOpen=true）。
/// 返回 { items, provenance, report }，schema 与 TS/HTTP 版完全一致。
#[napi]
pub async fn refine(items: Value, opts: Option<RefineOpts>) -> Result<Value> {
    let items = parse_items(items)?;
    let opts = opts.unwrap_or_default();
    let result = mineru_refine_core::refine(
        items,
        RefineOptions {
            sha256: opts.sha256,
            max_iterations: opts.max_iterations.map(u64::from),
            concurrency: opts.concurrency.map(|c| c as usize),
            image_dir: opts.image_dir.map(Into::into),
            fix_ocr_confusion: opts.fix_ocr_confusion.unwrap_or(false),
            extra_confusion_pairs: opts.extra_confusion_pairs.unwrap_or_default(),
            rewrite_garbled_tables: opts.rewrite_garbled_tables.unwrap_or(false),
            ..RefineOptions::default()
        },
    )
    .await;
    serde_json::to_value(result).map_err(|e| Error::from_reason(e.to_string()))
}

/// items → full.md 文本（确定性重渲染，与 MinerU pipeline 拼接规则对齐）。
#[napi]
pub fn render_markdown(items: Value) -> Result<String> {
    Ok(mineru_refine_core::render_markdown(&parse_items(items)?))
}

/// 探测器独立调用：返回疑点列表（kind/itemId/evidence/hasOp），不打 LLM。
#[napi]
pub fn detect_suspects(items: Value) -> Result<Value> {
    let suspects = mineru_refine_core::detect_items(&parse_items(items)?);
    serde_json::to_value(suspects).map_err(|e| Error::from_reason(e.to_string()))
}

/// 测试/运维用：清空进程内缓存。
#[napi]
pub fn clear_refine_cache() {
    mineru_refine_core::clear_refine_cache();
}

#[napi]
pub const REFINE_LOGIC_VERSION: &str = mineru_refine_core::REFINE_LOGIC_VERSION;

#[napi]
pub const MODEL_ID: &str = mineru_refine_core::MODEL_ID;

#[napi]
pub const PROMPT_VERSION: &str = mineru_refine_core::PROMPT_VERSION;

#[napi]
pub const CONFUSION_PROMPT_VERSION: &str = mineru_refine_core::CONFUSION_PROMPT_VERSION;

#[napi]
pub const GARBLED_PROMPT_VERSION: &str = mineru_refine_core::GARBLED_PROMPT_VERSION;
