// HTTP transport（首选）：POST /refine 收 content_list JSON，回 { items, provenance, report }。
// docfuse(Python) 在 _parse_extracted_dir 解析 content_list.json 之前调一次。
// fail-open 在 refine() 内已兜；transport 层再兜一层（坏请求 → 400，内部错 → 仍回原 items 不可能时 500）。
//
// 跑：  source ~/.ragent_profile && bun run src/server.ts

import { imageDirLoader, refine } from "./refine.ts";
import type { MineruItem } from "./types.ts";

const PORT = Number(process.env.MINERU_REFINE_PORT ?? 8771);

type RefineRequest = {
  items: MineruItem[];
  markdown?: string;
  sha256?: string;
  maxIterations?: number;
  /** MinerU 产物目录绝对路径（须与本服务共享文件系统）；提供则 split_table 启用 Qwen-VL 视觉裁决。 */
  imageDir?: string;
};

const server = Bun.serve({
  port: PORT,
  idleTimeout: 240, // LLM loop 可能跑几分钟
  async fetch(req) {
    const url = new URL(req.url);

    if (req.method === "GET" && url.pathname === "/health") {
      return Response.json({ ok: true, service: "mineru-refine" });
    }

    if (req.method === "POST" && url.pathname === "/refine") {
      let body: RefineRequest;
      try {
        body = (await req.json()) as RefineRequest;
      } catch {
        return Response.json({ error: "请求体不是合法 JSON" }, { status: 400 });
      }
      if (!Array.isArray(body.items)) {
        return Response.json({ error: "缺少 items 数组（MinerU content_list）" }, { status: 400 });
      }
      const result = await refine(body.items, {
        markdown: body.markdown,
        sha256: body.sha256,
        maxIterations: body.maxIterations,
        loadImage: body.imageDir ? imageDirLoader(body.imageDir) : undefined,
      });
      return Response.json(result);
    }

    return Response.json({ error: "not found" }, { status: 404 });
  },
});

console.error(`[mineru-refine] HTTP transport 启动: http://localhost:${server.port}  (POST /refine, GET /health)`);
