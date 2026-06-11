// Qwen-VL 冒烟：验 DashScope OpenAI 兼容端点 + 裸 API + 视觉判表地基。
// 拿三对真实表格裁剪图（两真续表 + 一假续表）问 qwen-vl-max，全判对才算绿。
// 跑：  cargo run -p mineru-refine --example qwen_smoke    # .env 里需有 QWEN_APIKEY

use mineru_refine::llm::{QwenVlClient, VisionClient};
use std::path::Path;

struct Case {
    name: &'static str,
    dir: &'static str,
    a: &'static str,
    b: &'static str,
    expect_merge: bool,
}

const CASES: [Case; 3] = [
    Case {
        name: "ZBZ-047 真续表（rowspan 跨页，5列vs4列）",
        dir: "test_data/mineru/MN-ZBZ-047_组织绩效管理规范",
        a: "images/57ee8ada9d34cdbd6260524ba1716b30907ce46ab378f4916fd88da56df4ed69.jpg",
        b: "images/f0d26bb13e2e52c67c775f120f53b76008130726810ccf1478f5f87ddd54cae2.jpg",
        expect_merge: true,
    },
    Case {
        name: "JZY-001 真续表（6列vs6列，B 首格空）",
        dir: "test_data/mineru/MN-JZY-001_战略管理规范",
        a: "images/9b70c0b8e5d1b4bf0ab62d2a09ab7cccf751c61d50f6eb5a61913cdb69a55a96.jpg",
        b: "images/94f18746e42d15397f9dc3a1837d439605b81a66fbd9b255cf54866e8e75dae5.jpg",
        expect_merge: true,
    },
    Case {
        name: "JZY-001 假续表（文控页两张不同的表，1列vs3列）",
        dir: "test_data/mineru/MN-JZY-001_战略管理规范",
        a: "images/da4d117cd13a7de850f6fbb08a00c19a7eb688dfd0dc4b05b1d2069cf76ec603.jpg",
        b: "images/d987f8176c4144308c913168763c1975882375457d8a1a2b07c81346ef9e1c13.jpg",
        expect_merge: false,
    },
];

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let client = QwenVlClient::from_env().expect("QWEN_APIKEY 未设置 — 在 .env 里填");

    let mut failed = 0;
    for c in &CASES {
        let img_a = std::fs::read(Path::new(c.dir).join(c.a))
            .unwrap_or_else(|e| panic!("读图失败 {}/{}: {e}（先跑 mineru:fetch）", c.dir, c.a));
        let img_b = std::fs::read(Path::new(c.dir).join(c.b)).expect("读图失败");
        let v = client
            .judge_split_table(&img_a, &img_b)
            .await
            .expect("Qwen-VL 调用失败");
        let ok = v.merge == c.expect_merge;
        if !ok {
            failed += 1;
        }
        let fmt = |m: bool| if m { "merge" } else { "dismiss" };
        println!("{} {}", if ok { "✅" } else { "❌" }, c.name);
        println!(
            "   期望={} 实际={} 依据={}",
            fmt(c.expect_merge),
            fmt(v.merge),
            v.reason
        );
        println!(
            "   usage=p{} c{}",
            v.usage.prompt_tokens, v.usage.completion_tokens
        );
    }
    if failed > 0 {
        eprintln!("\n{failed}/{} 判错 — 不绿，别盖楼", CASES.len());
        std::process::exit(1);
    }
    println!("\n全绿：key 可用、裸 API 通、三对真实表格全判对。");
}
