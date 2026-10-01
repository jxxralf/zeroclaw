# ZeroClaw 贡献工作流 SOP

> fork：`jxxralf/zeroclaw` → 上游：`zeroclaw-labs/zeroclaw`（32.9k★，Rust，Apache-2.0/MIT 双许可）
> 本地环境：`/workspace/zeroclaw`（双 remote），rustc 1.98.0（MSRV 1.96）
> 更新日期：2026-10-01

---

## 1. 环境与通道（重要约束）

沙箱网络策略**拦截 GitHub 直连**（443/22 全封），所有流量走替代通道：

| 操作 | 通道 | 说明 |
|---|---|---|
| clone / fetch | `gh-proxy.com` 加速镜像 | 只读，速度约 8MB/s |
| push / 开 PR / 评论 | **GitHub MCP 连接器**（内部网关） | 带认证的写操作，在会话中执行 |
| 读上游 issue / PR / 文件 | GitHub MCP 或 WebFetch | 匿名公开信息 |

**原则：本地永远不直接 `git push`**（`origin` 的 push url 已故意置为无效值防误操作）。改动通过 MCP 的 push_files / create_branch / create_pull_request 推送。

## 2. 仓库布局

```
/workspace/zeroclaw          # 工作副本
  origin   = gh-proxy.com/.../jxxralf/zeroclaw.git      # fork（fetch 走镜像）
  upstream = gh-proxy.com/.../zeroclaw-labs/zeroclaw.git # 上游
/workspace/zeroclaw-dev/     # 本目录：SOP + 脚本（不进仓库，不污染 fork）
```

git 身份：`jxxralf <106021140+jxxralf@users.noreply.github.com>`（noreply 邮箱，满足上游隐私纪律：**禁止提交真实姓名/邮箱/PII**）。

## 3. 日常贡献流程（标准七步）

```bash
# ① 同步：拉上游并快进本地 master
/workspace/zeroclaw-dev/sync_upstream.sh --apply

# ② 切特性分支 —— 必须从 upstream/master 切（不是本地 master！）
cd /workspace/zeroclaw
git switch -c feat/<short-name> upstream/master
# 分支命名规范：feat/* 或 fix/*（上游 CONTRIBUTING 要求）

# ③ 开发（一 PR 一关注点，小 PR 优先：XS/S/M）

# ④ 本地 CI 守护（见 §4，全部通过才进入下一步）

# ⑤ 生成提交（conventional commits）
git add -p && git commit -m "feat(scope): 一句话说明"

# ⑥ 推送到 fork —— 由会话中的 MCP 通道执行（说出需求即可）：
#    push_files → fork 的同名分支

# ⑦ 开 PR 到上游 —— MCP create_pull_request：
#    head = jxxralf:feat/<short-name>，base = zeroclaw-labs:master
```

## 4. 本地 CI 守护（PR 前必须全绿）

上游 PR 是强门禁，本地先跑同等检查：

```bash
cd /workspace/zeroclaw
export PATH="$HOME/.cargo/bin:$PATH"

# 格式（必须零 diff）
cargo fmt --all -- --check

# Clippy — 快速模式（本地反馈）
cargo clippy --locked --workspace --exclude zeroclaw-desktop --all-targets -- -D clippy::correctness
# Clippy — 严格模式（与 CI 同面，PR 前必跑）
cargo clippy --locked --workspace --exclude zeroclaw-desktop --all-targets --features ci-all -- -D warnings

# 测试（--locked 必须）
cargo test --locked --workspace --exclude zeroclaw-desktop

# Rustdoc 门禁（CI 必过）
cargo doc --no-deps --workspace --exclude zeroclaw-desktop

# 一键质量门脚本（fmt + clippy correctness + provider dispatch gate）
./scripts/ci/rust_quality_gate.sh
```

项目还有大量专项 gate（`scripts/ci/` 下 40+ 个脚本），涉及 providers 架构改动时注意 **ProviderDispatch 门禁**：禁止绕过 `zeroclaw-providers/src/dispatch.rs` 直接调 `ModelProvider` 方法。

## 5. 上游 PR 规范速查（来自 CONTRIBUTING.md）

- **PR 只 target `master`**（`main` 不存在，提错直接被拒）
- **模板必填**：`.github/pull_request_template.md` 每节都要填
- **验证证据必须真实**：贴实际命令输出，不能写 "CI will check"
- **小 PR + 单一关注点**：不混 refactor/feature/infra；大工作拆 stacked PR
- **无 PII**：真实姓名、邮箱、token、公司信息一律不进代码/测试/fixture
- **无 AI 尾注**：不加 bot/AI attribution trailer；人类 `Co-authored-by` 可以
- **squash-merge + conventional commits**：`feat:/fix:/docs:/chore:/refactor:/test:`
- 提交即视为同意 CLA（双许可自动授权）
- 卡住就尽早开 draft PR 提问；review 反馈要及时响应（长期不响应会被关闭 PR，主意保留）
- 新手入口：上游 `good first issue` 标签

## 6. 同步策略：fork ↔ upstream

**核心认知：贡献工作流不依赖 fork/master 与上游同步。**
所有特性分支从 `upstream/master` 切出 → push 到 fork 的**特性分支** → PR。fork 的 master 落后不影响任何 PR 的正确性。

保持 fork/master 干净的意义：将来 rebase、对比、二分都简单。

| 场景 | 操作 |
|---|---|
| 日常 | `sync_upstream.sh`（查看）／`--apply`（本地快进） |
| GitHub 网页上 fork/master 落后 | 打开 fork 页面点 **Sync fork**（服务器端操作，无需本地推送） |
| fork/master 意外污染 | 网页 **Discard commits**，或让会话通过 MCP 重建分支指向 upstream/master |

**纪律：fork/master 永远不提交自己的改动**——所有工作进 `feat/*`/`fix/*` 分支。

## 7. 提交语义与影响力策略

目标是在上游建立长期影响力，节奏建议：

1. **起步期**：修 `good first issue`、文档/测试补充——熟悉 review 文化、让维护者认识你
2. **成长期**：中等 bug 修复 + 小特性，保持 PR 周 转（响应 review < 24h）
3. **影响期**：参与 RFC 讨论（`docs/book/src/contributing/rfcs.md`）、架构页反馈、认领 roadmap 项

每个 PR 的 body 结构（模板要求）：问题背景 → 改动说明 → 验证证据（命令+输出）→ 风险/回滚。

## 8. 常见故障

| 症状 | 处理 |
|---|---|
| fetch 超时 | gh-proxy.com 抖动，重试或换 ghproxy.net |
| cargo 下载依赖失败 | 走 rsproxy.cn sparse 镜像（~/.cargo/config.toml 已配） |
| rustup 装版本 404 | 用腾讯云镜像：`RUSTUP_DIST_SERVER=https://mirrors.cloud.tencent.com/rustup` |
| push 被拒/403 | 本来就不允许本地 push，改走 MCP 通道 |
| MCP 401/403 | 连接器授权过期 → CodeBuddy 设置 → 连接器 → 重新授权 GitHub |
