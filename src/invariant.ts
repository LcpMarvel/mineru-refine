// 保真不变式：C_out ⊆ C_in（非空白内容字符多重集），
// table_body 逐字节相等（多重集包含；mergeTable 产物降级为行级逐字节），几何可定位。
// 每个 op 后调用一次（违反→回滚），出口处对整篇再调用一次（违反→fail-open）。

import type { MineruItem, RefItem } from "./types.ts";

/** "内容字符" = text + list_items 拼接 + table_caption 拼接，仅计非空白字符。 */
export function contentChars(items: readonly (MineruItem | RefItem)[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const entry of items) {
    const item: MineruItem = "item" in entry ? (entry as RefItem).item : (entry as MineruItem);
    const parts: string[] = [];
    if (typeof item.text === "string") parts.push(item.text);
    if (Array.isArray(item.list_items)) parts.push(...item.list_items);
    if (Array.isArray(item.table_caption)) parts.push(...item.table_caption);
    for (const part of parts) {
      for (const ch of part) {
        if (/\s/.test(ch)) continue; // 空白符在可削减白名单内，不计
        counts.set(ch, (counts.get(ch) ?? 0) + 1);
      }
    }
  }
  return counts;
}

export type FidelityResult = { ok: true } | { ok: false; reason: string };

/** C_out ⊆ C_in：输出不得包含任何输入里没有的非空白内容字符。 */
export function checkCharSubset(
  before: readonly (MineruItem | RefItem)[],
  after: readonly (MineruItem | RefItem)[],
): FidelityResult {
  const cin = contentChars(before);
  const cout = contentChars(after);
  for (const [ch, n] of cout) {
    const avail = cin.get(ch) ?? 0;
    if (n > avail) {
      return {
        ok: false,
        reason: `C_out ⊄ C_in：字符 ${JSON.stringify(ch)} 输出 ${n} 次 > 输入 ${avail} 次`,
      };
    }
  }
  return { ok: true };
}

/** table_body 的 <tr>…</tr> 行序列（MinerU 表格不嵌套，非贪婪匹配安全）。 */
export function tableRows(body: string): string[] {
  return body.match(/<tr[\s\S]*?<\/tr>/gi) ?? [];
}

/** table_body 去掉所有行后的"外壳"（<table>/<tbody> 包装等行外字节）。 */
export function tableShell(body: string): string {
  return body.replace(/<tr[\s\S]*?<\/tr>/gi, "");
}

function takeFromPool(pool: Map<string, number>, key: string): boolean {
  const n = pool.get(key) ?? 0;
  if (n <= 0) return false;
  pool.set(key, n - 1);
  return true;
}

/**
 * 未被 drop 的 table_body 逐字节相等（多重集 ⊆）；唯一例外是 mergeTable 产物——
 * 它必须能被行级证明：每个 <tr> 行逐字节来自输入行池、行外"外壳"逐字节命中某个输入外壳
 * （即除"把若干输入行按原字节拼进某个输入表"外，没有任何字节被改动）。
 */
export function checkTableBodies(
  before: readonly (MineruItem | RefItem)[],
  after: readonly (MineruItem | RefItem)[],
): FidelityResult {
  const bodies = (entries: readonly (MineruItem | RefItem)[]) =>
    entries
      .map((e) => ("item" in e ? (e as RefItem).item : (e as MineruItem)).table_body)
      .filter((b): b is string => typeof b === "string");

  // 第一遍：整表逐字节撮合，被命中的输入表视为已消费
  const inputPool = new Map<string, number>();
  for (const b of bodies(before)) inputPool.set(b, (inputPool.get(b) ?? 0) + 1);
  const unmatched: string[] = [];
  for (const b of bodies(after)) {
    if (!takeFromPool(inputPool, b)) unmatched.push(b);
  }
  if (unmatched.length === 0) return { ok: true };

  // 第二遍（mergeTable 产物）：行/外壳池只从【未被消费】的输入表构建——
  // 防止同一输入行被"整表命中"和"行级命中"双重消费
  const rowPool = new Map<string, number>();
  const shellPool = new Map<string, number>();
  for (const [body, n] of inputPool) {
    for (let k = 0; k < n; k++) {
      for (const row of tableRows(body)) rowPool.set(row, (rowPool.get(row) ?? 0) + 1);
      const shell = tableShell(body);
      shellPool.set(shell, (shellPool.get(shell) ?? 0) + 1);
    }
  }
  for (const body of unmatched) {
    if (!takeFromPool(shellPool, tableShell(body))) {
      return { ok: false, reason: `table_body 被篡改：行外字节与所有输入表外壳都不符（前 80 字: ${body.slice(0, 80)}）` };
    }
    for (const row of tableRows(body)) {
      if (!takeFromPool(rowPool, row)) {
        return { ok: false, reason: `table_body 被篡改：输出中存在输入里没有的表格行（前 80 字: ${row.slice(0, 80)}）` };
      }
    }
  }
  return { ok: true };
}

/** 几何可定位（软检查的硬化版）：bbox 为 4 个有限数、page_idx 落在输入页集合内。 */
export function checkGeometry(
  after: readonly RefItem[],
  validPages: ReadonlySet<number>,
): FidelityResult {
  for (const { id, item } of after) {
    const b = item.bbox;
    if (!Array.isArray(b) || b.length !== 4 || !b.every((v) => typeof v === "number" && Number.isFinite(v))) {
      return { ok: false, reason: `几何失效：${id} 的 bbox 非法 (${JSON.stringify(b)})` };
    }
    if (typeof item.page_idx !== "number" || !validPages.has(item.page_idx)) {
      return { ok: false, reason: `几何失效：${id} 的 page_idx=${item.page_idx} 不在输入页范围内` };
    }
  }
  return { ok: true };
}

export function inputPages(items: readonly (MineruItem | RefItem)[]): Set<number> {
  const pages = new Set<number>();
  for (const entry of items) {
    const item: MineruItem = "item" in entry ? (entry as RefItem).item : (entry as MineruItem);
    if (typeof item.page_idx === "number") pages.add(item.page_idx);
  }
  return pages;
}

function hasValidGeometry(item: MineruItem): boolean {
  return (
    Array.isArray(item.bbox) &&
    item.bbox.length === 4 &&
    item.bbox.every((v) => typeof v === "number" && Number.isFinite(v)) &&
    typeof item.page_idx === "number"
  );
}

/**
 * 完整保真闸门：字符子集 + table_body + 几何，任一不过即 fail。
 * 几何检查仅在输入本身全量带几何信息时执行——某些 MinerU 版本的 content_list
 * 不含 bbox，此时强检几何会把所有 op 误判回滚。
 */
export function checkFidelity(
  before: readonly RefItem[],
  after: readonly RefItem[],
  validPages?: ReadonlySet<number>,
): FidelityResult {
  const chars = checkCharSubset(before, after);
  if (!chars.ok) return chars;
  const tables = checkTableBodies(before, after);
  if (!tables.ok) return tables;
  if (before.every((r) => hasValidGeometry(r.item))) {
    const geo = checkGeometry(after, validPages ?? inputPages(before));
    if (!geo.ok) return geo;
  }
  return { ok: true };
}
