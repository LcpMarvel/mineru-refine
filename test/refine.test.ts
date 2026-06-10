// eval 六件套 + fail-open + 缓存 + schema 透明性。LLM 全程 mock，不打真 API。

import { beforeEach, describe, expect, test } from "bun:test";
import { assignIds } from "../src/id.ts";
import { detect } from "../src/detect.ts";
import { checkCharSubset, checkTableBodies } from "../src/invariant.ts";
import { adaptiveMaxIterations, clearRefineCache, refine } from "../src/refine.ts";
import type { MineruItem } from "../src/types.ts";
import { bbox, explodingChat, goldenExpected, goldenInput, makeMockChat } from "./helpers.ts";

beforeEach(() => clearRefineCache());

describe("① golden fixtures", () => {
  test("原始 content_list → 期望清洗结果", async () => {
    const input = goldenInput();
    const r = await refine(input, { chatFn: makeMockChat() });
    expect(r.report.failOpen).toBe(false);
    expect(r.items).toEqual(goldenExpected());
    expect(r.report.opCounts).toEqual({ demote: 1, merge: 1, drop: 1, strip: 1 });
    expect(r.report.removedSpans.map((s) => s.reason).sort()).toEqual(["drop", "strip:md_link"]);
    expect(r.provenance).toEqual([]); // 纯削减模式下恒为空
  });
});

describe("② 保真不变式 C_out ⊆ C_in", () => {
  test("输出无新增非空白字符", async () => {
    const input = goldenInput();
    const r = await refine(input, { chatFn: makeMockChat() });
    expect(checkCharSubset(input, r.items).ok).toBe(true);
  });
});

describe("③ table_body 不变", () => {
  test("未被 drop 的 table_body 逐字节相等", async () => {
    const input = goldenInput();
    const r = await refine(input, { chatFn: makeMockChat() });
    expect(checkTableBodies(input, r.items).ok).toBe(true);
    const table = r.items.find((it) => it.type === "table");
    expect(table?.table_body).toBe(input.find((it) => it.type === "table")!.table_body!);
  });
});

describe("④ 异常数单调", () => {
  test("输出的有 op 异常数 ≤ 输入", async () => {
    const input = goldenInput();
    const before = detect(assignIds(input).ref).filter((w) => w.hasOp).length;
    const r = await refine(input, { chatFn: makeMockChat() });
    const after = detect(assignIds(r.items).ref).filter((w) => w.hasOp).length;
    expect(before).toBe(4);
    expect(after).toBe(0); // golden 文档应被清干净
  });
});

describe("⑤ 几何可定位", () => {
  test("每个输出 item bbox 非空且 page_idx 在输入页范围内", async () => {
    const input = goldenInput();
    const inPages = new Set(input.map((it) => it.page_idx));
    const r = await refine(input, { chatFn: makeMockChat() });
    for (const it of r.items) {
      expect(Array.isArray(it.bbox) && it.bbox.length === 4).toBe(true);
      expect(inPages.has(it.page_idx)).toBe(true);
    }
  });
});

describe("⑥ 幂等", () => {
  test("对清洗结果再跑一次是 no-op，且全程不打 LLM", async () => {
    const first = await refine(goldenInput(), { chatFn: makeMockChat() });
    const boom = explodingChat();
    const second = await refine(first.items, { chatFn: boom });
    expect(second.items).toEqual(first.items);
    expect(second.report.iterations).toBe(0);
    expect(boom.calls).toBe(0); // worklist 为空 → 一次 LLM 都不调
    expect(second.report.failOpen).toBe(false);
  });
});

