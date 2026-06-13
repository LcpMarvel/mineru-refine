// CLI transport（备选，首选是 HTTP）：stdin 收 JSON、stdout 回 JSON，subprocess 调用。
// stdin 形如 { "items": [...], "sha256"?: "...", "maxIterations"?: n, "imageDir"?: "/abs/path",
//              "fixOcrConfusion"?: bool, "extraConfusionPairs"?: ["0D", ...],
//              "rewriteGarbledTables"?: bool, "degradeGarbledTables"?: bool }
// 或直接是 items 数组。imageDir 指向 MinerU 产物目录（含 images/），提供则启用视觉裁决。
// fixOcrConfusion 开启 OCR 字符混淆修正层；rewriteGarbledTables 开启重度乱码表的
// 视觉重转写层（需要 imageDir）；degradeGarbledTables 开启乱码表降级兜底
//（重转写救不回 → 整项降级为 image，纯机械）——均 opt-in，见 lib 文档。
//
// 跑：  cat content_list.json | mineru-refine

use mineru_refine::{MineruItem, RefineOptions, refine};
use serde_json::Value;
use std::io::Read;
use std::process::exit;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() || raw.trim().is_empty() {
        eprintln!("[mineru-refine] stdin 为空 — 需要 content_list JSON");
        exit(2);
    }

    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[mineru-refine] stdin 不是合法 JSON: {e}");
            exit(2);
        }
    };

    let (items_value, opts) = match parsed {
        Value::Array(_) => (parsed, RefineOptions::default()),
        Value::Object(ref obj) => {
            // 严格解析：含非字符串元素是配置错误，早抛不静默吞（与 HTTP server 的 serde 行为一致）
            let extra_confusion_pairs = match obj.get("extraConfusionPairs") {
                None => vec![],
                Some(v) => match serde_json::from_value::<Vec<String>>(v.clone()) {
                    Ok(pairs) => pairs,
                    Err(e) => {
                        eprintln!("[mineru-refine] extraConfusionPairs 必须是字符串数组: {e}");
                        exit(2);
                    }
                },
            };
            let opts = RefineOptions {
                sha256: obj
                    .get("sha256")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                max_iterations: obj.get("maxIterations").and_then(Value::as_u64),
                image_dir: obj.get("imageDir").and_then(Value::as_str).map(Into::into),
                fix_ocr_confusion: obj
                    .get("fixOcrConfusion")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                extra_confusion_pairs,
                rewrite_garbled_tables: obj
                    .get("rewriteGarbledTables")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                degrade_garbled_tables: obj
                    .get("degradeGarbledTables")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                ..RefineOptions::default()
            };
            (obj.get("items").cloned().unwrap_or(Value::Null), opts)
        }
        _ => (Value::Null, RefineOptions::default()),
    };

    let items: Vec<MineruItem> = match serde_json::from_value(items_value) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[mineru-refine] items 必须是数组（content_list）: {e}");
            exit(2);
        }
    };

    let result = refine(items, opts).await;
    println!(
        "{}",
        serde_json::to_string(&result).expect("RefineResult 序列化不可能失败")
    );
}
