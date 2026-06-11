# mineru-refine 开发入口。`just --list` 看全部。

default:
    @just --list

# 全量测试（全程 mock LLM，不打网络）
test:
    cargo test -p mineru-refine

# 静态检查
check:
    cargo clippy --workspace --all-targets --features bin -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

# CLI / HTTP server
build:
    cargo build -p mineru-refine --features bin --release

server:
    cargo run -p mineru-refine --features bin --release --bin mineru-refine-server

# 冒烟：验真实 Qwen-VL 判表（三对真实表格图，需 QWEN_APIKEY 与 test_data 产物）
smoke-vl:
    cargo run -p mineru-refine --features bin --example qwen_smoke

# 真实数据工作流：MinerU 产物 → refine（真 LLM）→ test_data/refined/<stem>/
refine-real *args:
    cargo run -p mineru-refine --features bin --release --example refine_real -- {{args}}

# 把 test_data/source/ 下的 PDF/DOC 交 MinerU 官方 API 解析（需 MINERU_API_TOKEN）
mineru-fetch *args:
    bun run scripts/mineru_fetch.ts {{args}}

# Python 绑定：构建 wheel 并装进 bindings/python/.venv
py-dev:
    cd bindings/python && uv venv .venv --allow-existing && uv pip install --python .venv/bin/python maturin && .venv/bin/maturin build --release -o dist && uv pip install --python .venv/bin/python --reinstall dist/*.whl

# JS 绑定：构建原生模块 + 冒烟
js-build:
    cd bindings/js && bun install && bun run build

js-test:
    cd bindings/js && bun run test

# ── 发布 ──────────────────────────────────────────────────
# 三个生态各自独立发布；版本号先 `just bump <semver>` 一把梭。
# 凭证：crates.io 用 `cargo login`；PyPI 用 MATURIN_PYPI_TOKEN（或 ~/.pypirc）；npm 用 `npm login`。

# 统一改版本：workspace Cargo.toml + Cargo.lock + bindings/js 主包与平台子包
bump version:
    python3 scripts/bump_version.py {{version}}

# 发布前体检：测试 + lint + 三端构建都得绿
publish-check:
    just test
    just check
    just js-build && just js-test
    cargo publish -p mineru-refine --dry-run --allow-dirty

# crates.io。仅首发用——Trusted Publishing 不支持新建 crate；首发后配好 crates.io 的
# Trusted Publisher，之后打 tag 由 CI（rust-release.yml）自动发布。
publish-rust:
    cargo publish -p mineru-refine

# PyPI：当前平台 wheel + sdist（其它平台 pip 装 sdist 时本机编译；要预编译多平台 wheel 上 CI 矩阵）
publish-py:
    cd bindings/python && uv venv .venv --allow-existing && uv pip install --python .venv/bin/python maturin
    cd bindings/python && rm -rf dist && .venv/bin/maturin build --release -o dist && .venv/bin/maturin sdist -o dist
    cd bindings/python && .venv/bin/maturin upload dist/*

# npm：发布【本机能构建出的平台】子包 + 主包。
# linux 包在 linux 机器（或 CI）上跑同一条命令即可补发——npm 对缺失的 optionalDependencies 是容忍的。
publish-js:
    cd bindings/js && bun install && bunx napi build --platform --release
    cd bindings/js && bunx napi create-npm-dirs
    # 把本机构建出的 .node 拷进对应平台目录（napi artifacts 是 CI 场景的等价步骤）
    cd bindings/js && for f in *.node; do \
        p="${f#mineru-refine.}"; p="${p%.node}"; cp "$f" "npm/$p/"; done
    cd bindings/js && bunx napi pre-publish -t npm --skip-optional-publish   # 同步 optionalDependencies 版本，不发布
    cd bindings/js && for d in npm/*/; do \
        if ls "$d"*.node >/dev/null 2>&1; then (cd "$d" && npm publish --access public); \
        else echo "跳过 ${d}（本机没有该平台的 .node，去对应平台机器跑 just publish-js 补发）"; fi; done
    cd bindings/js && npm publish --access public

# 全家桶（按依赖顺序：crates.io → PyPI → npm）
publish-all: publish-check publish-rust publish-py publish-js
