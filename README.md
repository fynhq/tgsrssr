<a id="chinese"></a>

# tgsrssr

> **T**elegram's **R**SS **R**ust — RSS 订阅 Telegram 推送 Bot

把 RSS/Atom 源的新条目定时推送到 Telegram 频道/群组，并对**微博、X (Twitter)、Telegram 频道**提供原生抓取。支持富媒体相册、内容筛选、消息反查、断点续发与持久化重试。

中文 | [English](#english)

## 效果展示

示例频道：[People's Daily](https://t.me/peoplesdailyzh)（微博）、[Elon Musk](https://t.me/elonmuskxtot)（X）、[Hack News](https://t.me/hackernewsyc)（RSS）。

## 为什么选 tgsrssr

- **原生平台抓取**：微博（长文全文、多图补全、视频 HD、投票转 Poll）、X（GraphQL 原图与最高码率视频）、Telegram 公开频道。
- **不丢消息**：发送失败先降级纯文本，仍失败进入 outbox 持久化重试（1 分钟→2 小时退避，72 小时放弃）。
- **富媒体管线**：图片/视频混排相册、GIF/文档处理、超限转文档、file_id 去重、流式下载。
- **富消息增强**：文本超单条上限（约 4094 可见字符）、带媒体文本超 1024 字符，或图片/视频/文档超过 10 个时自动合并为一条富消息，失败回退分段/相册；需 Android/iOS 12.8+、Desktop 6.9.2+、macOS 12.9+、Web K（2026-07-03 后构建），旧客户端提示不支持。
- **多用户与权限分级**：单一管理员 + 白名单；频道归属按邀请者判定，平台 cookie 全局共享。
- **轻量部署**：Rust 单二进制 + SQLite，Docker 一键启动；常驻内存约 22 MB、CPU 接近 0（compose 默认限 1 GB 内存 / 1 CPU）；界面支持中文/English。

## 快速开始

### 前置条件

- Docker + Docker Compose（推荐 Linux；`network_mode: host` 仅 Linux 原生支持）
- Telegram Bot Token（[TG@BotFather](https://t.me/BotFather)）
- 你的 Telegram 用户 ID（[TG@userinfobot](https://t.me/userinfobot)）

### Docker（推荐）

```bash
cp compose.example.yaml compose.yaml
# 编辑 compose.yaml，填入 TELOXIDE_TOKEN 和 ADMIN_USER_IDS（管理员，仅一个正整数）
docker compose pull
docker compose up -d
docker logs -f tgsrssr
```

从源码构建：注释 `image` 行、取消注释 `build: .`，再执行 `docker compose up -d --build`（首次约 5-10 分钟）。

### 静态二进制

从 [Releases](../../releases) 下载 Linux tar.gz（x86_64 / aarch64，musl 静态，包名含 `-musl`）与 `SHA256SUMS`，校验后解压运行：

```bash
sha256sum -c --ignore-missing SHA256SUMS
tar xzf tgsrssr-vX.Y.Z-<arch>-musl.tar.gz
TELOXIDE_TOKEN=xxx ADMIN_USER_IDS=123 ./tgsrssr
```

> 需自行安装 ffmpeg（视频缩略图/时长）。Android（Termux）包为 `tgsrssr-vX.Y.Z-aarch64-android.tar.gz`（Android 7+/API 24，非普通 Android 应用），同样需 ffmpeg；TLS 使用内置 Mozilla 根证书。

### 首次使用

1. 私聊 Bot 发送 `/start`
2. 把 Bot 加入频道或群组（频道需发消息权限）
3. 在**私聊**中发送 `/sub <频道ID或@用户名> <RSS地址>`

所有命令仅限私聊使用；群组/频道只作为推送目标。需要推送大视频时，可部署本地 Bot API（见下文）。

## 命令

| 命令 | 说明 |
|---|---|
| `/start` | 帮助 |
| `/set [频道]` | 频道管理面板 |
| `/list <频道>` | 查看频道订阅列表 |
| `/sub <频道> <url...>` | 添加订阅（可一次多个） |
| `/test <频道> <源地址> [条目序号]` | 立即推送指定源的某一条（0=最新） |
| `/find <TG消息链接>` | 按 TG 消息反查 RSS 条目；也可直接转发频道消息给 bot |
| `/cookies` | 平台登录 cookie 管理（仅管理员） |
| `/loget <日期>` | 获取指定日期的日志文件（仅管理员） |
| `/about` | 关于（版本信息） |

仅管理员与 `ALLOWED_USER_IDS` 白名单可用；管理员可在频道列表切换界面语言；`/cookies`、`/loget` 仅管理员可用，平台 cookie 由管理员配置后所有用户共享。
**所有命令与按钮面板仅限私聊**：群组/频道内的命令被静默忽略，`/find` 的转发消息也需在私聊操作；`/sub`、`/test`、`/list` 必须显式给出频道参数（`@用户名` 或数字 ID）。

**限制**：非管理员 `/sub`、`/test` 合共每分钟最多 10 次（管理员豁免）；单次 `/sub` 最多 10 个去重 URL；每频道默认最多 99 个订阅，非管理员每用户默认最多 300 个、平台源默认最多 20 个（均可配置为 0 = 不限）。超限整条拒绝。

## 支持的内容源

任意 RSS 2.0 / Atom 源（支持 ETag / Last-Modified 条件请求）与以下原生源：

| 平台 | 订阅地址 | 要求与能力 |
|---|---|---|
| 微博 | `https://weibo.com/u/<uid>` | 需登录 cookie；长文全文、多图补全（>9 张）、视频 HD、表情转 emoji、投票转 Poll 置顶、引用块、头条/直播封面 |
| X / Twitter | `https://x.com/<screen_name>` | 需 `auth_token` cookie；长推文全文、原图、最高码率视频，@/#/$ 自动超链，转推/引用以引用块呈现 |
| Telegram | `https://t.me/<channel>` | 仅公开频道、免登录；保留超链接，回复以引用块呈现 |

本地 RSSHub：SSRF 防护默认拒绝私网/回环地址，需在 `RSS_TRUSTED_PRIVATE_HOSTS` 精确配置 `host:port`（如 `rsshub:1200`）；该 allowlist 仅对管理员/受信用户生效（scope 默认 `admin`），不是通用私网白名单，`RSS_NO_PROXY` 只影响路由。条目内媒体仅在与订阅 feed 同源（scheme+host+port）时继承该 allowlist，公网 feed 无法借它访问内网。

## 频道管理面板（/set）

- 频道/源列表分页（每页 10 个），显示订阅数，源最新在前，停用源带 ⏸。
- 频道设置：推送间隔（默认 30 分钟）、内容模式、来源显示、媒体在下（仅影响图片/视频/动图说明位置）、链接预览；源级可逐项覆盖，与频道相同会自动清除覆盖（✏️/🔗）。
- 源操作：暂停/恢复、改名、内容筛选（源级黑/白名单，关键词用半角/全角逗号或分号分隔，匹配标题、不区分大小写）；频道页可一键恢复全部停用源。
- 退订管理：一键退订全部（确认后事务执行并发送结果）；频道列表底部可切换中文/English。

## 推送与媒体

- **内容模式**：`title_link` / `title_summary`（140 字摘要，只下载首个图片/视频）/ `title_content` / `title_only` / `content_only` / `media_only`；**来源显示**：`source` / `author` / `none`。
- 长文按可见长度分段（正文 4096 / 媒体说明 1024 UTF-16 码元），后续分段引用第一条；HTML 解析失败降级纯文本。
- 图片与视频可混排同一相册（每册最多 10 项、单条最多 30 项），GIF 单独发送；超限大图、长图及 sendPhoto 不支持的格式（AVIF/SVG/HEIC/TIFF 等）转文档保留原图（WebP/BMP 尺寸允许时按图片发送），转文档媒体成组发送。
- 流式下载防 OOM，内容哈希 + file_id 去重；视频自动附缩略图与时长；可选图片中转反代；视频超限降级纯文本并通知。

## 可靠性与调度

- 连续抓取失败达阈值（默认 20 次）自动停源并通知；每小时自愈检查，冷却 3 小时后恢复。
- Outbox 持久化重试：1 分钟→2 小时指数退避，72 小时放弃；支持分段/op 级续发。
- 平台级健康监控：连续 2 轮全部源失败通知管理员，恢复后再次通知；媒体下载 5xx 重试（间隔 5 秒）、4xx 不重试，失败随条目最多重试 3 轮后降级发送。
- 定时任务：抓取（默认 5 分钟，按源间隔）、outbox 重试（5 分钟）、自愈（每小时）、清理（默认 60 分钟）。
- 通知对象：频道属主（源停用、媒体失败/视频超限、入队/72h 放弃）、管理员（cookie 失效、平台异常与恢复）；Bot 被移出频道时软停用该频道并保留数据，重新添加后恢复（属主不变）。

## 配置参考（compose.yaml environment）

| 变量 | 说明 | 默认 |
|---|---|---|
| `TELOXIDE_TOKEN` | BotFather token（**必填**） | - |
| `ADMIN_USER_IDS` | 管理员，**必须配置且只允许一个正整数**用户 ID；可用 /cookies、/loget，接收平台告警 | - |
| `ALLOWED_USER_IDS` | 用户白名单 ID，逗号分隔（可选；管理员隐式放行） | 空 |
| `BOT_API_URL` | 本地 Bot API 地址；空 = 官方 API（设置后 `BOT_PROXY` 被忽略） | 空 |
| `BOT_PROXY` | Bot API 代理（`http://`、`socks5://`、`socks5h://`）；空 = 直连 | 空 |
| `RSS_PROXY` | RSS/平台抓取与媒体下载代理（如 `socks5h://127.0.0.1:1080`；含 t.me） | 空 |
| `RSS_NO_PROXY` | 不走代理的域名（逗号分隔） | 空 |
| `RSS_TRUSTED_PRIVATE_HOSTS` | 受信私网源 allowlist，精确 `host:port`；仅管理员/受信用户可用，非通用白名单 | 空 |
| `RSS_TRUSTED_PRIVATE_SCOPE` | `admin` = 仅管理员；`allowed` = 管理员 + 白名单 | admin |
| `MAX_MEDIA_DOWNLOAD_MB` | 单文件媒体下载上限（MB，所有媒体类型统一）；官方 API 下实际生效 49MB | 100 |
| `MAX_SUBSCRIPTIONS_PER_CHAT` | 每频道订阅数上限（0 = 不限） | 99 |
| `DB_PATH` | SQLite 路径 | data/rss_bot.db |
| `MEDIA_DIR` | 媒体目录 | media |
| `LOG_DIR` | 日志目录 | logs |
| `RUST_LOG` | 日志级别（tracing EnvFilter） | info |

代理只由 `RSS_PROXY`/`BOT_PROXY` 控制：程序不读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY`；`BOT_PROXY` 仅覆盖 Bot API。完整可调项（并发、清理、配额、缓存保留期等）见 [compose.example.yaml](compose.example.yaml)。

## 本地 Bot API（可选）

官方 API 单文件上限 50MB（本项目取 49MB）。部署本地 Bot API 后上限提高到 2GB，富消息图片/视频附件可由 bot-api 通过 `file://` 直读（文档附件仍走 multipart 保留文件名）。媒体仍受 `MAX_MEDIA_DOWNLOAD_MB`（默认 100）与单条目 200MB 总量约束；下载按 magic/容器头校验，HTML 错误页与 HLS 清单不会作为媒体发送。

| 模式 | 触发条件 | 单文件上限 |
|---|---|---|
| 官方 API | `BOT_API_URL` 留空 | 49MB |
| 本地 Bot API | `BOT_API_URL` 已设置 | 2GB |

bot-api 侧需设置 `TELEGRAM_API_ID`/`TELEGRAM_API_HASH`（my.telegram.org）与 `TELEGRAM_LOCAL=1`，并让两个容器挂载同一宿主媒体目录（挂载示例见 [compose.example.yaml](compose.example.yaml)）。tgsrssr 侧配置：

```yaml
services:
  tgsrssr:
    environment:
      BOT_API_URL: "http://127.0.0.1:8081"
      BOT_API_MEDIA_DIR: "/var/lib/telegram-bot-api-media"
```

`BOT_API_MEDIA_DIR` 默认 `/var/lib/telegram-bot-api-media`，是否启用 2GB 上限只看 `BOT_API_URL`；本地模式下 `BOT_PROXY` 被忽略，到 Telegram 的连通性由 bot-api 负责。

## 开发者

- Rust 1.96+；`cargo build --release --locked`；表结构见 [`migrations/`](migrations/)。
- CI 在 tag push 时构建 amd64/arm64 镜像，并上传 Linux musl 与 Android（Termux）静态二进制及合并的 `SHA256SUMS`；Docker 部署推荐 Linux，macOS/Windows 需 bridge 网络。

## 致谢

参考与数据来源：[RSSHub](https://github.com/DIYgod/RSSHub)、[RSS-to-Telegram-Bot](https://github.com/Rongronggg9/RSS-to-Telegram-Bot)、[yt-dlp](https://github.com/yt-dlp/yt-dlp)、[TwitterInternalAPIDocument](https://github.com/fa0311/TwitterInternalAPIDocument)、[CPython](https://github.com/python/cpython)。运行时依赖：[teloxide](https://github.com/teloxide/teloxide)、[feed-rs](https://github.com/feed-rs/feed-rs)、[reqwest](https://github.com/seanmonstar/reqwest)、[SQLx](https://github.com/launchbadge/sqlx)、[SQLite](https://www.sqlite.org/)、[Tokio](https://github.com/tokio-rs/tokio)、[Serde](https://github.com/serde-rs/serde)、[tracing](https://github.com/tokio-rs/tracing)。部署与工具：[telegram-bot-api](https://github.com/aiogram/telegram-bot-api)、[FFmpeg](https://ffmpeg.org/)、[proxychains-ng](https://github.com/rofl0r/proxychains-ng)、[cross](https://github.com/cross-rs/cross)。其余依赖见 [Cargo.toml](Cargo.toml)。

## 许可

基于 MIT 许可发布，见 [LICENSE](LICENSE)。

---

<a id="english"></a>

# tgsrssr

> **T**elegram's **R**SS **R**ust — an RSS subscription Telegram push bot

Periodically pushes new entries from RSS/Atom feeds to Telegram channels/groups, with **native fetching for Weibo, X (Twitter), and Telegram channels**. Includes rich media albums, content filtering, message reverse lookup, resumable sending, and persistent retries.

[中文](#chinese) | English

## Showcase

Example channels: [People's Daily](https://t.me/peoplesdailyzh) (Weibo), [Elon Musk](https://t.me/elonmuskxtot) (X), [Hack News](https://t.me/hackernewsyc) (RSS).

## Why tgsrssr

- **Native platform fetching**: Weibo (full long-form text, multi-image completion, HD video, polls → TG Poll), X (GraphQL original images and highest-bitrate video), public Telegram channels.
- **No message loss**: failed sends fall back to plain text, then enter a persistent outbox (1 min → 2 h backoff, abandoned after 72 h).
- **Rich media pipeline**: mixed photo/video albums, GIF/document handling, oversized media → documents, file_id dedup, streaming downloads.
- **Rich-message enhancement**: text over the single-message limit (about 4,094 visible chars), media posts over 1,024 characters, or more than 10 photos/videos/documents are combined into one rich message, falling back to the split/album path on failure. Requires Android/iOS 12.8+, Desktop 6.9.2+, macOS 12.9+, and Web K builds from 2026-07-03 or later; older clients show an unsupported message.
- **Multi-user + admin tiers**: one admin + whitelist; channel ownership by inviter; shared platform cookies.
- **Lightweight deployment**: single Rust binary + SQLite, one-command Docker startup; about 22 MB resident memory and near-zero CPU (the bundled compose file caps it at 1 GB RAM / 1 CPU); Chinese/English UI.

## Quick Start

### Prerequisites

- Docker + Docker Compose (Linux recommended; `network_mode: host` is Linux-only)
- A Telegram Bot Token ([TG@BotFather](https://t.me/BotFather))
- Your Telegram user ID ([TG@userinfobot](https://t.me/userinfobot))

### Docker (recommended)

```bash
cp compose.example.yaml compose.yaml
# Edit compose.yaml: set TELOXIDE_TOKEN and ADMIN_USER_IDS (exactly one positive admin ID)
docker compose pull
docker compose up -d
docker logs -f tgsrssr
```

Build from source: comment out `image`, uncomment `build: .`, then run `docker compose up -d --build` (~5-10 minutes on first run).

### Static binary

Download a Linux tar.gz from [Releases](../../releases) (x86_64 / aarch64, musl static; the archive name includes `-musl`) plus `SHA256SUMS`, verify, then extract and run:

```bash
sha256sum -c --ignore-missing SHA256SUMS
tar xzf tgsrssr-vX.Y.Z-<arch>-musl.tar.gz
TELOXIDE_TOKEN=xxx ADMIN_USER_IDS=123 ./tgsrssr
```

> ffmpeg must be installed separately (video thumbnails/duration). Android (Termux) builds are published as `tgsrssr-vX.Y.Z-aarch64-android.tar.gz` (Android 7+/API 24, Termux only; not a regular Android app) and also require ffmpeg; the Android build validates HTTPS with bundled Mozilla roots.

### First use

1. Send `/start` to the bot in a private chat
2. Add the bot to your channel or group (channels need permission to post)
3. Send `/sub <channel ID or @username> <RSS URL>` **in a private chat**

All commands are private-chat only; groups/channels are push targets. To push large videos, deploy a local Bot API server (see below).

## Commands

| Command | Description |
|---|---|
| `/start` | Help |
| `/set [channel]` | Channel management panel |
| `/list <channel>` | View channel subscription list |
| `/sub <channel> <url...>` | Add subscriptions (multiple at once) |
| `/test <channel> <feed URL> [entry index]` | Immediately push one entry from a feed (0 = latest) |
| `/find <TG message link>` | Reverse-lookup an RSS entry from a TG message; forwarded channel messages also work |
| `/cookies` | Platform login cookie management (admin only) |
| `/loget <date>` | Get the log file for a specific date (admin only) |
| `/about` | About (version info) |

Only the admin and `ALLOWED_USER_IDS` whitelist can use the bot; the admin can switch the UI language from the channel list; `/cookies` and `/loget` are admin-only, and platform cookies configured by the admin are shared by all users.
**All commands and inline panels are private-chat only**: commands typed in groups/channels are silently ignored, and forwarding a channel message for `/find` must also happen in a private chat. `/sub`, `/test`, and `/list` must name the channel explicitly (`@username` or numeric ID).

**Limits**: non-admin `/sub` and `/test` share a 10-per-minute limit (admins are exempt); one `/sub` accepts at most 10 deduplicated URLs; each channel defaults to at most 99 subscriptions, and non-admin users default to at most 300 total and 20 platform sources (configurable, 0 = unlimited). Over-limit commands are rejected as a whole.

## Supported Sources

Any RSS 2.0 / Atom feed (with ETag / Last-Modified conditional requests), plus these native sources:

| Platform | Subscribe URL | Requirements and capabilities |
|---|---|---|
| Weibo | `https://weibo.com/u/<uid>` | Login cookie required; full long-form text, multi-image completion (>9), HD video, emoji conversion, polls → pinned TG Poll, blockquotes, article/live covers |
| X / Twitter | `https://x.com/<screen_name>` | `auth_token` cookie required; full long tweets, original images, highest-bitrate video, auto-linked @/#/$, retweets/quotes as blockquotes |
| Telegram | `https://t.me/<channel>` | Public channels only, no login; hyperlinks preserved, replies as blockquotes |

Local RSSHub: SSRF protection rejects private/loopback addresses by default; add the exact `host:port` to `RSS_TRUSTED_PRIVATE_HOSTS` (e.g. `rsshub:1200`). This allowlist applies only to the admin/trusted users (scope defaults to `admin`) and is not a general private-network allowlist; `RSS_NO_PROXY` only affects routing. Entry media inherits the allowlist only when it is same-origin with the subscribed feed (scheme+host+port); a public feed cannot use it to reach the internal network.

## Channel Management Panel (/set)

- Paginated channel/source lists (10 per page) with subscription counts; newest sources first; paused sources marked ⏸.
- Channel settings: interval (default 30 min), content mode, source display, media below (affects photo/video/animation captions only), link preview; per-source overrides, cleared by choosing the channel value (✏️/🔗 markers).
- Source actions: pause/resume, rename, content filter (per-source blacklist/whitelist; keywords separated by half-width/full-width commas or semicolons, matched against titles, case-insensitive); the channel page can resume all paused sources at once.
- Unsubscribe management: bulk unsubscribe runs immediately after confirmation and sends a text result notice; the channel list has a Chinese/English switch.

## Push & Media

- **Content modes**: `title_link` / `title_summary` (140-char summary, downloads only the first image/video) / `title_content` / `title_only` / `content_only` / `media_only`; **source display**: `source` / `author` / `none`.
- Long text is split by visible length (body 4096 / media caption 1024 UTF-16 code units); later parts reply to the first; HTML parse failures fall back to plain text.
- Photos and videos can share one album (max 10 per album, 30 per entry); GIFs are sent separately. Oversized/long images and formats not supported by sendPhoto (AVIF, SVG, HEIC, TIFF, etc.) are sent as documents to preserve the original (WebP/BMP are sent as photos when dimensions allow); document media is grouped.
- Streaming downloads avoid OOM; content hash + file_id dedup; videos include thumbnails and duration; optional image relay proxy; oversized videos fall back to text with a notice.

## Reliability & Scheduling

- A source is auto-disabled after consecutive failures (default 20) with a notice; the hourly self-heal job resumes it after a 3-hour cooldown.
- Persistent outbox retry: 1 min → 2 h exponential backoff, abandoned after 72 h; segment/op-level resumption.
- Platform health monitoring: two consecutive all-source failure rounds notify the admin, and recovery notifies again; media downloads retry on 5xx (5 s interval) but not 4xx, and failed entries are retried up to 3 fetch rounds before being sent degraded.
- Scheduled jobs: fetch (default 5 min, per-source interval), outbox retry (5 min), self-heal (hourly), cleanup (default 60 min).
- Notifications: channel owner (source disabled, media failure/oversized video, queued/72h abandoned) and admin (cookie expiry, platform errors/recovery); removing the bot from a channel soft-disables it and keeps its data, so re-adding restores the original owner and subscriptions.

## Configuration Reference (compose.yaml environment)

| Variable | Description | Default |
|---|---|---|
| `TELOXIDE_TOKEN` | BotFather token (**required**) | - |
| `ADMIN_USER_IDS` | Admin, **required, exactly one positive** user ID; can use /cookies and /loget, receives platform alerts | - |
| `ALLOWED_USER_IDS` | Whitelisted user IDs, comma-separated (optional; admin is implicitly allowed) | empty |
| `BOT_API_URL` | Local Bot API address; empty = official API (`BOT_PROXY` is ignored when set) | empty |
| `BOT_PROXY` | Bot API proxy (`http://`, `socks5://`, `socks5h://`); empty = direct | empty |
| `RSS_PROXY` | Proxy for RSS/platform fetches and media downloads (e.g. `socks5h://127.0.0.1:1080`; includes t.me) | empty |
| `RSS_NO_PROXY` | Domains bypassing the proxy (comma-separated) | empty |
| `RSS_TRUSTED_PRIVATE_HOSTS` | Trusted private-source allowlist, exact `host:port`; admin/trusted users only, not a general allowlist | empty |
| `RSS_TRUSTED_PRIVATE_SCOPE` | `admin` = admin only; `allowed` = admin + whitelist | admin |
| `MAX_MEDIA_DOWNLOAD_MB` | Per-file media download limit (MB, all media types); effective cap is 49MB on the official API | 100 |
| `MAX_SUBSCRIPTIONS_PER_CHAT` | Subscriptions per channel (0 = unlimited) | 99 |
| `DB_PATH` | SQLite path | data/rss_bot.db |
| `MEDIA_DIR` | Media directory | media |
| `LOG_DIR` | Log directory | logs |
| `RUST_LOG` | Log level (tracing EnvFilter) | info |

Proxies are controlled only by `RSS_PROXY`/`BOT_PROXY`: `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` are ignored; `BOT_PROXY` covers the Bot API only. See [compose.example.yaml](compose.example.yaml) for the full set of tunables (concurrency, cleanup, quotas, cache retention, etc.).

## Local Bot API (optional)

The official API has a 50MB per-file limit (this project uses 49MB). A local Bot API server raises the limit to 2GB, and rich-message photo/video attachments can be read by the server via `file://` (document attachments stay on multipart to keep their file name). All media types are still gated by `MAX_MEDIA_DOWNLOAD_MB` (default 100) and a 200MB per-entry media cap; downloads are validated by magic bytes/container header, so HTML error pages and HLS manifests are never sent as media.

| Mode | Trigger | Per-file cap |
|---|---|---|
| Official API | `BOT_API_URL` empty | 49MB |
| Local Bot API | `BOT_API_URL` set | 2GB |

On the bot-api side, set `TELEGRAM_API_ID`/`TELEGRAM_API_HASH` (my.telegram.org) and `TELEGRAM_LOCAL=1`, and mount the same host media directory in both containers (see [compose.example.yaml](compose.example.yaml) for a mount example). tgsrssr-side settings:

```yaml
services:
  tgsrssr:
    environment:
      BOT_API_URL: "http://127.0.0.1:8081"
      BOT_API_MEDIA_DIR: "/var/lib/telegram-bot-api-media"
```

`BOT_API_MEDIA_DIR` defaults to `/var/lib/telegram-bot-api-media`; the 2GB cap is controlled by whether `BOT_API_URL` is set. In local mode, `BOT_PROXY` is ignored: connectivity to Telegram is handled by the bot-api server.

## Developers

- Rust 1.96+; `cargo build --release --locked`; schema lives in [`migrations/`](migrations/).
- CI builds amd64/arm64 images on tag push and uploads Linux musl and Android (Termux) static binaries with a combined `SHA256SUMS`; Docker deployment is recommended on Linux, while macOS/Windows Docker Desktop requires bridge networking.

## Acknowledgements

References and data sources: [RSSHub](https://github.com/DIYgod/RSSHub), [RSS-to-Telegram-Bot](https://github.com/Rongronggg9/RSS-to-Telegram-Bot), [yt-dlp](https://github.com/yt-dlp/yt-dlp), [TwitterInternalAPIDocument](https://github.com/fa0311/TwitterInternalAPIDocument), [CPython](https://github.com/python/cpython). Runtime dependencies: [teloxide](https://github.com/teloxide/teloxide), [feed-rs](https://github.com/feed-rs/feed-rs), [reqwest](https://github.com/seanmonstar/reqwest), [SQLx](https://github.com/launchbadge/sqlx), [SQLite](https://www.sqlite.org/), [Tokio](https://github.com/tokio-rs/tokio), [Serde](https://github.com/serde-rs/serde), [tracing](https://github.com/tokio-rs/tracing). Deployment and tooling: [telegram-bot-api](https://github.com/aiogram/telegram-bot-api), [FFmpeg](https://ffmpeg.org/), [proxychains-ng](https://github.com/rofl0r/proxychains-ng), [cross](https://github.com/cross-rs/cross). See [Cargo.toml](Cargo.toml) for the remaining dependencies.

## License

Released under the MIT license. See [LICENSE](LICENSE).
