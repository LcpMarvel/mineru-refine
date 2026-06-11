// 离线审计：对 content_list.json 跑机械清洗 + 探测器，打印疑点与清洗统计（不打 LLM）。
// 跑：  cargo run --example offline_audit -- /path/to/content_list.json

use mineru_refine::agent_loop::Logger;
use mineru_refine::detect::detect;
use mineru_refine::id::assign_ids;
use mineru_refine::mechanical::mechanical_clean;
use mineru_refine::types::MineruItem;
use std::sync::Arc;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("用法: offline_audit <content_list.json>");
    let raw = std::fs::read_to_string(&path).expect("读取失败");
    let items: Vec<MineruItem> = serde_json::from_str(&raw).expect("不是合法 content_list");

    let (mut ref_items, _) = assign_ids(&items);
    let log: Logger = Arc::new(|m: &str| eprintln!("[mech] {m}"));
    let mech = mechanical_clean(&mut ref_items, &log);

    println!("== 机械清洗 ==");
    for (k, v) in &mech.counts {
        println!("  {k}: {v}");
    }
    for s in &mech.removed_spans {
        println!("  span {} {} 「{}」", s.item_id, s.reason, s.text);
    }

    println!("== 疑点（机械清洗后）==");
    for w in detect(&ref_items) {
        println!("  [{}] {} {}", w.kind.as_str(), w.item_id, w.evidence);
    }
}