describe("fail-open（异常时原样返回输入）", () => {
  test("LLM 不可用 → 原样返回输入 + failOpen=true", async () => {
    const input = goldenInput();
    const r = await refine(input, { chatFn: explodingChat(), log: () => {} });
    expect(r.report.failOpen).toBe(true);
    expect(r.items).toEqual(input);
  });

  test("LLM 给的 op 全被拒（参数瞎填）→ 疑点被搁置，照常出结果不崩", async () => {
    const mock = makeMockChat({
      pseudo_heading: () => ({ name: "demote", args: { id: "it_9999" } }), // 不存在的 ID
      cross_page_break: (id) => ({ name: "dismiss", args: { id, reason: "测试" } }),
      page_artifact: (id) => ({ name: "dismiss", args: { id, reason: "测试" } }),
      residual_markup: (id) => ({ name: "dismiss", args: { id, reason: "测试" } }),
    });
    const r = await refine(goldenInput(), { chatFn: mock, log: () => {} });
    expect(r.report.failOpen).toBe(false);
    expect(r.items).toEqual(goldenInput()); // 什么都没改
    expect(r.report.dismissed).toBe(4);
  });
});

describe("缓存（sha256 + 逻辑/模型/prompt 版本）", () => {
  test("同 sha256 第二次命中缓存，不再跑 loop", async () => {
    const mock1 = makeMockChat();
    const r1 = await refine(goldenInput(), { chatFn: mock1, sha256: "abc123" });
    expect(mock1.calls).toBeGreaterThan(0);

    const boom = explodingChat();
    const r2 = await refine(goldenInput(), { chatFn: boom, sha256: "abc123" });
    expect(boom.calls).toBe(0);
    expect(r2.items).toEqual(r1.items);

    // 不同 sha256 不命中
    const mock3 = makeMockChat();
    await refine(goldenInput(), { chatFn: mock3, sha256: "def456" });
    expect(mock3.calls).toBeGreaterThan(0);
  });
});

describe("schema 透明性", () => {
  test("输出不掺内部字段，未知字段原样透传，输入不被突变", async () => {
    const input: MineruItem[] = goldenInput();
    (input[0] as Record<string, unknown>).some_future_field = { x: 1 }; // MinerU 未来新增字段
    const frozen = structuredClone(input);
    const r = await refine(input, { chatFn: makeMockChat() });
    expect(input).toEqual(frozen); // 入参零突变
    for (const it of r.items) {
      expect("id" in it).toBe(false); // 内部稳定 ID 不外漏
    }
    expect((r.items[0] as Record<string, unknown>).some_future_field).toEqual({ x: 1 });
  });

  test("空输入 / 无疑点输入直接通过", async () => {
    const boom = explodingChat();
    expect((await refine([], { chatFn: boom })).items).toEqual([]);
    const clean: MineruItem[] = [{ type: "text", text: "干净的一句话。", page_idx: 0, bbox: bbox(0) }];
    const r = await refine(clean, { chatFn: boom });
    expect(r.items).toEqual(clean);
    expect(boom.calls).toBe(0);
  });
});

describe("守卫", () => {
  test("maxIterations 强停后仍走出口闸门", async () => {
    // mock 永远 dismiss 不掉：每次都给非法 op → 疑点被搁置进 dismissed 集，循环必然收敛
    const r = await refine(goldenInput(), { chatFn: makeMockChat(), maxIterations: 2, log: () => {} });
    expect(r.report.iterations).toBeLessThanOrEqual(2);
    expect(r.report.failOpen).toBe(false);
  });
});

describe("并发容错", () => {
  test("单个疑点 LLM 持续故障 → 仅搁置该疑点，其余照常修复", async () => {
    const mock = makeMockChat({
      // cross_page_break 的对话永远炸；其它 kind 正常裁决
      cross_page_break: () => {
        throw new Error("注入的单点故障");
      },
    });
    const r = await refine(goldenInput(), { chatFn: mock, log: () => {} });
    expect(r.report.failOpen).toBe(false);
    expect(r.report.opCounts).toEqual({ demote: 1, drop: 1, strip: 1 }); // merge 缺席
    expect(r.report.dismissed).toBe(1); // 故障疑点被搁置
  });
});

