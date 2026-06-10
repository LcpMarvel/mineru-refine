// 异常探测器（SPEC §9）：确定性启发式 → worklist。产出"疑点"非"结论"，
// 由 LLM 在 loop 中裁决。判定逻辑借鉴 docfuse _SANITIZE_PASSES 的思想原型。

import { PAGE_FURNITURE_TYPES, type RefItem, type SuspectKind, type WorkItem } from "./types.ts";

const SENTENCE_END = /[。．.!！?？;；…]\s*$/;
// 列表项特征开头：-、•、①…⑳、(1)等。列表行天然无句末标点，跨页相邻也绝不能 merge（会把多个列表项粘成一行）
const BULLET_START = /^\s*[-–—•·●○◦▪‣*①-⑳]/;
// 标题/编号特征开头：第X章/节/条、1. / 1.2 / (一) / 一、 等
const HEADING_START =
  /^\s*(第\s*[0-9一二三四五六七八九十百]+\s*[章节条款部分篇]|[0-9]+(\.[0-9]+)*[.、\s]|[(（][0-9一二三四五六七八九十]+[)）]|[一二三四五六七八九十]+[、.])/;
const LEADING_NUMBERING = /^\s*(第\s*[0-9一二三四五六七八九十百]+\s*[章节条款部分篇]\s*|[0-9]+(\.[0-9]+)*[.、\s]+|[(（][0-9一二三四五六七八九十]+[)）]\s*|[一二三四五六七八九十]+[、.]\s*)/;

const GIANT_BLOCK_CHARS = 1200;
const ARTIFACT_MAX_CHARS = 30;
const ARTIFACT_MIN_REPEATS = 3;

