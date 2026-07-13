# Codex Provider Hub

Codex Provider Hub 是一个基于 Tauri 2、Rust 和 React 的本地桌面工具。它用于修复 Codex 切换 Provider 后出现的会话可见性问题，不负责管理 Provider 密钥或模型配置。

工具默认只读。执行修复前会先创建两套 SQLite 的可回滚快照，随后只更新可信的本地会话状态和侧栏索引。rollout JSONL、`session_index.jsonl`、凭据和 Provider 配置不会被改写。

## 解决什么问题

切换 OpenAI、Custom 或 CodexPilot 后，历史会话可能还在磁盘上，却不再出现在 Codex 侧栏。常见原因不是 JSONL 丢失，而是几层本地状态没有对齐：

- `config.toml` 只记录当前根级 `model_provider`。
- `state_5.sqlite / threads` 仍保留会话原来的 Provider、归档和来源信息。
- `sqlite/codex-dev.db / local_thread_catalog` 决定本机侧栏是否有对应索引。
- rollout JSONL 中的 `session_meta` 可能保留旧 Provider；本工具只读取它，不修改它。

Codex Provider Hub 会按 thread ID 合并这些来源，先预览差异，再备份、同步和验证。修复成功表示本地状态和侧栏索引已经对齐，不表示上游侧栏会一次展示全部历史记录。Codex 自身仍可能只展示最近一部分会话。

## 快速使用

### 桌面端

1. 启动安装版或 `codex-provider-hub.exe`。
2. 等待工具自动发现并扫描 `CODEX_HOME`。
3. 查看可恢复会话、侧栏覆盖、待恢复和远端映射数量。
4. 选择目标 Provider，先保留“预览模式”。
5. 检查变更数、跳过原因、权限和进程锁状态。
6. 准备写入时，先退出 Codex、Codex++、Provider launcher 及相关进程。
7. 关闭预览模式，执行同步。写入目标必须与 `config.toml` 当前 Provider 一致。
8. 同步后执行验证。需要撤销时，使用“回滚最近一次”。

实际写入前会自动创建 SQLite 快照。界面的“创建 SQLite 快照”用于额外手动保护，不是执行修复的前置步骤。重复执行已经完成的修复应返回 `0 changes`，也不会创建无意义的新备份。

### CLI

桌面程序和 CLI 是两个独立入口。双击 `codex-provider-hub.exe` 只打开桌面窗口，不会附带控制台；自动化脚本使用 `codex-provider-hub-cli.exe`：

```powershell
codex-provider-hub-cli.exe scan
codex-provider-hub-cli.exe repair
codex-provider-hub-cli.exe repair --apply
codex-provider-hub-cli.exe verify
codex-provider-hub-cli.exe restore
```

常用参数：

```powershell
codex-provider-hub-cli.exe scan --codex-home C:\Users\me\.codex
codex-provider-hub-cli.exe repair --target-provider openai --dry-run
codex-provider-hub-cli.exe repair --target-provider openai --apply
codex-provider-hub-cli.exe verify --target-provider openai
codex-provider-hub-cli.exe restore backups\provider-hub\repair-20260713-120000-000
```

- `repair` 默认是 dry-run，只有 `--apply` 或 `--write` 才允许写入 SQLite。
- `--target-provider` 省略时读取 `config.toml` 的当前 Provider。
- `--codex-home` 省略时先读取 `CODEX_HOME` 环境变量，再使用 `%USERPROFILE%\.codex` 或 `$HOME/.codex`。
- `restore` 的备份路径必须位于 `CODEX_HOME/backups/provider-hub` 内。
- 成功结果以 JSON 写到 stdout，错误写到 stderr。成功退出码为 `0`，失败为 `1`。

## 数据来源和指标口径

这里没有一个文件可以单独代表“全部有效会话”。工具按 ID 合并来源，再用同一批 ID 计算覆盖率。

| 数据源 | 用途 | 写入策略 |
| --- | --- | --- |
| `state_5.sqlite / threads` | 线程登记、Provider、归档、来源和角色 | 仅更新通过安全筛选的普通本地线程 |
| `sessions/**/*.jsonl` | 活动会话内容和 `session_meta` | 只读 |
| `archived_sessions/**/*.jsonl` | 已归档会话内容 | 只读，默认不参与修复 |
| `sqlite/codex-dev.db / local_thread_catalog` | 本机及远端侧栏映射 | 只修改安全的 `host_id=local` 行 |
| `session_index.jsonl` | 辅助快速索引，可能滞后或不完整 | 只读诊断，不作为总量 |
| `.codex-global-state.json` | UI 和线程状态辅助信息 | 只读诊断 |

核心集合可以简化为：

