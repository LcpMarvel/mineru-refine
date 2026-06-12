// 乱码表视觉重转写冒烟：对真实文档跑机械检测 + Qwen-VL 逐格重转写（只跑该层，不跑核心 loop）。
// ZBZ-047 的已知乱码表必须被检出并修出「目标值/数据来源/Michael」级别的内容才算绿。
// 跑：  cargo run -p mineru-refine --example garbled_smoke --features bin   # .env 里需有 QWEN_APIKEY

use mineru_refine::garbled::{detect_garbled_table, rewrite_garbled_tables};
use mineru_refine::llm::{ImageDirLoader, QwenVlClient};
use mineru_refine::types::MineruItem;
use mineru_refine::{Logger, assign_ids};
use std::path::Path;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let dir = Path::new("test_data/mineru/MN-ZBZ-047_组织绩效管理规范");
    let raw = std::fs::read_to_string(dir.join("content_list.json"))
        .expect("需要本地 test_data（gitignored），在仓库根目录运行");
    let items: Vec<MineruItem> = serde_json::from_str(&raw).unwrap();

    for (i, it) in items.iter().enumerate() {
        if let Some(tb) = it.table_body()
            && let Some((sample, cov)) = detect_garbled_table(tb)
        {
            println!(
                "检出乱码表 item {i}（page {:?}）：覆盖率 {cov:.2}，样本 {sample} 字",
                it.page_idx()
            );
        }
    }

    let (ref_items, _) = assign_ids(&items);
    let vision = QwenVlClient::from_env().expect("QWEN_APIKEY 未设置");
    let loader = ImageDirLoader::new(dir);
    let log: Logger = Arc::new(|m: &str| eprintln!("[garbled_smoke] {m}"));

    let (fixed, outcome) = rewrite_garbled_tables(ref_items, vision, loader, 4, &log).await;
    println!(
        "\n落地 {} 格 | 拒绝 {} | tokens p={} c={}",
        outcome.fixes.len(),
        outcome.rejected,
        outcome.usage.prompt,
        outcome.usage.completion
    );
    for f in &outcome.fixes {
        println!("  r{}c{}: 「{}」→「{}」", f.row, f.col, f.before, f.after);
    }
    let _ = fixed;
    assert!(
        !outcome.fixes.is_empty(),
        "已知乱码表应至少修出一格——一格没修说明检测或重转写回归了"
    );
}