const MD_LINK = /\[[^\]]*\]\([^)]*\)/;
// `\\[a-zA-Z]+\s*\{` 兜住 \mathsf { … } 这类去掉 $ 定界符后的命令残骸（真实数据踩过：
// strip:latex_dollar 只去定界符时，残骸不再命中关键词列表，形成探测盲区）
const LATEX =
  /\$[^$\n]+\$|\\(frac|sqrt|begin|end|cdot|times|alpha|beta|lambda|sum|mathsf|mathrm|mathbf|mathit|geqslant|leqslant|operatorname)\b|\\[a-zA-Z]+\s*\{/;
// 孤立的 \$ 转义（如「\$APPEALS」工具名）：成对 $ 的 LATEX 规则不命中，需单独探测
const ESCAPED_DOLLAR = /\\\$/;
// 只认已知 HTML 标签名：宽泛的 /<[^>]+>/ 会把正文里的「<MB-ZZ-155 部门OGSMT>」这类表单引用误判成标签（真实数据踩过）
const HTML_TAG =
  /<\/?(?:br|hr|b|i|u|s|em|strong|sub|sup|span|div|p|a|img|font|center|small|big|del|ins|mark|code|pre|table|tbody|thead|tr|td|th)(?:\s[^<>]*)?\/?>/i;

function nonWs(s: string): string {
  return s.replace(/\s+/g, "");
}

function nonWsLen(s: string): number {
  return nonWs(s).length;
}

function suspect(kind: SuspectKind, itemId: string, evidence: string, hasOp: boolean): WorkItem {
  return { kind, itemId, evidence, hasOp };
}

export function detect(items: readonly RefItem[]): WorkItem[] {
  const out: WorkItem[] = [];

  // 高频重复短文本（页眉/页脚/水印候选）：同文本出现在 ≥3 个不同页。
  // 注意：MinerU 的 type=header/footer/page_number 是【已正确分类】的页面家具，不是 quirk
  // （§9 针对的是被误标为 text 的混入正文），那些类型一律不进 worklist。
  const textPages = new Map<string, Set<number>>();
  for (const { item } of items) {
    if (item.type === "text" && typeof item.text === "string") {
      const key = item.text.trim();
      if (key.length > 0 && nonWsLen(key) <= ARTIFACT_MAX_CHARS) {
        if (!textPages.has(key)) textPages.set(key, new Set());
        if (typeof item.page_idx === "number") textPages.get(key)!.add(item.page_idx);
      }
    }
  }
  const repeatedTexts = new Set(
    [...textPages.entries()].filter(([, pages]) => pages.size >= ARTIFACT_MIN_REPEATS).map(([t]) => t),
  );

  // 家具同文佐证：已被 MinerU 正确分类的 header/footer/page_number 文本（≥2 处佐证）。
  // 同文却被标成 type=text 的就是漏网的跑马灯页眉/页脚——这类泄漏往往只出现 1~2 页
  // （其余页被正确分类），到不了 ARTIFACT_MIN_REPEATS 阈值，必须靠家具佐证抓
  // （真实数据：附件标题 8 处、公司名 3 处全因此漏网）。
  const furnitureCounts = new Map<string, number>();
  for (const { item } of items) {
    if (PAGE_FURNITURE_TYPES.has(item.type) && typeof item.text === "string") {
      const key = nonWs(item.text);
      if (key) furnitureCounts.set(key, (furnitureCounts.get(key) ?? 0) + 1);
    }
  }
  // 长的优先，处理「公司名 + 文档编号」拼成一个 text 块的泄漏形态
  const corroborated = [...furnitureCounts.entries()]
    .filter(([, n]) => n >= 2)
    .map(([t]) => t)
    .sort((x, y) => y.length - x.length);

  /** text 是否完全由已分类家具文本拼成；是则返回命中的家具文本列表。 */
  function furnitureLeak(text: string): string[] | null {
    let rest = nonWs(text);
    if (!rest || rest.length > 200) return null;
    const hits: string[] = [];
    for (const f of corroborated) {
      if (rest.includes(f)) {
        hits.push(f);
        rest = rest.split(f).join("");
      }
    }
    return hits.length > 0 && rest.length === 0 ? hits : null;
  }

  for (let i = 0; i < items.length; i++) {
    const { id, item } = items[i]!;
    const text = typeof item.text === "string" ? item.text : "";

    // ── 伪 HEADING（→ demote/merge）──
    if (item.type === "text" && item.text_level !== undefined && text) {
      const body = text.replace(LEADING_NUMBERING, "");
      const reasons: string[] = [];
      if (/[，,；;]/.test(text)) reasons.push("含逗号/分号");
      if (SENTENCE_END.test(text)) reasons.push("以句末标点收尾");
      if (nonWsLen(body) > 40) reasons.push(`去编号后正文过长(${nonWsLen(body)}字)`);
      if (reasons.length > 0) {
        out.push(suspect("pseudo_heading", id, `疑似伪标题: ${reasons.join("、")}。text=「${text.slice(0, 80)}」`, true));
      }
    }

    // ── 跨页断句（→ merge）。页边界处隔着 header/page_number 等页面家具，跳过它们找下一个内容块。──
    {
      let j = i + 1;
      while (j < items.length && PAGE_FURNITURE_TYPES.has(items[j]!.item.type)) j++;
      const next = items[j];
      const ntext = typeof next?.item.text === "string" ? next.item.text : "";
      if (
        next &&
        item.type === "text" &&
        next.item.type === "text" &&
        item.text_level === undefined &&
        next.item.text_level === undefined &&
        typeof item.page_idx === "number" &&
        typeof next.item.page_idx === "number" &&
        next.item.page_idx === item.page_idx + 1 &&
        text &&
        ntext &&
        !SENTENCE_END.test(text) &&
        !HEADING_START.test(ntext) &&
        !BULLET_START.test(text) && // 前块是列表项：行尾无标点是常态，不是断句
        !BULLET_START.test(ntext) // 后块是列表项：是新条目，不是前句的延续
      ) {
        out.push(
          suspect(
            "cross_page_break",
            id,
            `疑似跨页断句: 第${item.page_idx}页块尾「…${text.slice(-40)}」无句末标点，第${next.item.page_idx}页块首「${ntext.slice(0, 40)}…」非标题特征。后块=${next.id}`,
            true,
          ),
        );
      }
    }

    // ── 巨型块（→ split）──
    if (item.type === "text" && nonWsLen(text) > GIANT_BLOCK_CHARS) {
      const headingHits = text.match(/(?:^|\n)\s*(?:[0-9]+(?:\.[0-9]+)*[.、\s]|第\s*[0-9一二三四五六七八九十百]+\s*[章节条款])/g) ?? [];
      if (headingHits.length >= 2) {
        out.push(
          suspect("giant_block", id, `巨型块: ${nonWsLen(text)} 字且含 ${headingHits.length} 个疑似小标题编号`, true),
        );
      }
    }

    // ── 混入正文的页码/页眉页脚（→ drop）。仅看 type=text：已被 MinerU 正确分类的
    // page_number/header/footer 不是异常，消费方自会处理。──
    const leakHits = item.type === "text" ? furnitureLeak(text) : null;
    if (leakHits) {
      out.push(
        suspect(
          "page_artifact",
          id,
          `与已分类页眉/页脚同文（${leakHits.map((h) => `「${h}」×${furnitureCounts.get(h)}处家具佐证`).join("、")}），疑似漏标的页面家具。text=「${text.trim()}」`,
          true,
        ),
      );
    } else if (item.type === "text" && repeatedTexts.has(text.trim())) {
      out.push(
        suspect(
          "page_artifact",
          id,
          `高频重复短文本（出现于 ${textPages.get(text.trim())!.size} 个不同页），疑似页眉/页脚/水印。text=「${text.trim()}」`,
          true,
        ),
      );
    } else if (
      item.type === "text" &&
      item.text_level === undefined &&
      text.trim() &&
      nonWsLen(text) <= 10 &&
      /^[\s\d\-–—一二三四五六七八九十之/\\.()（）页第共]+$/.test(text.trim())
    ) {
      out.push(suspect("page_artifact", id, `疑似混入正文的页码: text=「${text.trim()}」`, true));
    }

    // ── 残留符号（→ strip）──
    if (item.type === "text" && text) {
      const hits: string[] = [];
      if (MD_LINK.test(text)) hits.push("markdown 链接");
      if (LATEX.test(text)) hits.push("LaTeX 残片");
      else if (ESCAPED_DOLLAR.test(text)) hits.push("孤立 \\$ 转义");
      if (HTML_TAG.test(text)) hits.push("HTML 标签");
      if (hits.length > 0) {
        out.push(suspect("residual_markup", id, `残留符号: ${hits.join("、")}。text=「${text.slice(0, 100)}」`, true));
      }
    }

    // ── 以下仅标记、无 op（D5）──
    if (item.type === "table" && i + 1 < items.length) {
      const next = items[i + 1]!;
      if (
        next.item.type === "table" &&
        typeof item.page_idx === "number" &&
        next.item.page_idx === item.page_idx + 1
      ) {
        out.push(suspect("split_table", id, `相邻两 table 跨页（${id} + ${next.id}），疑似同一表格被拆`, false));
      }
    }
    if (item.type === "list" && i + 1 < items.length) {
      const next = items[i + 1]!;
      if (
        next.item.type === "list" &&
        typeof item.page_idx === "number" &&
        next.item.page_idx === item.page_idx + 1
      ) {
        out.push(suspect("split_list", id, `相邻两 list 跨页（${id} + ${next.id}），疑似同一列表被拆`, false));
      }
    }
    if (item.type === "table" || item.type === "image") {
      const caption = item.type === "table" ? item.table_caption : (item as { img_caption?: string[] }).img_caption;
      if (!Array.isArray(caption) || caption.length === 0 || caption.every((c) => !c.trim())) {
        out.push(suspect("caption_issue", id, `${item.type} 无 caption 或 caption 为空`, false));
      }
    }
  }

  return out;
}

/** 当前被标为 page_artifact 的 id 集（drop 白名单的第二道保险）。 */
export function droppableIds(worklist: readonly WorkItem[]): Set<string> {
  return new Set(worklist.filter((w) => w.kind === "page_artifact").map((w) => w.itemId));
}
