#!/usr/bin/env python3
"""统一改版本号：workspace Cargo.toml + bindings/js/package.json。

Python wheel 版本跟随 Cargo（pyproject dynamic version）；
js 平台子包（npm/*）与 optionalDependencies 由 napi pre-publish 在发布时自动同步。

用法：  python3 scripts/bump_version.py 0.8.0
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+(-[\w.]+)?", sys.argv[1]):
    sys.exit(f"用法: {sys.argv[0]} <semver>（如 0.8.0）")
version = sys.argv[1]

# workspace Cargo.toml（只动 [workspace.package] 里的 version）
cargo_toml = ROOT / "Cargo.toml"
s = cargo_toml.read_text()
new, n = re.subn(
    r'(\[workspace\.package\]\nversion = ")[^"]+(")',
    rf"\g<1>{version}\g<2>",
    s,
    count=1,
)
if n != 1:
    sys.exit("没找到 [workspace.package] 的 version 字段——Cargo.toml 结构变了？")
cargo_toml.write_text(new)

# bindings/js/package.json
pkg_path = ROOT / "bindings/js/package.json"
pkg = json.loads(pkg_path.read_text())
pkg["version"] = version
pkg_path.write_text(json.dumps(pkg, indent=2, ensure_ascii=False) + "\n")

# Cargo.lock 跟上（否则 publish 的 --locked 校验会因脏 lock 报错）
subprocess.run(["cargo", "update", "--workspace", "--quiet"], cwd=ROOT, check=True)

print(f"✅ 版本已统一为 {version}（Cargo.toml / Cargo.lock / bindings/js/package.json）")
print("   记得同步 REFINE_LOGIC_VERSION（crates/mineru-refine/src/refine.rs）——逻辑没变可不动，缓存 key 用它")
