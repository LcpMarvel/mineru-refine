// 包公共入口：消费方 import { refine } from "mineru-refine"。
// 内部模块（loop/ops/deepseek/qwen_vl）不直接暴露，按需从这里 re-export。

export {
  refine,
  imageDirLoader,
  adaptiveMaxIterations,
  cacheKeyFor,
  clearRefineCache,
  REFINE_LOGIC_VERSION,
  PROMPT_VERSION,
  MODEL_ID,
  type RefineOptions,
} from "./refine.ts";

export type {
  MineruItem,
  RefineResult,
  RefineReport,
  RemovedSpan,
  ProvenanceEntry,
  WorkItem,
  SuspectKind,
  OpCall,
  OpName,
  StripPattern,
} from "./types.ts";

export type { LoadImageFn, ChatFn } from "./loop.ts";
export type { VisionJudgeFn, SplitTableVerdict } from "./qwen_vl.ts";

// 独立可用的工具件：探测器（疑点统计）与 full.md 确定性重渲染
export { detect, droppableIds } from "./detect.ts";
export { assignIds, stripIds } from "./id.ts";
export { renderMarkdown } from "./markdown.ts";