```text
普通候选 T = 活动、可信 Provider、普通用户 threads
有效内容 R = sessions 中 ID 唯一、可读取且不含歧义的 rollout
本地索引 L = 非 missing_candidate 的本地 catalog
远端映射 N = 非本机 host 或带明确 SSH/WSL/容器标记的记录
可恢复 E   = (T ∩ R) - 远端排除项

侧栏覆盖率       = |E ∩ L| / |E|
session index覆盖 = |E ∩ session_index| / |E|，仅用于诊断
待恢复数量       = |E - L|
```

界面指标含义：

- `state` 是 `threads` 的唯一 ID 总数，包含归档、自动化和其他跳过项。
- `普通候选` 是经过 Provider、来源、角色和归档筛选后的活动线程。
- `可恢复会话` 还要求存在唯一且可读取的活动 rollout，并排除远端线程。
- `侧栏覆盖` 的分子和分母都来自可恢复集合，不会用 local catalog 总数除以 session index 总数。
- `远端映射` 是 catalog host 或来源元数据明确标记为远端的唯一 ID。
- `孤儿` 存在于本地索引、rollout 或 session index，但没有可信 `threads` 行。已识别的远端映射不算本地孤儿。
- `内容异常` 表示普通本地候选缺少唯一、无歧义的活动 rollout。
- `待恢复` 是可恢复集合中缺少本地 catalog 的 ID 数量。
- `Provider 漂移` 是同一可恢复 ID 在 state 与本地 catalog 中的 Provider 不一致。
- `JSONL Provider 漂移` 只作提示。工具不改 JSONL，因此修复后它仍可能存在。

## VS Code 和远端会话

`source=vscode` 只说明会话由 VS Code 入口创建，不能据此判断它来自本机、SSH、WSL 还是 Dev Container。

工具只使用强信号识别远端：

- catalog 的 `host_id` 不是 `local`；
- `source_kind` 或 `source_detail` 明确表示 remote、SSH、WSL、Dev Container 或 Codespaces；
- thread 来源元数据包含明确的远端 authority。

只有 remote catalog、没有安全本地映射的线程会被排除，不会被补成 `host_id=local`。同一 ID 同时存在 local 和 remote 行时，只检查 local 行，remote 行保持原样。工具不会连接 SSH 主机，也不会扫描远端机器上的 `~/.codex`。

Linux 风格 `cwd` 本身不是远端证据。这样可以避免把本机 WSL 路径、容器挂载目录或普通 VS Code 会话误判成远端。

## 能力边界

当前支持：

- 自动发现 `CODEX_HOME`，也可通过 CLI 指定路径。
- 扫描两套 SQLite、活动和归档 rollout、session index、global state。
- OpenAI、Custom、CodexPilot Provider allowlist。
- 普通 `cli`、`vscode`、`appServer`、`custom` 和 `user` 来源。
- dry-run、SQLite 快照、Provider 对齐、本地 catalog 增补、验证和回滚。
- 跳过远端、归档、子代理、自动化、未知来源、不可信 Provider 和内容异常记录。
- 幂等修复。第二次执行应显示 `0 changes`。

当前不做：

- Provider 密钥、模型、`config.toml` 或 `auth.json` 管理。
- JSONL、`session_index.jsonl` 或 global state 重写。
- 找回已经丢失的会话内容。
- 云同步、后台 watcher、自动连接远端主机。
- 强制关闭 Codex 进程或盲目删除锁文件。
- 自动处理子代理、自动化、exec 或未知来源。
- 绕过 Codex 上游侧栏的显示数量限制。
- 完整复制整个 `CODEX_HOME`。
- 持久化执行日志。桌面日志只存在于当前应用会话，CLI 日志由 stdout/stderr 输出。

## 安全设计

| 操作 | 是否修改会话数据 |
| --- | --- |
| 扫描 | 否 |
| dry-run | 否 |
| 验证 | 否 |
| 创建快照 | 不改业务库；写入备份目录和工具锁 |
| apply | 修改两套 SQLite 中经过筛选的字段和本地索引 |
| restore | 用快照恢复两套 SQLite |

写入前会执行以下检查：

- 两套 SQLite 位于 `CODEX_HOME` 内，是普通文件且不是符号链接。
- schema 包含修复所需的表和字段。
- SQLite `quick_check` 通过。
- 没有活动 Codex、Codex++ 或 launcher 进程。
- 工具锁可获取，数据库可以取得写锁。
- 目标 Provider 位于 allowlist，且与 `config.toml` 当前 Provider 一致。
- 写入计划只包含本地、活动、普通且具有有效 rollout 的线程。

备份目录：

```text
CODEX_HOME/backups/provider-hub/repair-YYYYMMDD-HHMMSS-mmm/
```

每次备份实际复制：

```text
state_5.sqlite
sqlite/codex-dev.db
```

manifest 会记录这两份快照的 SHA-256、SQLite user version，以及 config、auth、global state 和 JSONL 的路径、大小、修改时间。后面这些文件只登记资产信息，不复制内容。因此这是“两库 SQLite 快照 + 资产清单”，不是完整的 `CODEX_HOME` 灾备。

