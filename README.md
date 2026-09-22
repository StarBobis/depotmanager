# DepotManager — Steam 历史版本下载器

一个桌面工具（Tauri 2 + Vue 3 + **纯 Rust 自研 Steam 协议栈**），可以搜索浏览 Steam 游戏、查看各分支与历史版本清单，并把指定版本高速下载到指定目录。

不依赖 Steam 客户端、不包装 DepotDownloader、不调用任何 C# 运行时 —— 从 CM WebSocket 握手、protobuf 消息路由、PICS 产品信息、manifest 解析（文件名 AES-256-ECB/CBC 解密）、到 CDN 分块下载（vZstd/VZip/Zip 解压 + adler32 校验）全部由本仓库的 Rust 代码实现。协议细节参考了 [DepotDownloader](https://github.com/SteamRE/DepotDownloader) 与 [SteamKit2](https://github.com/SteamRE/SteamKit)（`research/` 目录为调研快照）。

## 用途

Steam 默认只允许安装游戏的最新版本，本工具解决这些场景：

- **版本回退**：游戏更新后出现 BUG / MOD 不兼容 / 手感变差？查看历史 manifest，把游戏降级到任意可下载的旧版本
- **版本固定**：为 MOD 社区、速通、服务器架设等场景锁定一个特定 build，随时重现
- **安装修复**："仅校验"模式按分块校验已有安装目录，只补下损坏/缺失的分块，不必整包重下
- **省流量切换版本**：不同版本之间内容一致的文件自动跳过，只下载差异分块
- **部分下载**：用正则过滤文件（如 `\.exe$`），只取需要的文件

## 功能

- **搜索浏览游戏**：Steam 商店搜索 API（美区/国区双源）+ 本地应用列表缓存兜底；已知 AppID 可直接打开
- **游戏详情**：名称、支持平台、全部分支（buildid/说明）、全部 depot（系统/架构/语言/大小/当前会话权限）
- **历史版本**：
  - 每个 depot 的各分支当前 manifest（始终可用）
  - SteamDB manifest 历史（尽力而为，SteamDB 常有 Cloudflare 拦截）
  - 手动粘贴 Manifest GID（与 DepotDownloader 用法一致）
- **高速下载**：
  - 多连接并行分块（默认 16，可调至 64），实测 30-50 MB/s（视网络/CDN 限速）
  - 断点续传：重新下载时按分块 adler32 校验已有文件，只补缺失/损坏分块
  - 版本切换：与目标版本内容一致的文件自动跳过
  - 暂停 / 继续 / 取消，实时速度、进度、ETA、当前文件
  - 正则文件过滤（只下载需要的文件，例如 `\.exe$`）
  - 仅校验模式（修复已有安装）
- **登录**：匿名（部分免费游戏）、账号密码（Steam Guard 邮箱/令牌验证码）、手机扫码；令牌安全保存，重启自动登录；断线自动重连
- **自动领取免费许可**：登录账号打开未入库的免费游戏时，自动向 Steam 领取 FreeOnDemand 许可并刷新完整 Depot 列表
- **设置持久化**：下载目录、并发数、系统/架构/语言过滤、区域 cell 等

## 运行

```bash
bun install          # 或 npm install
bun run tauri dev    # 开发模式（带热更新）
bun run tauri build  # 发布构建
```

Rust 协议层可独立冒烟测试（无需 GUI）：

```bash
cd src-tauri
cargo run --bin smoke  -- 570                 # 连接 CM + 匿名登录 + PICS 应用信息
cargo run --bin smoke2 -- 730                 # CDN 服务器 + depot 密钥 + manifest + 分块下载
cargo run --bin smoke3 -- 730 2347770 "\.txt$" # 完整下载引擎（带文件过滤）
cargo run --bin smoke4 -- 730                 # 匿名授权包（17906）内容检查
```

## 架构

```
src-tauri/src/
├── steamkit/           # 自研 Steam 协议栈
│   ├── connection.rs   #   CM WebSocket 连接（ClientHello、消息路由、Multi 拆包、断线通知）
│   ├── session.rs      #   登录会话（匿名/账号、心跳、JWT 解析）
│   ├── auth.rs         #   认证服务流（扫码/账密/Steam Guard 轮询）
│   ├── pics.rs         #   PICS 产品信息（appinfo/package、访问令牌）
│   ├── manifest.rs     #   二进制 manifest 解析/序列化（文件名解密）
│   ├── cdn.rs          #   CDN：服务器列表、请求码、鉴权 token、分块下载/解密/解压/校验
│   ├── crypto.rs       #   AES-256-ECB/CBC、RSA 密码加密、adler32
│   ├── kv.rs           #   Valve KeyValues（文本 VDF + 二进制 KV）
│   └── msgs.rs         #   帧编解码、EMsg/EResult 常量
├── engine/mod.rs       # 下载引擎：depot 解析、权限检查、文件校验、并行分块调度
├── steam_runtime.rs    # 会话生命周期、自动重连、登录流编排
├── commands.rs         # Tauri 命令层（前后端契约）
├── store.rs            # 商店搜索 + 应用列表缓存
├── history.rs          # manifest 历史（SteamDB 抓取 + PICS 分支合并）
└── state.rs            # 设置/账号持久化（config/ 目录）

src/                    # Vue 3 前端
├── components/LoginPanel.vue    # 登录（账密/扫码/已存账号/匿名）
├── components/SearchView.vue    # 搜索浏览
├── components/DetailView.vue    # 游戏详情、depot/分支/历史版本选择、下载配置
├── components/TasksView.vue     # 下载队列（进度/速度/暂停/取消）
└── components/SettingsView.vue  # 设置
```

## 已知限制

- **匿名登录**只能访问匿名授权包（sub 17906）覆盖的免费游戏（如 CS2、Dota 2）；不在包内的免费游戏（如永劫无间）Steam 服务端不向匿名用户返回 Depot 信息，登录任意账号即可自动领取免费许可后下载；付费游戏需登录拥有该游戏的账号
- **老版本 manifest**需要 Steam 返回请求码（request code）；开发者可以封锁老版本，此时只能下载当前分支版本
- SteamDB 历史为尽力而为（Cloudflare 拦截时仅显示当前分支版本，可手动粘贴 GID）
- 任务列表不持久化：重启应用后任务消失，但已下载文件保留，重新发起同版本下载会自动续传

## 发布新版本

仓库自带一键发布脚本（需要 bun / cargo / tauri-cli，以及已登录的 git 凭据）：

```powershell
.\scripts\release.ps1            # 按 tauri.conf.json 中的版本号构建并发布
.\scripts\release.ps1 -Version 0.2.0   # 先升级版本号再发布
```

脚本会同步 `tauri.conf.json` 与 `Cargo.toml` 的版本号、执行 `tauri build`（NSIS + MSI 安装包）、创建并推送 git tag，最后通过 GitHub API 创建 Release 并上传安装包。

## 许可证

MIT。仅供学习研究，请遵守 Steam 服务条款。
