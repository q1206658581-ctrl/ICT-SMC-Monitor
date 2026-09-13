# ICT Radar / ICT-SMC-Monitor

基于 Tauri、Rust、React 和 TypeScript 的桌面行情监控与 ICT/SMC 分析工具。目前主要在 macOS 上开发和验证。

## 功能

- TradingView 行情接入、多周期 K 线、单图和三联图、品种搜索及分组管理。
- SMT、C2/C3、CISD、MSS、FVG、流动性等指标及可折叠参数设置。
- SMT Inbox、Alert Inbox、Reversal Inbox、Decisions 与运行日志，支持记录定位和桌面通知。
- 可选 LLM 辅助分析，程序计算交易方向、入场区、失效位与目标位，并校验模型输出。
- 多头/空头仓位示意图：拖动价位、整体平移、精确输入、可空止盈、RR 显示和本地保存。
- 历史告警可“画到图上”，固定在 C2 确认时刻；手动画仓位显示在最新 K 线右侧。未设止盈时可直接拖动手柄添加目标。
- 独立离线回测命令，支持真实目标、固定 R 和 user-v1 模式。

仓位图是人工分析标注；C2 确认不会自动创建仓位图，应用不自动下单。

## 开发环境

- Rust / Cargo（项目使用 Rust 2021 edition）。
- Node.js 24 与 pnpm（当前开发环境使用 pnpm 11）。
- macOS 的 Xcode Command Line Tools；其他系统需要对应的 Tauri 2 原生构建依赖，尚未完成同等装机验证。

以下命令均在仓库根目录执行。

```sh
git clone git@github.com:q1206658581-ctrl/ICT-SMC-Monitor.git
cd ICT-SMC-Monitor
pnpm --dir ui install --frozen-lockfile
./ui/node_modules/.bin/tauri dev
```

Tauri 会启动前端开发服务，地址为 `http://localhost:5173`。单独运行 `pnpm --dir ui dev` 可启动前端，但主应用的数据与操作依赖 Tauri IPC。

## 本地配置与数据

| 内容 | 默认位置 | 环境变量覆盖 |
| --- | --- | --- |
| 应用配置 | `~/.ict-monitor/config.toml` | `ICT_CONFIG_PATH` |
| SQLite 数据库 | `~/.ict-monitor/ict.db` | `ICT_DB_PATH` |

没有配置文件时，应用使用内置分组，LLM 默认关闭。最小配置示例：

```toml
[llm]
enabled = false
```

TradingView 配置支持 `sessionid`、`sessionid_sign` 和独立 `proxy_url`；行情可用范围取决于数据源及账号权限。配置字段和默认值见 [config.rs](src-tauri/src/config.rs)。

启用 LLM 时，需要设置 `[llm]` 下的 `enabled`、`provider`、`model`、`base_url` 和 `api_key_env`。当前代码支持 `openai_compatible`、`deepseek_official`、`volcengine_ark_plan`。API Key 从 `api_key_env` 指定的环境变量读取，macOS 也支持登录钥匙串；不要把真实密钥或会话 Cookie 放入仓库。

历史仓位预填优先读取保存的程序价格快照；没有决策快照时按告警时间复算。未找到合格目标时止盈为空、RR 显示 `—`，可手动补充。历史行情覆盖不足时会提示，不能保证所有旧记录都能定位或完整复算。

## 构建

```sh
# 前端类型检查及打包
pnpm --dir ui build

# macOS 应用构建
./ui/node_modules/.bin/tauri build --bundles app
```

默认产物位于 `target/release/bundle/macos/ICT Radar.app`。设置 `CARGO_TARGET_DIR` 时产物位于对应目录。构建不会自动覆盖 `/Applications` 中的已安装应用；分发签名和公证需要另外配置。

## 验证

```sh
cargo test --workspace
node --experimental-strip-types --test ui/tests/*.test.ts ui/tests/*.test.mjs
pnpm --dir ui build
```

绘图交互隔离页面：

```sh
pnpm --dir ui dev
# 浏览器打开 http://localhost:5173/tests/positions-browser.html
```

该页面使用真实图表组件、模拟 IPC、合成行情和独立浏览器存储，不连接生产数据库。

查看离线回测参数：

```sh
cargo run --bin backtest-eval -- --help
```

## 代码目录

| 路径 | 用途 |
| --- | --- |
| `src-tauri/src/bin/ict_radar.rs` | 桌面应用入口、后台任务与 IPC |
| `src-tauri/src/data_source/` | 行情连接与品种查询 |
| `src-tauri/src/detector/` | 指标与结构识别 |
| `src-tauri/src/alert/`、`candidate/` | 告警与候选生命周期 |
| `src-tauri/src/llm/` | 上下文、程序价格规则和模型调用 |
| `src-tauri/src/storage/` | SQLite 持久化 |
| `src-tauri/src/backtest/` | 离线评估 |
| `ui/src/components/chart/positions/` | 仓位图及拖动交互 |
| `ui/src/components/layout/` | 工具栏、侧栏与 Inbox |
| `prompts/` | 编译时载入的运行时提示词 |

仓库保留源代码、测试、依赖锁文件和本 README。PRD、开发过程与验收文档、本地数据库、截图、报表、密钥以及构建产物不随代码提交。`prompts/*.md` 是应用运行所需的提示词，不属于过程文档。