修复后会重新扫描并验证。写入失败或验证失败时，工具会尝试恢复刚创建的快照；如果恢复也失败，错误会明确报告。失效工具锁会被改名留存，不会直接删除。

## 排障

### `CODEX_HOME` 未发现

确认目录存在。桌面端使用 `CODEX_HOME` 环境变量或当前用户的 `~/.codex`；CLI 可以显式指定：

```powershell
codex-provider-hub.exe scan --codex-home C:\Users\me\.codex
```

### `SQLite` 少于 2 个或 schema 不支持

确认下面两个文件存在，并由当前 Codex 版本正常创建：

```text
CODEX_HOME/state_5.sqlite
CODEX_HOME/sqlite/codex-dev.db
```

不要手工创建空数据库。检查当前用户是否有读取权限。

### `process-active`

日志会列出检测到的进程。保存正在进行的工作，正常退出 Codex、Codex++ 和 Provider launcher，然后重新扫描。工具不会代替用户结束进程。

### 活动锁或数据库锁

先确认锁的 owner PID 是否仍在运行。不要直接删除 `.provider-hub.lock` 或 SQLite 的 `-wal/-shm` 文件。退出相关应用后重试；持续出现 `database is locked or not writable` 时，再检查文件权限和进程权限级别。

### 需要管理员权限

先检查是否有以管理员身份运行的 Codex 相关进程。普通情况下不需要提升权限；如果相关数据库确实由更高权限的进程持有，再以相同权限运行本工具。

### `target provider must match config.toml`

选择界面显示的当前 Provider。需要切换 Provider 时，先在外部完成切换，再重新扫描。工具不会修改 `config.toml`。

### 验证通过但 JSONL drift 仍存在

这是可能出现的正常结果。JSONL Provider 只读，验证关注的是受支持的 state 与本地 catalog 写入范围。

### 验证通过但侧栏仍未显示全部会话

先重启 Codex，让侧栏重新读取索引。若索引覆盖率已经是 100%，剩余差异通常来自上游侧栏的最近记录显示上限，或会话属于归档、远端和其他排除类别。

### 如何回滚

关闭相关进程后，在桌面端选择“回滚最近一次”，或执行：

```powershell
codex-provider-hub.exe restore
```

也可以指定 `CODEX_HOME/backups/provider-hub` 下的某个备份目录。

## 本地运行

需要：

- Node.js 和 npm；
- Rust stable 和 Cargo；
- 当前操作系统要求的 Tauri 2 原生依赖；
- Windows 上可用的 WebView2 Runtime。

安装依赖并启动完整桌面环境：

```powershell
npm ci
npm run tauri -- dev
```

`npm run dev` 只启动 Vite，不包含 Rust core 和 Tauri IPC。开发时的 `http://localhost:1420` 只是 WebView 资源地址，不是产品的浏览器模式。直接用浏览器打开时只会看到“请从 Tauri 桌面端启动”的提示。

## 验证和构建

```powershell
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --lib -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --lib
```

隔离测试使用临时 `CODEX_HOME`，覆盖 dry-run 零写入、WAL 无副作用、remote-only 跳过、内容缺失跳过、归档和自动化跳过、备份、修复、验证、幂等和恢复。测试不会写入当前用户的真实 `~/.codex`。

## 打包和部署

这是桌面软件，不需要部署 Web 服务器：

```powershell
npm run tauri -- build
```

Windows 构建输出：

```text
src-tauri/target/release/codex-provider-hub.exe
src-tauri/target/release/codex-provider-hub-cli.exe
src-tauri/target/release/bundle/msi/Codex Provider Hub_0.1.0_x64_en-US.msi
src-tauri/target/release/bundle/nsis/Codex Provider Hub_0.1.0_x64-setup.exe
```

主程序、安装包和 CLI 都应作为同一版本 GitHub Release 的附件发布。tag 只指向源码提交，二进制文件实际挂在与该 tag 关联的 Release 页面上。

macOS 和 Linux 应分别在对应系统或 CI matrix 中执行同一构建命令。`tauri.conf.json` 中的 `targets: "all"` 表示生成当前平台支持的 bundle，不表示 Windows 会交叉生成 macOS 或 Linux 安装包。

公开分发前还需要按平台配置代码签名。macOS 通常还需要公证；当前仓库没有自动更新、签名、公证或发布流水线配置。

## 项目结构

```text
src/                    React/TypeScript 桌面界面
src-tauri/src/core.rs   扫描、计划、备份、修复、验证和恢复核心
src-tauri/src/main.rs   无控制台的桌面入口
src-tauri/src/bin/      独立 CLI 入口
src-tauri/src/lib.rs    Tauri 命令桥接
src-tauri/src/main.rs   桌面与 CLI 入口
src-tauri/icons/        Windows、macOS、Linux 及移动端图标资源
src-tauri/tauri.conf.json
```
