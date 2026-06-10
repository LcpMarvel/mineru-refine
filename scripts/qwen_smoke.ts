// Qwen-VL 冒烟：验 DashScope OpenAI 兼容端点 + 裸 fetch + 视觉判表地基。
// 拿三对真实表格裁剪图（两真续表 + 一假续表）问 qwen-vl-max，全判对才算绿。
// 跑：  bun run scripts/qwen_smoke.ts

const key = process.env.QWEN_APIKEY;
const baseUrl = process.env.QWEN_BASE_URL ?? "https://dashscope.aliyuncs.com/compatible-mode/v1";
const model = process.env.QWEN_VISION_MODEL ?? "qwen-vl-max";
if (!key) throw new Error("QWEN_APIKEY 未设置 — 在 .env 里填");

async function b64(path: string): Promise<string> {
  const buf = await Bun.file(path).arrayBuffer();
  return `data:image/jpeg;base64,${Buffer.from(buf).toString("base64")}`;
}

async function judge(imgA: string, imgB: string): Promise<{ verdict: string; reason: string; usage: unknown }> {
  const res = await fetch(`${baseUrl}/chat/completions`, {
    method: "POST",
    headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
    body: JSON.stringify({
      model,
      temperature: 0,
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: "图1是 PDF 某页末尾的表格，图2是紧接着的下一页开头的表格。判断图2是否是图1这张表被分页拆开的延续部分（看列网格是否同一套、切缝处内容/编号是否接续、图2有无自己独立的表头主题）。只输出 JSON：{\"verdict\":\"merge\"|\"dismiss\",\"reason\":\"一句话依据\"}，merge=同一张表的延续，dismiss=两张不同的表。" },
            { type: "image_url", image_url: { url: await b64(imgA) } },
            { type: "image_url", image_url: { url: await b64(imgB) } },
          ],
        },
      ],
    }),
  });
  if (!res.ok) throw new Error(`Qwen HTTP ${res.status}: ${(await res.text()).slice(0, 400)}`);
  const json: any = await res.json();
  const content: string = json.choices?.[0]?.message?.content ?? "";
  const m = content.match(/\{[\s\S]*\}/);
  if (!m) throw new Error(`Qwen 回复里没有 JSON: ${content.slice(0, 200)}`);
  return { ...JSON.parse(m[0]), usage: json.usage };
}

const CASES: { name: string; dir: string; a: string; b: string; expect: "merge" | "dismiss" }[] = [
  {
    name: "ZBZ-047 真续表（rowspan 跨页，5列vs4列）",
    dir: "test_data/mineru/MN-ZBZ-047_组织绩效管理规范",
    a: "images/57ee8ada9d34cdbd6260524ba1716b30907ce46ab378f4916fd88da56df4ed69.jpg",
    b: "images/f0d26bb13e2e52c67c775f120f53b76008130726810ccf1478f5f87ddd54cae2.jpg",
    expect: "merge",
  },
  {
    name: "JZY-001 真续表（6列vs6列，B 首格空）",
    dir: "test_data/mineru/MN-JZY-001_战略管理规范",
    a: "images/9b70c0b8e5d1b4bf0ab62d2a09ab7cccf751c61d50f6eb5a61913cdb69a55a96.jpg",
    b: "images/94f18746e42d15397f9dc3a1837d439605b81a66fbd9b255cf54866e8e75dae5.jpg",
    expect: "merge",
  },
  {
    name: "JZY-001 假续表（文控页两张不同的表，1列vs3列）",
    dir: "test_data/mineru/MN-JZY-001_战略管理规范",
    a: "images/da4d117cd13a7de850f6fbb08a00c19a7eb688dfd0dc4b05b1d2069cf76ec603.jpg",
    b: "images/d987f8176c4144308c913168763c1975882375457d8a1a2b07c81346ef9e1c13.jpg",
    expect: "dismiss",
  },
];

let failed = 0;
for (const c of CASES) {
  const r = await judge(`${c.dir}/${c.a}`, `${c.dir}/${c.b}`);
  const ok = r.verdict === c.expect;
  if (!ok) failed++;
  console.log(`${ok ? "✅" : "❌"} ${c.name}`);
  console.log(`   期望=${c.expect} 实际=${r.verdict} 依据=${r.reason}`);
  console.log(`   usage=${JSON.stringify(r.usage)}`);
}
if (failed > 0) {
  console.error(`\n${failed}/${CASES.length} 判错 — 不绿，别盖楼`);
  process.exit(1);
}
console.log("\n全绿：key 可用、裸 API 通、三对真实表格全判对。");
