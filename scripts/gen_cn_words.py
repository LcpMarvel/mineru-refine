#!/usr/bin/env python3
"""生成 crates/mineru-refine/data/cn_words.txt（乱码表检测用的常用词词典）。

来源：jieba 词典（https://github.com/fxsjy/jieba，MIT License）。
取 2~4 字纯汉字词，按词频降序取 top 60000。

用法：
    curl -so /tmp/jieba_dict.txt https://raw.githubusercontent.com/fxsjy/jieba/master/jieba/dict.txt
    python3 scripts/gen_cn_words.py /tmp/jieba_dict.txt

阈值标定（test_data 三份真实文档，2026-06）：最大匹配字符覆盖率下，
乱码表（ZBZ-047 item113）= 0.46，最差正常表（JZY-001 item290，满表股票代码与公司名）= 0.61。
"""

import sys

TOP_N = 60000


def is_hanzi(c: str) -> bool:
    return "一" <= c <= "鿿"


def main() -> None:
    src = sys.argv[1] if len(sys.argv) > 1 else "/tmp/jieba_dict.txt"
    entries = []
    for line in open(src, encoding="utf-8"):
        parts = line.split()
        if len(parts) >= 2:
            w, f = parts[0], int(parts[1])
            if 2 <= len(w) <= 4 and all(is_hanzi(c) for c in w):
                entries.append((f, w))
    entries.sort(key=lambda x: (-x[0], x[1]))
    words = [w for _, w in entries[:TOP_N]]

    out = "crates/mineru-refine/data/cn_words.txt"
    with open(out, "w", encoding="utf-8") as fh:
        fh.write("# 常用中文词（2~4 字，词频 top %d），乱码表检测的字典命中率用。\n" % TOP_N)
        fh.write("# 来源：jieba 词典（MIT License，https://github.com/fxsjy/jieba），\n")
        fh.write("# 由 scripts/gen_cn_words.py 生成，勿手改。\n")
        fh.write("\n".join(words))
        fh.write("\n")
    print(f"wrote {len(words)} words to {out}")


if __name__ == "__main__":
    main()
