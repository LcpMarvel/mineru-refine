#!/usr/bin/env python3
"""统一改版本号：workspace Cargo.toml + bindings/js 主包与平台子包（npm/*）。

Python wheel 版本跟随 Cargo（pyproject dynamic version）；
js 平台子包与 optionalDependencies 在这里一并同步，保证仓库状态自洽，
发布时 napi pre-publish 对版本即为幂等 no-op。

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

# bindings/js/package.json（主包版本 + optionalDependencies 指向同版本的平台子包）
pkg_path = ROOT / "bindings/js/package.json"
pkg = json.loads(pkg_path.read_text())
pkg["version"] = version
for name in pkg["optionalDependencies"]:
    pkg["optionalDependencies"][name] = version
pkg_path.write_text(json.dumps(pkg, indent=2, ensure_ascii=False) + "\n")

# bindings/js/npm/*/package.json（平台子包）
npm_pkgs = sorted((ROOT / "bindings/js/npm").glob("*/package.json"))
if not npm_pkgs:
    sys.exit("bindings/js/npm 下没找到平台子包——目录结构变了？")
for sub_path in npm_pkgs:
    sub = json.loads(sub_path.read_text())
    sub["version"] = version
    sub_path.write_text(json.dumps(sub, indent=2, ensure_ascii=False) + "\n")

# Cargo.lock 跟上（否则 publish 的 --locked 校验会因脏 lock 报错）
subprocess.run(["cargo", "update", "--workspace", "--quiet"], cwd=ROOT, check=True)

print(f"✅ 版本已统一为 {version}（Cargo.toml / Cargo.lock / bindings/js 主包 + {len(npm_pkgs)} 个平台子包）")
print("   记得同步 REFINE_LOGIC_VERSION（crates/mineru-refine/src/refine.rs）——逻辑没变可不动，缓存 key 用它")
