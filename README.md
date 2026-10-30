# tgsrssr

**中文** · [English](#tgsrssr-english)

> **T**elegram's **R**SS **R**ust — RSS 订阅 Telegram 推送 Bot

把 RSS/Atom 源的新条目定时推送到 Telegram 频道/群组，并对**微博、X (Twitter)、Telegram 频道**提供原生抓取。支持富媒体相册、内容筛选、消息反查、断点续发与持久化重试。

## 效果展示

以下频道由 tgsrssr 实际驱动：

| 示例频道 | 内容来源 | 内容方式 |
|---|---|---|
| [People's Daily](https://t.me/peoplesdailyzh) | 微博原生抓取 | 仅内容 |
| [Elon Musk](https://t.me/elonmuskxtot) | X (Twitter) 原生抓取 | 仅内容 |
| [Hack News](https://t.me/hackernewsyc) | 标准 RSS 源 | 标题+源超链 |

## 为什么选 tgsrssr

- **原生平台抓取**：微博（长文全文、多图补全、视频 HD、投票转 Poll）、X（GraphQL 原图与最高码率视频）、Telegram 公开频道。
- **不丢消息**：发送失败先降级纯文本，仍失败进入 outbox 持久化重试（1 分钟→2 小时退避，72 小时放弃）。
- **富媒体管线**：图片/视频混排相册、GIF/文档处理、超限转文档、file_id 去重、流式下载。
- **长文/多图自动增强**：无媒体文本超出标准单条上限（约 4094 可见字符，预留分隔符）、带媒体文本超过 1024 字符、或参与富消息的图片/视频/文档总数超过 10 个时，自动改用 Telegram 富消息单条发送（图片、视频与长图/超大图转出的文档一并进入同一条消息；音频/文件附件随后单独发送并引用该条）；失败回退原有分段/相册路径。富消息需 Android/iOS 12.8+、Desktop 6.9.2+、macOS 12.9+、Web K（2026-07-03 后构建）；旧客户端会提示不支持。
- **多用户与权限分级**：单一管理员 + 白名单/开放注册；频道归属按邀请者判定，平台 cookie 全局共享。
- **轻量部署**：Rust 单二进制 + SQLite，Docker 一键启动；常驻内存约 22 MB、CPU 接近 0（媒体推送为主要开销，compose 默认限 1 GB 内存 / 1 CPU）；界面支持中文/English。

## 快速开始

### 前置条件

- Docker + Docker Compose（推荐 Linux；`network_mode: host` 仅 Linux 原生支持）
- Telegram Bot Token（[TG@BotFather](https://t.me/BotFather)）
- 你的 Telegram 用户 ID（[TG@userinfobot](https://t.me/userinfobot)）

### Docker 镜像（推荐）

```bash
cp compose.example.yaml compose.yaml
# 编辑 compose.yaml，填入 TELOXIDE_TOKEN 和 ADMIN_USER_IDS（管理员，仅一个正整数）
docker compose pull
docker compose up -d
docker logs -f tgsrssr
```

### 从源码构建

```bash
# 编辑 compose.yaml：注释 image 行，取消注释 build: .
docker compose up -d --build   # 首次约 5-10 分钟
```

### 静态二进制

从 [Releases](../../releases) 下载 Linux tar.gz（x86_64 / aarch64，musl 静态链接，包名含 `-musl`）与 `SHA256SUMS`，先校验再解压：

```bash
sha256sum -c --ignore-missing SHA256SUMS
tar xzf tgsrssr-vX.Y.Z-<arch>-musl.tar.gz
TELOXIDE_TOKEN=xxx ADMIN_USER_IDS=123 ./tgsrssr
```

> 二进制方式需自行安装 ffmpeg（视频缩略图/时长）。

Android（Termux）另有 `tgsrssr-vX.Y.Z-aarch64-android.tar.gz`（Android 7+/API 24，仅限 Termux，不是普通 Android 应用），同样需安装 ffmpeg；TLS 使用内置 Mozilla 根证书校验。

### 首次使用

1. 私聊 Bot 发送 `/start`
2. 把 Bot 加入频道或群组（频道需发消息权限）
3. 在**私聊**中发送 `/sub <频道ID或@用户名> <RSS地址>`

所有命令仅限私聊使用；群组/频道只作为推送目标。

需要推送大视频时，可部署本地 Bot API，详见下文「本地 Bot API（可选）」。

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

默认仅管理员与 `ALLOWED_USER_IDS` 白名单可用；设置 `OPEN_TO_ALL=true` 可对所有用户开放。管理员可在频道列表切换界面语言。
`/cookies`、`/loget` 仅管理员可用；平台 cookie 由管理员配置后所有用户共享。
**所有命令与按钮面板仅限私聊**：群组/频道内的命令会被静默忽略（不产生任何回复），`/find` 的转发频道消息也需在私聊中操作；`/sub`、`/test`、`/list` 必须显式给出频道参数（`@用户名` 或数字 ID）。

## 支持的内容源

### RSS/Atom

任意 RSS 2.0 / Atom 源，支持 ETag / Last-Modified 条件请求。

### 微博

订阅 `https://weibo.com/u/<uid>`。需登录 cookie（管理员经 `/cookies` 配置，失效私聊通知管理员）。

- 长微博全文、多图补全（>9 张）、视频 HD 升级。
- 转发引用块、表情转 emoji、投票转 Telegram Poll 并自动置顶。
- 头条文章、直播封面自动提取。

### X / Twitter

订阅 `https://x.com/<screen_name>`。需 `auth_token` cookie。

- GraphQL 抓取长推文全文、原图、最高码率视频。
- @mention / #hashtag / $cashtag 自动超链，转推/引用以引用块呈现。

### Telegram 频道

订阅 `https://t.me/<channel>`。仅公开频道，无需登录。

- 保留超链接，回复以引用块呈现。

## 频道管理面板（/set）

- 列表分页（每页 10 个），显示订阅数；源列表最新在前，停用源带 ⏸。
- 频道设置：推送间隔（默认 30 分钟）、内容模式、来源显示、媒体在下（仅影响图片/视频/动图说明位置）、链接预览。
- 源级设置：暂停/恢复、改名、内容筛选；推送间隔、内容模式、来源显示、媒体在下、链接预览可逐项覆盖，与频道相同会自动清除覆盖（✏️/🔗 标记）。
- 退订管理：一键退订全部（确认后在事务中执行，并发送文本结果通知；不再生成 OPML 文件）；频道页支持一键恢复全部停用源。
- 频道列表底部可切换中文/English 界面。

## 推送与媒体

- **内容模式**：`title_link` / `title_summary`（140 字摘要，只下载首个图片/视频）/ `title_content` / `title_only` / `content_only` / `media_only`。
- **来源显示**：`source` / `author` / `none`。
- 长文按可见长度分段（正文 4096 / 媒体说明 1024 UTF-16 码元），后续分段引用第一条；HTML 解析失败降级纯文本。
- 图片与视频可混排同一相册，每册最多 10 项、单条最多 30 项；GIF 单独发送。
- 超限大图、长图及 AVIF/SVG/HEIC/TIFF 等 sendPhoto 不支持的格式转文档保留原图（WebP/BMP 在尺寸允许时按图片发送）；转文档媒体成组发送。
- 流式下载防 OOM，内容哈希 + file_id 去重；视频自动附带缩略图和时长；可选图片中转反代；视频超限降级纯文本并通知。

## 内容筛选

- 源级黑名单/白名单，关键词可用半角/全角逗号或分号分隔，匹配标题、不区分大小写。
- 典型场景：过滤 GitHub releases 中的 beta/rc 版本。

## 可靠性与调度

- 连续抓取失败达到阈值（默认 20 次）自动停源并通知；每小时自愈检查，冷却 3 小时后恢复。
- Outbox 持久化重试：1 分钟→2 小时指数退避，72 小时放弃；支持分段/op 级续发。
- 平台级健康监控：连续 2 轮全部源失败通知管理员，恢复后再次通知。
- 媒体下载 5xx 重试（间隔 5 秒），4xx 不重试；Bot 被移出频道时清空该频道数据。
- 定时任务：抓取（默认 5 分钟，按源间隔）、outbox 重试（5 分钟）、自愈（每小时）、清理（默认 60 分钟）。

## 配置参考（compose.yaml environment）

| 变量 | 说明 | 默认 |
|---|---|---|
| `TELOXIDE_TOKEN` | BotFather token（**必填**） | - |
| `ADMIN_USER_IDS` | 管理员，**必须配置且只允许一个正整数**用户 ID；可用 /cookies、/loget，接收平台告警 | - |
| `ALLOWED_USER_IDS` | 用户白名单 ID，逗号分隔（可选；管理员隐式放行） | 空 |
| `OPEN_TO_ALL` | true = 对所有用户开放；false = 仅管理员+白名单 | false |
| `BOT_API_URL` | 本地 Bot API 地址；空 = 官方 API | 空 |
| `BOT_API_MEDIA_DIR` | bot-api 容器内的共享媒体目录（富消息附件 file:// 直读；非空 + `BOT_API_URL` 启用 2GB 上限） | /var/lib/telegram-bot-api-media |
| `RSS_PROXY` | 抓取 SOCKS5 代理（如 `socks5h://127.0.0.1:1080`） | 空 |
| `RSS_NO_PROXY` | 不走代理的域名（逗号分隔） | 空 |
| `RSS_INSECURE_HOSTS` | 跳过 TLS 验证的域名 | 空 |
| `IMG_RELAY_ENABLED` | 防盗链图片中转开关 | true |
| `IMG_RELAY_BASE_URL` | 中转服务地址（空 = 不启用） | 空 |
| `IMG_RELAY_DOMAINS` | 走中转的域名（逗号分隔） | 空 |
| `MAX_VIDEO_PUSH_MB` | 视频推送上限（MB）；官方 API 下实际生效上限 49MB | 100 |
| `MAX_FETCH_ERRORS` | 连续失败停源阈值 | 20 |
| `SCHEDULER_INTERVAL_MINUTES` | 抓取调度周期（分钟） | 5 |
| `FETCH_CONCURRENCY` | 抓取并发上限 | 8 |
| `DOWNLOAD_CONCURRENCY` | 下载并发上限 | 4 |
| `MEDIA_OUTBOUND_TTL_HOURS` | 媒体文件保留（小时；outbox 待重试媒体受清理保护） | 80 |
| `MEDIA_MAX_SIZE_GB` | 媒体目录总配额（GB） | 10 |
| `LONG_IMAGE_ASPECT_RATIO` | 长图判定高宽比阈值（有限正数；0/负数/NaN/∞ 告警并回退 8.0） | 8.0 |
| `CLEANUP_INTERVAL_MINUTES` | 清理任务周期（分钟） | 60 |
| `DB_PATH` | SQLite 路径 | data/rss_bot.db |
| `MEDIA_DIR` | 媒体目录 | media |
| `LOG_DIR` | 日志目录 | logs |
| `RUST_LOG` | 日志级别（tracing EnvFilter） | info |

完整可调项（清理、备份、缓存保留期等）及默认值见 [compose.example.yaml](compose.example.yaml)。

## 本地 Bot API（可选）

媒体上传统一由本进程从本地文件流式 multipart 上传（按块读取，堆内存常量，不整文件读入）。官方 API 的单文件上限为 50MB（本项目取 49MB）；部署本地 Bot API 后上限提高到 2GB，标准发送仍是流式 multipart，富消息的图片/视频附件可由 bot-api 服务器通过 `file://` 直读（文档附件仍走 multipart bytes 以保留文件名）。实际视频仍受 `MAX_VIDEO_PUSH_MB`（默认 100）约束。

| 模式 | 触发条件 | 传输方式 | 单文件上限 |
|---|---|---|---|
| 官方 API | `BOT_API_URL` 留空 | 客户端从本地文件流式 multipart | 49MB |
| 本地 Bot API | `BOT_API_URL` 已设且 `BOT_API_MEDIA_DIR` 非空 | 客户端流式 multipart；富消息图片/视频附件可 file:// 服务器直读 | 2GB |

富消息的 `file://` 附件要求两个容器挂载同一宿主媒体目录；标准发送从 bot 自己的 `MEDIA_DIR` 流式读取，不依赖 bot 容器访问 `BOT_API_MEDIA_DIR`。以下仅列出相关配置：

```yaml
# telegram-bot-api
services:
  telegram-bot-api:
    environment:
      TELEGRAM_API_ID: "<api_id>"      # my.telegram.org，非 BotFather token
      TELEGRAM_API_HASH: "<api_hash>"
      TELEGRAM_LOCAL: "1"
    volumes:
      - ./media:/var/lib/telegram-bot-api-media

# tgsrssr
services:
  tgsrssr:
    environment:
      BOT_API_URL: "http://127.0.0.1:8081"
      BOT_API_MEDIA_DIR: "/var/lib/telegram-bot-api-media"
    volumes:
      - ../telegram-bot-api/media:/app/media
```

`BOT_API_MEDIA_DIR` 默认即 `/var/lib/telegram-bot-api-media`，仅当 bot-api 容器内路径不同才需修改。由于该默认值非空，**是否启用本地 Bot API 模式（2GB 上限）只看 `BOT_API_URL` 是否设置**。

## 通知

- **频道属主**（私聊）：源自动停用、媒体下载/发送失败、视频超限跳过、推送入队/72h 放弃。
- **管理员**：平台 cookie 失效、平台访问异常与恢复。

## 开发者

- Rust 1.96+；`cargo build --release --locked`，产物在 `target/release/tgsrssr`。
- SQLite 表结构见 [`migrations/`](migrations/)；CI 在 tag push 时构建 linux/amd64 + linux/arm64 镜像，并上传 Linux musl 与 Android（Termux）静态二进制及合并的 `SHA256SUMS` 校验文件。
- Docker 部署推荐 Linux；macOS/Windows Docker Desktop 需改用 bridge 网络。

## 致谢

- **参考项目与数据来源**：[RSSHub](https://github.com/DIYgod/RSSHub)、[RSS-to-Telegram-Bot](https://github.com/Rongronggg9/RSS-to-Telegram-Bot)、[yt-dlp](https://github.com/yt-dlp/yt-dlp)、[TwitterInternalAPIDocument](https://github.com/fa0311/TwitterInternalAPIDocument)、[CPython](https://github.com/python/cpython)
- **运行时依赖**：[teloxide](https://github.com/teloxide/teloxide)、[feed-rs](https://github.com/feed-rs/feed-rs)、[reqwest](https://github.com/seanmonstar/reqwest)、[SQLx](https://github.com/launchbadge/sqlx)、[SQLite](https://www.sqlite.org/)、[Tokio](https://github.com/tokio-rs/tokio)、[Serde](https://github.com/serde-rs/serde)、[tracing](https://github.com/tokio-rs/tracing)
- **部署与工具**：[telegram-bot-api](https://github.com/aiogram/telegram-bot-api)、[FFmpeg](https://ffmpeg.org/)、[proxychains-ng](https://github.com/rofl0r/proxychains-ng)、[cross](https://github.com/cross-rs/cross)

其余依赖见 [Cargo.toml](Cargo.toml)。感谢所有直接与间接依赖的维护者。

## 许可

基于 MIT 许可发布，见 [LICENSE](LICENSE)。

---

# tgsrssr (English)

[中文](#tgsrssr) · **English**

> **T**elegram's **R**SS **R**ust — an RSS subscription Telegram push bot

Periodically pushes new entries from RSS/Atom feeds to Telegram channels/groups, with **native fetching for Weibo, X (Twitter), and Telegram channels**. Includes rich media albums, content filtering, message reverse lookup, resumable sending, and persistent retries.

## Showcase

The following channels are powered by tgsrssr:

| Demo Channel | Content Source | Content Mode |
|---|---|---|
| [People's Daily](https://t.me/peoplesdailyzh) | Weibo native fetch | Content only |
| [Elon Musk](https://t.me/elonmuskxtot) | X (Twitter) native fetch | Content only |
| [Hack News](https://t.me/hackernewsyc) | Standard RSS feed | Title + source link |

## Why tgsrssr

- **Native platform fetching**: Weibo (full long-form text, multi-image completion, HD video, polls → TG Poll), X (GraphQL original images and highest-bitrate video), public Telegram channels.
- **No message loss**: failed sends fall back to plain text, then enter a persistent outbox (1 min → 2 h backoff, abandoned after 72 h).
- **Rich media pipeline**: mixed photo/video albums, GIF/document handling, oversized media → documents, file_id dedup, streaming downloads.
- **Automatic long-post/multi-media enhancement**: text-only posts exceeding the standard single-message limit (about 4,094 visible chars after the separator reserve), media posts with over 1,024 characters, or more than 10 photos/videos/documents participating in the rich message are sent as a single Telegram rich message (photos, videos, and documents converted from long or oversized images are carried in the same message; audio/file attachments follow as separate messages quoting it); failures fall back to the existing split/album path. Rich messages require Android/iOS 12.8+, Desktop 6.9.2+, macOS 12.9+, and Web K builds from 2026-07-03 or later; older clients show an unsupported message.
- **Multi-user + admin tiers**: one admin + whitelist/open registration; channel ownership by inviter; shared platform cookies.
- **Lightweight deployment**: single Rust binary + SQLite, one-command Docker startup; about 22 MB resident memory and near-zero CPU (media delivery is the main cost; the bundled compose file caps it at 1 GB RAM / 1 CPU); Chinese/English UI.

## Quick Start

### Prerequisites

- Docker + Docker Compose (Linux recommended; `network_mode: host` is Linux-only)
- A Telegram Bot Token ([TG@BotFather](https://t.me/BotFather))
- Your Telegram user ID ([TG@userinfobot](https://t.me/userinfobot))

### Docker image (recommended)

```bash
cp compose.example.yaml compose.yaml
# Edit compose.yaml: set TELOXIDE_TOKEN and ADMIN_USER_IDS (exactly one positive admin ID)
docker compose pull
docker compose up -d
docker logs -f tgsrssr
```

### Build from source

```bash
# Edit compose.yaml: comment out image, uncomment build: .
docker compose up -d --build   # ~5-10 minutes on first run
```

### Static binary

Download a Linux tar.gz from [Releases](../../releases) (x86_64 / aarch64, musl static; the archive name includes `-musl`) plus `SHA256SUMS`, verify, then extract:

```bash
sha256sum -c --ignore-missing SHA256SUMS
tar xzf tgsrssr-vX.Y.Z-<arch>-musl.tar.gz
TELOXIDE_TOKEN=xxx ADMIN_USER_IDS=123 ./tgsrssr
```

> The binary approach requires ffmpeg installed separately (video thumbnails/duration).

Android (Termux) builds are published as `tgsrssr-vX.Y.Z-aarch64-android.tar.gz` (Android 7+/API 24, Termux only; not a regular Android app) and also require ffmpeg. The Android build validates HTTPS with bundled Mozilla roots.

### First use

1. Send `/start` to the bot in a private chat
2. Add the bot to your channel or group (channels need permission to post)
3. Send `/sub <channel ID or @username> <RSS URL>` **in a private chat**

All commands are private-chat only; groups/channels are push targets.

To push large videos, deploy a local Bot API server — see "Local Bot API (optional)" below.

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

Only the admin and `ALLOWED_USER_IDS` whitelist can use the bot by default; set `OPEN_TO_ALL=true` to open it to everyone. The admin can switch the UI language from the channel list.
`/cookies` and `/loget` are admin-only; platform cookies configured by the admin are shared by all users.
**All commands and inline panels are private-chat only**: commands typed in groups/channels are silently ignored (no reply), and forwarding a channel message for `/find` must also happen in a private chat. `/sub`, `/test`, and `/list` must name the channel explicitly (`@username` or numeric ID).

## Supported Sources

### RSS/Atom

Any RSS 2.0 / Atom feed, with ETag / Last-Modified conditional requests.

### Weibo

Subscribe with `https://weibo.com/u/<uid>`. Requires a login cookie (admin configures via `/cookies`; expiry is DM'd to the admin).

- Full long-form text, multi-image completion (>9 images), HD video upgrade.
- Retweets as blockquotes, emoji conversion, polls → Telegram Poll with auto-pin.
- Article and live-stream cover extraction.

### X / Twitter

Subscribe with `https://x.com/<screen_name>`. Requires an `auth_token` cookie.

- GraphQL fetching: full long tweets, original images, highest-bitrate video.
- Auto-linking for @mentions / #hashtags / $cashtags; retweets/quotes rendered as blockquotes.

### Telegram channels

Subscribe with `https://t.me/<channel>`. Public channels only, no login required.

- Preserves hyperlinks; replies rendered as blockquotes.

## Channel Management Panel (/set)

- Paginated lists (10 per page) with subscription counts; newest sources first; paused sources marked ⏸.
- Channel settings: interval (default 30 min), content mode, source display, media below (affects photo/video/animation captions only), link preview.
- Source settings: pause/resume, rename, content filter; per-source overrides for interval, content mode, source display, media below, and link preview (choosing the channel value clears the override; ✏️/🔗 markers).
- Unsubscribe management: bulk unsubscribe runs immediately after confirmation and sends a text result notice (no OPML file); channel page can resume all paused sources at once.
- The channel list has a Chinese/English language switch.

## Push & Media

- **Content modes**: `title_link` / `title_summary` (140-char summary, downloads only the first image/video) / `title_content` / `title_only` / `content_only` / `media_only`.
- **Source display**: `source` / `author` / `none`.
- Long text is split by visible length (body 4096 / media caption 1024 UTF-16 code units); later parts reply to the first; HTML parse failures fall back to plain text.
- Photos and videos can share one album (max 10 per album, 30 per entry); GIFs are sent separately.
- Oversized/long images and formats not supported by sendPhoto (AVIF, SVG, HEIC, TIFF, etc.) are sent as documents to preserve the original (WebP/BMP are sent as photos when dimensions allow); document media is grouped.
- Streaming downloads avoid OOM; content hash + file_id dedup; videos include thumbnails and duration; optional image relay proxy; oversized videos fall back to text with a notice.

## Content Filtering

- Per-source blacklist/whitelist; keywords separated by half-width/full-width commas or semicolons, matched against titles, case-insensitive.
- Typical use: filtering beta/rc releases from GitHub releases feeds.

## Reliability & Scheduling

- A source is auto-disabled after consecutive failures (default 20) with a notice; the hourly self-heal job resumes it after a 3-hour cooldown.
- Persistent outbox retry: 1 min → 2 h exponential backoff, abandoned after 72 h; segment/op-level resumption.
- Platform health monitoring: two consecutive all-source failure rounds notify the admin; recovery notifies again.
- Media downloads retry on 5xx (5 s interval), not on 4xx; removing the bot from a channel purges that channel's data.
- Scheduled jobs: fetch (default 5 min, per-source interval), outbox retry (5 min), self-heal (hourly), cleanup (default 60 min).

## Configuration Reference (compose.yaml environment)

| Variable | Description | Default |
|---|---|---|
| `TELOXIDE_TOKEN` | BotFather token (**required**) | - |
| `ADMIN_USER_IDS` | Admin, **required, exactly one positive** user ID; can use /cookies and /loget, receives platform alerts | - |
| `ALLOWED_USER_IDS` | Whitelisted user IDs, comma-separated (optional; admin is implicitly allowed) | empty |
| `OPEN_TO_ALL` | true = open to all users; false = admin + whitelist only | false |
| `BOT_API_URL` | Local Bot API address; empty = official API | empty |
| `BOT_API_MEDIA_DIR` | Shared media dir inside the bot-api container (rich-message file:// attachments; non-empty + `BOT_API_URL` enables the 2GB cap) | /var/lib/telegram-bot-api-media |
| `RSS_PROXY` | Fetch SOCKS5 proxy (e.g. `socks5h://127.0.0.1:1080`) | empty |
| `RSS_NO_PROXY` | Domains bypassing the proxy (comma-separated) | empty |
| `RSS_INSECURE_HOSTS` | Domains skipping TLS verification | empty |
| `IMG_RELAY_ENABLED` | Hotlink-protection image relay toggle | true |
| `IMG_RELAY_BASE_URL` | Relay service address (empty = disabled) | empty |
| `IMG_RELAY_DOMAINS` | Domains routed through the relay (comma-separated) | empty |
| `MAX_VIDEO_PUSH_MB` | Video push limit (MB); effective cap is 49MB on the official API | 100 |
| `MAX_FETCH_ERRORS` | Consecutive-failure threshold to disable a source | 20 |
| `SCHEDULER_INTERVAL_MINUTES` | Fetch scheduling interval (minutes) | 5 |
| `FETCH_CONCURRENCY` | Max concurrent fetches | 8 |
| `DOWNLOAD_CONCURRENCY` | Max concurrent downloads | 4 |
| `MEDIA_OUTBOUND_TTL_HOURS` | Media file retention (hours; media for pending outbox retries is protected) | 80 |
| `MEDIA_MAX_SIZE_GB` | Total media directory quota (GB) | 10 |
| `LONG_IMAGE_ASPECT_RATIO` | Aspect-ratio threshold for long-image detection (finite positive; 0/negative/NaN/∞ warn and fall back to 8.0) | 8.0 |
| `CLEANUP_INTERVAL_MINUTES` | Cleanup job interval (minutes) | 60 |
| `DB_PATH` | SQLite path | data/rss_bot.db |
| `MEDIA_DIR` | Media directory | media |
| `LOG_DIR` | Log directory | logs |
| `RUST_LOG` | Log level (tracing EnvFilter) | info |

The full set of tunables (cleanup, backup, cache retention, etc.) and their defaults are in [compose.example.yaml](compose.example.yaml).

## Local Bot API (optional)

Media uploads are streamed from the local file as multipart (read in chunks; constant heap memory, no whole-file buffering). The official API has a 50MB per-file limit (this project uses 49MB). Deploying a local Bot API server raises the limit to 2GB; standard sends still use streaming multipart, while rich-message photo/video attachments can be read by the server via `file://` (document attachments stay on multipart bytes to keep their file name). Videos are still gated by `MAX_VIDEO_PUSH_MB` (default 100).

| Mode | Trigger | Transport | Per-file cap |
|---|---|---|---|
| Official API | `BOT_API_URL` empty | client-side streaming multipart | 49MB |
| Local Bot API | `BOT_API_URL` set and `BOT_API_MEDIA_DIR` non-empty | client-side streaming multipart; rich photo/video attachments may use server-side `file://` | 2GB |

Rich-message `file://` attachments require both containers to mount the same host media directory; standard sends read the bot's own `MEDIA_DIR` and do not require the bot container to see `BOT_API_MEDIA_DIR`. Only the relevant settings are shown:

```yaml
# telegram-bot-api
services:
  telegram-bot-api:
    environment:
      TELEGRAM_API_ID: "<api_id>"      # from my.telegram.org, not the BotFather token
      TELEGRAM_API_HASH: "<api_hash>"
      TELEGRAM_LOCAL: "1"
    volumes:
      - ./media:/var/lib/telegram-bot-api-media

# tgsrssr
services:
  tgsrssr:
    environment:
      BOT_API_URL: "http://127.0.0.1:8081"
      BOT_API_MEDIA_DIR: "/var/lib/telegram-bot-api-media"
    volumes:
      - ../telegram-bot-api/media:/app/media
```

`BOT_API_MEDIA_DIR` defaults to `/var/lib/telegram-bot-api-media`; override it only if the bot-api container uses a different path. Because the default is non-empty, **local Bot API mode (2GB cap) is controlled by whether `BOT_API_URL` is set**.

## Notifications

- **Channel owner** (private message): source auto-disabled, media download/send failure, oversized video skipped, push queued/abandoned after 72h.
- **Admin**: platform cookie expiry, platform access errors and recovery.

## Developers

- Rust 1.96+; `cargo build --release --locked`; output at `target/release/tgsrssr`.
- SQLite schema lives in [`migrations/`](migrations/); CI builds linux/amd64 + linux/arm64 images and uploads Linux musl and Android (Termux) static binaries with a combined `SHA256SUMS` file on tag push.
- Docker deployment is recommended on Linux; macOS/Windows Docker Desktop requires bridge networking.

## Acknowledgements

- **References and data sources**: [RSSHub](https://github.com/DIYgod/RSSHub), [RSS-to-Telegram-Bot](https://github.com/Rongronggg9/RSS-to-Telegram-Bot), [yt-dlp](https://github.com/yt-dlp/yt-dlp), [TwitterInternalAPIDocument](https://github.com/fa0311/TwitterInternalAPIDocument), [CPython](https://github.com/python/cpython)
- **Runtime dependencies**: [teloxide](https://github.com/teloxide/teloxide), [feed-rs](https://github.com/feed-rs/feed-rs), [reqwest](https://github.com/seanmonstar/reqwest), [SQLx](https://github.com/launchbadge/sqlx), [SQLite](https://www.sqlite.org/), [Tokio](https://github.com/tokio-rs/tokio), [Serde](https://github.com/serde-rs/serde), [tracing](https://github.com/tokio-rs/tracing)
- **Deployment and tooling**: [telegram-bot-api](https://github.com/aiogram/telegram-bot-api), [FFmpeg](https://ffmpeg.org/), [proxychains-ng](https://github.com/rofl0r/proxychains-ng), [cross](https://github.com/cross-rs/cross)

See [Cargo.toml](Cargo.toml) for the remaining dependencies. Thanks also to all maintainers of direct and transitive dependencies.

## License

Released under the MIT license. See [LICENSE](LICENSE).
