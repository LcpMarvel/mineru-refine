// 对真实 MinerU 解析产物（test_data/mineru/<stem>/content_list.json）跑 refine。
// 输出目录是 MinerU 产物目录的【完整替身】（drop-in）：images/、layout.json 等
// 原样镜像（content_list 里的 img_path 引用才不会断），仅替换 content_list.json 为清洗版，
// full.md 从清洗后 items 确定性重渲染，另附 refine_report.json。
//
// 跑：  cargo run -p mineru-refine --example refine_real --features bin           # 全部
//      cargo run -p mineru-refine --example refine_real --features bin -- <stem> # 只跑某个文档
//      REFINE_MAX_ITERATIONS=N 可显式覆盖外层循环上限
//      REFINE_FIX_CONFUSION=1 开启 OCR 字符混淆修正层（opt-in，直接替换）
//      REFINE_REWRITE_GARBLED=1 开启重度乱码表的视觉重转写层（opt-in，整格替换）

use mineru_refine::types::MineruItem;
use mineru_refine::{RefineOptions, detect_items, refine, render_markdown};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn suspect_stats(items: &[MineruItem]) -> BTreeMap<String, u64> {
    let mut stats: BTreeMap<String, u64> = BTreeMap::new();
    for w in detect_items(items) {
        let key = format!("{}{}", w.kind.as_str(), if w.has_op { "" } else { "*" });
        *stats.entry(key).or_insert(0) += 1;
    }
    stats
}

fn copy_dir_filtered(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name_s.ends_with("content_list.json") || name_s.ends_with("content_list_v2.json") {
            continue; // 清洗版稍后写入；带 UUID 前缀的原始副本不拷贝（避免混淆）
        }
        let to = dest.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir_filtered(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let mineru_dir = PathBuf::from("test_data/mineru");
    let out_dir = PathBuf::from("test_data/refined");

    let only = std::env::args().nth(1);
    let stems: Vec<String> = std::fs::read_dir(&mineru_dir)
        .expect("test_data/mineru/ 为空 — 先跑 mineru:fetch")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|d| !d.starts_with('.'))
        .collect();
    assert!(
        !stems.is_empty(),
        "test_data/mineru/ 为空 — 先跑 mineru:fetch"
    );
    let targets: Vec<&String> = match &only {
        Some(o) => stems.iter().filter(|s| *s == o).collect(),
        None => stems.iter().collect(),
    };
    assert!(
        !targets.is_empty(),
        "找不到文档 {only:?}，已有: {}",
        stems.join(", ")
    );

    for stem in targets {
        let doc_dir = mineru_dir.join(stem);
        let raw = std::fs::read_to_string(doc_dir.join("content_list.json"))
            .expect("content_list.json 读取失败");
        let items: Vec<MineruItem> =
            serde_json::from_str(&raw).expect("content_list.json 不是合法 JSON");
        println!("\n════ {stem} ════  ({} items)", items.len());
        println!(
            "输入疑点: {}  (* = 仅标记类，无 op)",
            serde_json::to_string(&suspect_stats(&items)).unwrap()
        );

        let t0 = Instant::now();
        let r = refine(
            items.clone(),
            RefineOptions {
                max_iterations: std::env::var("REFINE_MAX_ITERATIONS")
                    .ok()
                    .and_then(|v| v.parse().ok()),
                image_dir: Some(doc_dir.clone()), // split_table 走 Qwen-VL 视觉裁决
                fix_ocr_confusion: std::env::var("REFINE_FIX_CONFUSION")
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false),
                rewrite_garbled_tables: std::env::var("REFINE_REWRITE_GARBLED")
                    .map(|v| v == "1" || v == "true")
                    .unwrap_or(false),
                ..RefineOptions::default()
            },
        )
        .await;
        let secs = t0.elapsed().as_secs_f64();

        println!(
            "输出疑点: {}",
            serde_json::to_string(&suspect_stats(&r.items)).unwrap()
        );
        println!(
            "耗时 {secs:.1}s | items {}→{} | 迭代 {} | ops {} | dismissed {} | violations {} | failOpen {} | tokens p={} c={}",
            items.len(),
            r.items.len(),
            r.report.iterations,
            serde_json::to_string(&r.report.op_counts).unwrap(),
            r.report.dismissed,
            r.report.violations,
            r.report.fail_open,
            r.report.token_usage.prompt,
            r.report.token_usage.completion,
        );
        for s in &r.report.removed_spans {
            let head: String = s.text.chars().take(60).collect();
            println!("  删除 [{}] {}: 「{head}」", s.reason, s.item_id);
        }
        if !r.report.table_rewrites.is_empty() || r.report.table_rewrite_rejected > 0 {
            println!(
                "重转写层: 落地 {} | 拒绝 {}",
                r.report.table_rewrites.len(),
                r.report.table_rewrite_rejected,
            );
            for f in &r.report.table_rewrites {
                let before: String = f.before.chars().take(40).collect();
                let after: String = f.after.chars().take(40).collect();
                println!(
                    "  重转写 {} r{}c{}: 「{before}」→「{after}」",
                    f.item_id, f.row, f.col
                );
            }
        }
        if !r.report.confusion_fixes.is_empty() || r.report.confusion_rejected > 0 {
            println!(
                "混淆层: 落地 {} | 拒绝 {} | observations {}",
                r.report.confusion_fixes.len(),
                r.report.confusion_rejected,
                r.report.confusion_observations.len(),
            );
            for f in &r.report.confusion_fixes {
                println!(
                    "  替换 [{}] {} {}@{}: 「{}」→「{}」 {}",
                    f.source, f.item_id, f.field, f.char_offset, f.before, f.after, f.note
                );
            }
            for o in &r.report.confusion_observations {
                let head: String = o.chars().take(100).collect();
                println!("  观察: {head}");
            }
        }

        // 镜像整个 MinerU 产物目录（drop-in 替身），再写入清洗版 content_list.json
        let dest = out_dir.join(stem);
        let _ = std::fs::remove_dir_all(&dest);
        copy_dir_filtered(&doc_dir, &dest).expect("镜像产物目录失败");
        std::fs::write(
            dest.join("content_list.json"),
            serde_json::to_string_pretty(&r.items).unwrap(),
        )
        .unwrap();
        // full.md 从清洗后 items 确定性重渲染（与清洗版 content_list 保持一致；
        // 注意 MinerU 原版 full.md 本身就与其 content_list 有少量出入，以 content_list 为准）
        std::fs::write(dest.join("full.md"), render_markdown(&r.items)).unwrap();
        std::fs::write(
            dest.join("refine_report.json"),
            serde_json::to_string_pretty(&r.report).unwrap(),
        )
        .unwrap();
        let copied: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        println!("→ test_data/refined/{stem}/  [{}]", copied.join(", "));
    }
}
