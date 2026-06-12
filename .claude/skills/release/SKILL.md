---
name: release
description: 发布新版本：根据上一个 tag 以来的提交自动推导下一个版本号（feat→minor，否则 patch），bump 三端版本、体检、commit、打 tag 推到 GitHub，由 CI 自动发布 crates.io/PyPI/npm。当用户说"发布一个版本"、"发版"、"release"时使用。不监听 CI 结果。
---

# 发布版本：自动推版本号 → bump → 体检 → tag 推送

发布全靠 tag 驱动：推 `v*` tag 后，三条 GitHub Actions 流水线
（rust-release.yml / py-release.yml / js-release.yml）自动发布到
crates.io、PyPI、npm（均为 Trusted Publishing / CI 凭证，本地不需要任何 token）。
本 skill 只负责把 tag 正确地推出去，**不监听 CI 结果**。

## 步骤

### 1. 前置检查

- 工作区必须干净（`git status`），有未提交改动就停下来问用户。
- 当前分支必须是 master，且与 origin/master 同步（先 `git pull --ff-only`）。

### 2. 推导下一个版本号

- 上一个版本：`git tag --sort=-v:refname | head -1`（形如 `v0.9.0`）。
  同时核对 workspace `Cargo.toml` 的 `version` 与之一致；不一致先弄清原因。
- 看自上个 tag 以来的提交：`git log <last-tag>..HEAD --oneline`。
  - 没有任何提交 → 没东西可发，停。
  - 有 `feat` 提交 → **minor**（0.9.0 → 0.10.0）。
  - 只有 `fix` / `chore` / `docs` 等 → **patch**（0.9.0 → 0.9.1）。
  - major 永远不自动升，用户明说才升。
- 把推导结果（版本号 + 依据的提交列表）告诉用户，但不必等确认，直接继续。

### 3. bump 版本

```
just bump <semver>        # 统一 Cargo.toml / Cargo.lock / bindings/js 主包 + 平台子包
```

### 4. 同步 REFINE_LOGIC_VERSION（容易漏，重要）

`crates/mineru-refine/src/refine.rs` 里的 `REFINE_LOGIC_VERSION` 是**逻辑版本**，
独立于包版本，用作 LLM 结果缓存 key。判断标准：

- 自上个 tag 以来的提交**改变了 refine 产物**（新探测器、裁决逻辑、清洗规则、
  prompt 变更等）→ 必须升它（minor 递增即可），并在常量上方的注释里
  按既有格式补一行变更说明。
- 只是 CI / 文档 / 绑定层 / 重构不改产物 → 不动。
- prompt 变了还要看 `PROMPT_VERSION` 是否需要同步升。

### 5. 发布前体检

```
just publish-check        # 测试 + clippy + fmt --check + JS 构建/测试 + cargo publish dry-run
```

红了就停下来修，别打 tag。历史教训：v0.8.0 的 tag 打在没过 `cargo fmt --check`
的提交上，Rust Release 在 CI 的 Lint 步骤挂掉，crates.io 整个版本没发出去
（PyPI/npm 先成功了，造成三端版本错位）。体检全绿是打 tag 的硬前提。

### 6. commit + tag + push

```
git add -A
git commit -m "chore: bump vX.Y.Z"   # REFINE_LOGIC_VERSION 有动的话在 body 里说明
git tag vX.Y.Z
git push origin master vX.Y.Z
```

### 7. 收尾汇报（不监听 CI）

推完 tag 直接汇报，附上 Actions 页面链接让用户自己看进度：

- 版本号及推导依据（几个 feat / fix）
- REFINE_LOGIC_VERSION 是否升了、为什么
- https://github.com/LcpMarvel/mineru-refine/actions

不要轮询、不要后台监控 CI 运行状态。
