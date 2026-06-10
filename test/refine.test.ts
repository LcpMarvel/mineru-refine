// M5：eval 六件套（SPEC §13）+ fail-open + 缓存 + schema 透明性。LLM 全程 mock，不打真 API。

import { beforeEach, describe, expect, test } from "bun:test";
import { assignIds } from "../src/id.ts";
import { detect } from "../src/detect.ts";
import { checkCharSubset, checkTableBodies } from "../src/invariant.ts";
import { clearRefineCache, refine } from "../src/refine.ts";
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
    expect(r.provenance).toEqual([]); // D4=(c) 恒为空
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

describe("fail-open（§2 失败行为）", () => {
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

describe("缓存（§2：sha256 + 逻辑/模型/prompt 版本）", () => {
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

describe("schema 透明性（§2/§4a）", () => {
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

describe("守卫（§10）", () => {
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