describe("跨页表格/列表（端到端，mock LLM）", () => {
  /** 真实形态复刻：前表(p0) + 家具 + 续表(p1) + 空壳(p2) + 拆开的 list(p2/p3)。 */
  function splitDocInput(): MineruItem[] {
    return [
      {
        type: "table",
        table_body: "<table><tbody><tr><td>序号</td><td>事项</td></tr><tr><td>1</td><td>启动</td></tr></tbody></table>",
        table_caption: ["表1 安排"],
        page_idx: 0,
        bbox: [50, 100, 550, 800],
      },
      { type: "page_number", text: "1", page_idx: 0, bbox: bbox(820) },
      { type: "header", text: "页眉", page_idx: 1, bbox: bbox(10) },
      {
        type: "table",
        table_body: "<table><tbody><tr><td>2</td><td>评审</td></tr></tbody></table>",
        table_caption: [],
        page_idx: 1,
        bbox: [50, 80, 550, 300],
      },
      { type: "table", img_path: "", table_caption: [], table_footnote: [], page_idx: 2, bbox: bbox(80) }, // 空壳
      { type: "list", list_items: ["甲", "乙"], page_idx: 2, bbox: [50, 200, 550, 800] },
      { type: "list", list_items: ["丙"], page_idx: 3, bbox: [50, 80, 550, 160] },
    ];
  }

  test("mergeTable + 空壳 drop + mergeList 全链路，行级保真闸过、出口闸过", async () => {
    const input = splitDocInput();
    const r = await refine(input, { chatFn: makeMockChat() });
    expect(r.report.failOpen).toBe(false);
    expect(r.report.opCounts).toEqual({ mergeTable: 1, drop: 1, mergeList: 1 });

    const tables = r.items.filter((it) => it.type === "table");
    expect(tables).toHaveLength(1);
    expect(tables[0]!.table_body).toBe(
      "<table><tbody><tr><td>序号</td><td>事项</td></tr><tr><td>1</td><td>启动</td></tr><tr><td>2</td><td>评审</td></tr></tbody></table>",
    );
    expect(tables[0]!.table_caption).toEqual(["表1 安排"]);
    expect(tables[0]!.page_idx).toBe(0);

    const lists = r.items.filter((it) => it.type === "list");
    expect(lists).toHaveLength(1);
    expect(lists[0]!.list_items).toEqual(["甲", "乙", "丙"]);

    // 家具原位保留；空壳 drop 留痕
    expect(r.items.filter((it) => it.type === "page_number" || it.type === "header")).toHaveLength(2);
    expect(r.report.removedSpans).toContainEqual({ itemId: "it_0005", text: "[table]", reason: "drop" });

    expect(checkCharSubset(input, r.items).ok).toBe(true);
    expect(checkTableBodies(input, r.items).ok).toBe(true);
  });

  test("幂等：清洗结果再跑一次是 no-op 且零 LLM 调用", async () => {
    const first = await refine(splitDocInput(), { chatFn: makeMockChat() });
    const second = makeMockChat();
    const r2 = await refine(first.items, { chatFn: second });
    expect(r2.items).toEqual(first.items);
    expect(r2.report.opCounts).toEqual({});
    expect(second.calls).toBe(0);
  });

  test("LLM 判两表是不同表 → dismiss，不动文档", async () => {
    const input = splitDocInput();
    const r = await refine(input, {
      chatFn: makeMockChat({
        split_table: (id) => ({ name: "dismiss", args: { id, reason: "两张不同的表" } }),
        split_list: (id) => ({ name: "dismiss", args: { id, reason: "两个独立列表" } }),
      }),
    });
    expect(r.report.failOpen).toBe(false);
    expect(r.report.opCounts).toEqual({ drop: 1 }); // 只剩空壳被删
    expect(r.items.filter((it) => it.type === "table")).toHaveLength(2);
    expect(r.report.dismissed).toBe(2);
  });
});

describe("split_table 视觉裁决（Qwen-VL mock）", () => {
  const PNG = new Uint8Array([1, 2, 3]);
  function visionDocInput(): MineruItem[] {
    return [
      {
        type: "table",
        table_body: "<table><tbody><tr><td>表头</td></tr><tr><td>甲</td></tr></tbody></table>",
        table_caption: ["表1"],
        img_path: "images/a.jpg",
        page_idx: 0,
        bbox: [50, 100, 550, 800],
      },
      { type: "page_number", text: "1", page_idx: 0, bbox: bbox(820) },
      {
        type: "table",
        table_body: "<table><tbody><tr><td>乙</td></tr></tbody></table>",
        table_caption: [],
        img_path: "images/b.jpg",
        page_idx: 1,
        bbox: [50, 80, 550, 300],
      },
    ];
  }
  const loadImage = async (p: string) => (p.startsWith("images/") ? PNG : null);
  /** split_table 不该落到文本路径时，文本 mock 一被调用就炸。 */
  const chatMustNotSeeSplitTable = () =>
    makeMockChat({
      split_table: () => {
        throw new Error("split_table 不应走文本路径");
      },
    });

  test("视觉判 merge → mergeTable 落地，token 计入 report，不走文本路径", async () => {
    let visionCalls = 0;
    const r = await refine(visionDocInput(), {
      chatFn: chatMustNotSeeSplitTable(),
      loadImage,
      visionFn: async (a, b) => {
        visionCalls++;
        expect(a).toBe(PNG);
        expect(b).toBe(PNG);
        return { verdict: "merge", reason: "同一张表", usage: { prompt_tokens: 1500, completion_tokens: 30 } };
      },
    });
    expect(visionCalls).toBe(1);
    expect(r.report.failOpen).toBe(false);
    expect(r.report.opCounts).toEqual({ mergeTable: 1 });
    expect(r.items.filter((it) => it.type === "table")).toHaveLength(1);
    expect(r.items[0]!.table_body).toContain("<tr><td>甲</td></tr><tr><td>乙</td></tr>");
    expect(r.report.tokenUsage.prompt).toBe(1500);
  });

  test("视觉判 dismiss → 不动文档，计入 dismissed", async () => {
    const r = await refine(visionDocInput(), {
      chatFn: chatMustNotSeeSplitTable(),
      loadImage,
      visionFn: async () => ({ verdict: "dismiss", reason: "两张不同的表", usage: { prompt_tokens: 1, completion_tokens: 1 } }),
    });
    expect(r.report.opCounts).toEqual({});
    expect(r.items.filter((it) => it.type === "table")).toHaveLength(2);
    expect(r.report.dismissed).toBe(1);
  });

  test("图取不到（loadImage 回 null）→ 回退文本路径", async () => {
    const chat = makeMockChat(); // 默认 split_table → mergeTable
    const r = await refine(visionDocInput(), {
      chatFn: chat,
      loadImage: async () => null,
      visionFn: async () => {
        throw new Error("不该被调用：图都没取到");
      },
    });
    expect(r.report.opCounts).toEqual({ mergeTable: 1 });
    expect(chat.calls).toBeGreaterThan(0);
  });

  test("视觉 API 故障 → 回退文本路径，不 fail-open", async () => {
    const chat = makeMockChat();
    const r = await refine(visionDocInput(), {
      chatFn: chat,
      loadImage,
      visionFn: async () => {
        throw new Error("Qwen-VL 不可用（测试注入）");
      },
    });
    expect(r.report.failOpen).toBe(false);
    expect(r.report.opCounts).toEqual({ mergeTable: 1 });
    expect(chat.calls).toBeGreaterThan(0);
  });
});

describe("maxIterations 自适应默认", () => {
  test("公式：min(max(48, 2N+16), 512)", () => {
    expect(adaptiveMaxIterations(0)).toBe(48);
    expect(adaptiveMaxIterations(16)).toBe(48);
    expect(adaptiveMaxIterations(17)).toBe(50);
    expect(adaptiveMaxIterations(60)).toBe(136); // JZY-001 实测 60 个初始疑点 → 136，足够其 ~100 的总工作量
    expect(adaptiveMaxIterations(300)).toBe(512); // 病态文档封顶
  });

  test("显式 maxIterations 优先于自适应", async () => {
    // golden 文档有 4 个疑点；maxIterations=1 应只裁 1 个就强停
    const chat = makeMockChat();
    const r = await refine(goldenInput(), { chatFn: chat, maxIterations: 1 });
    expect(r.report.iterations).toBe(1);
  });
});
