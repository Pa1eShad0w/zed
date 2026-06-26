# 更新日志

本文档记录 `perforce-integration` 分支在 upstream `zed-industries/zed` 之上的所有显著变更。

格式遵循 [Keep a Changelog 0.3.0](https://keepachangelog.com/zh-CN/0.3.0/)。
本项目尚未切独立 tag，所有已合入但未发布的变更统一归入 `[Unreleased]`。
分类关键字（`Added` / `Changed` / `Deprecated` / `Removed` / `Fixed` / `Security`）按规范保留英文；条目正文用中文。

> 文件名说明：规范要求 `CHANGELOG.md`，但本 fork 选用 `CHANGELOG.fork.md` 以避免与 upstream 未来可能新增的 `CHANGELOG.md` 在 rebase / merge 时冲突。这是有意识地偏离规范的唯一一处。

## [Unreleased]

### Added

- `ef1ac27` (2026-06-23) Perforce file history 表格新增两列：行首 `Revision` (`#<rev>`)、行尾 `Branch`（integration 来源 stream 第 2 段路径），整体布局 `[Revision, Description, Date, Author, Change, Branch]`。仅在 Perforce repo 的 `LogSource::Path` 下开启；git file history 与全仓 graph 的 5-tuple 列归一化、`Table::new(4)`、4-cell 行渲染等都保留不变。数据来源 `CommitData.file_revision` / `.branch`（filelog 解析时填入）。
- `2a51aa6` (2026-06-23) Changes 面板：待 resolve 的文件在 icon 右上角叠加 merge-conflict glyph（p4v 风格位置），与已有的左下 out-of-date 三角共存。走 `p4 fstat -Ru //client/...`（`-Ru` 只扫描未 resolve 的 opened 文件，绝不全量扫描）→ `perforce_unresolved_paths` trait（非 Perforce 默认空）→ git_store → 面板 `unresolved` set，每次 reload 重取。
- `7aaeef6` (2026-06-23) Changelist 范围的 diff 视图：tab 标题显示 `Changelist #N` / `Default Changelist`，`refresh()` 在 await base-load 之前跳过范围外文件，避免对整个仓库逐文件 `p4 print`；changelist header 右键菜单新增 `Open Changelist Diff` 与 `View in Swarm`。
- `f551c6a` `60f1bcf` `cf03a4f` (2026-06-22 ~ 06-23) 文件超期标记（out-of-date badge）：通过 `p4 fstat -Ro`（仅扫描已 opened 文件，不会全量扫描）识别落后于 head 的本地副本，在 Changes 面板的状态图标与项目面板树节点图标上叠加警告三角。
- `cf03a4f` (2026-06-22) 文件历史视图：复用 GitGraph 渲染 `p4 filelog -l -t -m <max>` 结果；CommitView 通过 `p4 describe` + `p4 where` + `p4 print #rev` / `#rev-1` 还原 diff；Changes 面板新增 `View File History` 入口。
- `fdbe70e` (2026-06-22) Changes 面板大幅增强：header 工具栏（折叠/展开）、文件右键菜单（Open Diff / Open Diff (File) / Shelve / Shelve and Revert / Revert / Unshelve / View in Swarm）、窗口重获焦点时自动刷新（仅在面板 active 且 Perforce-backed 时执行）；新增 `perforce.enabled` / `perforce.executable_path` / `perforce.swarm_host` 配置项。
- `0855b07` `5cf32f4` `4483f95` (2026-06-22) Markdown 渲染可读性增强：新增 `paragraph_line_height` 与 `inline_code_padding` 主题字段；inline-code 通过纯渲染层空白实现横向 padding，复制/选区走源映射不被吞；preview 视图默认 line-height 1.6 + inline-code 加深背景，agent 视图保持温和。
- `91d46fa` (2026-06-18) Perforce Changes 面板（左侧 dock，仅 Perforce workspace 可见）：渲染所有 pending changelist（Default 在最上，编号倒序）及其打开/shelved 文件；`uniform_list` 虚拟化 + 折叠分组（默认折叠）+ 多行 description tooltip；行拖到 changelist header 触发 `reopen -c`（pending）或 `unshelve -s ... -c ... -Af`（shelved）。
- `83d1b3e` (2026-06-18) 文件重命名时记录 Perforce move：`Project::rename_entry` hook 触发 `p4 edit src` + `p4 move -k src dst`（`-k` 让 p4 不动磁盘文件，由 Zed 自己 worktree rename）；跨 repo / 目录 rename 暂跳过，由 `move_on_file_rename` 设置控制。
- `7994c5b` (2026-06-18) 新增 `session.restore_unsaved_buffers_max_size` 配置项（默认 50 MB）：超过阈值的脏 buffer 不再序列化进 workspace SQLite，避免单次打开数百 MB 文件后续每次启动都触发数十秒卡顿。
- `2d7bb3e` (2026-06-16) Perforce auto-checkout（默认全部开启）：保存时 `p4 edit`、首次编辑时 `p4 edit`、创建文件时 `p4 add`、删除文件时 `p4 delete`；所有 hook 均 best-effort，失败仅 log 不阻断保存路径；新增 `perforce.{editOnFileSave, editOnFileModified, addOnFileCreate, deleteOnFileDelete}` 设置（均默认 true）。
- `87a390d` (2026-06-17) Perforce 行内 blame：`p4 annotate -c -q -dw //file#have` 取行→change 映射、`p4 filelog -l -m <max_history_count>` 取作者/时间/描述；`BlameEntry` 新增可选 `revision_label`，gutter 与 hover popover 在 Perforce 下显示 `@change` 而非缩写 sha（git 仍显示 sha）；新增 `perforce::Annotate` 命令。
- `ac8b491` (2026-06-17) 新增 `perforce.max_history_count` 配置项（默认 50），控制单文件 history 的 `p4 filelog -m <n>` 上限。
- `502324c` (2026-06-16) 原生 Perforce 后端 MVP（read-only status）：实现 `GitRepository` trait，复用 Zed 现有项目面板 gutter / git panel / inline diff 等 UI，无需新增 UI 组件；`status()` 走 `p4 opened`（小集时 scoped，否则 full），`load_committed_text()` 走 `p4 print #have`；工作区通过 `P4CONFIG` marker 发现（在 worktree 扫描中与 `.git` 并列，git 优先级更高）；连接参数（client / stream / depot / port / user / path）全部在运行时解析，无任何硬编码。

### Changed

- `31cdb27` (2026-06-23) Recent-projects 标题栏 popover 改为弹性宽度（min 20rem / max 48rem）：长项目名（含长 Perforce client 名）不再被截断，可在屏幕空间允许时按内容扩展，下限保持历史固定宽度故永远不会比之前更窄；Modal 风格仍为固定宽度（min == max），原 modal 布局不受影响。新增 `WidthConstraints` 内部抽象 + 3 个针对契约的单元测试。
- `2de0bba` (2026-06-23) 统一 source-control dock 按钮：在 Perforce workspace 中 Git panel 隐藏 dock 图标（新增缓存 `active_is_perforce`，repo 变化时通过 `is_perforce_resolved` 异步解析；lazy `peek` 不可靠），Perforce 面板也改用与 Git 面板相同的 `IconName::GitBranch`——因为两者互斥，dock 上只呈现一个 source-control 按钮，tooltip 区分（"Git Panel" / "Perforce Changes"）。git / 无 repo 的 workspace 不受影响。
- `a93f4dc` `300304b` `07275a7` (2026-06-23) 全面用 changelist 替换 synthetic-Oid 十六进制：graph commit detail header / 上下文菜单 / CommitView / file-history 列表 全部显示 `@<change>`（filelog 行额外带 `#<rev>`）；`Copy` 写入裸 changelist 数字而非 40 字符 hex；按钮 / 菜单文案改为 `Copy Changelist` / `Changelist <n>` / `Changelist`；Perforce commit 的 graph 上下文菜单在 `perforce.swarm_host` 配置时新增 `View in Swarm`。所有逻辑 gate 在 `CommitData.revision_label` 非空，git commit 仍显示 hex + `Copy SHA`，所有非 Perforce 的 `CommitData` 构造点 `revision_label: None`，行为完全不变。
- `7aaeef6` (2026-06-23) 文件右键菜单的全仓库 `Open Diff` 入口移除，等价入口下沉到 changelist header；文件级 diff 入口仍叫 `Open Diff`（单文件 SoloDiffView，`open_diff` 的 `solo` 参数移除）。
- `7aaeef6` (2026-06-23) Default changelist 下的 pending 文件原本会隐藏 `Shelve` / `Shelve and Revert` 入口，现在改为灰显并附说明文案（p4 不支持从 default changelist 直接 shelve），左侧 dock 视区下文案渲染在右侧以免被裁切。
- `fdbe70e` (2026-06-22) Perforce buffer 中的 inline diff hunk 控件隐藏 `Stage` / `Unstage`（Perforce 没有 index），并将 `Restore` 重命名为 `Revert`（语义是回到 `#have`）。
- `4483f95` (2026-06-22) Markdown 渲染采用分级主题值：agent 上下文用偏保守的间距，preview 上下文用完整可读性值（line-height 1.6 + inline-code padding + 深色背景）。
- `af2d5ea` (2026-06-16) Perforce scoped status 启动优化：调 `p4 opened` 前先按磁盘只读位预筛（同步且未 open 的文件一定只读），全部只读时跳过 `p4 opened` round-trip——实测消除启动时约 50 次零结果 `p4 opened` 调用；任何可写/缺失/目录/无法 stat 的路径保守 fallback 到 `p4 opened` 权威查询。
- `e4e530b` (2026-06-26) README 添加 fork 声明（维护者 @Pa1eShad0w、GPL 许可证信息），About 对话框标题与版本行标注 "Perforce fork"，满足 GPL v3 Section 5(a) 修改标记要求。

### Fixed

- `03ae625` (2026-06-25) 修复 agent / preview markdown 渲染中**行首** inline code 配合 `inline_code_padding` 时崩溃：行首的纯渲染左 pad（`append_styled_no_source`，无源映射）把该行首个 `SourceMapping` 顶离 `rendered_index 0`，对落在前导 pad gap 的 rendered index 做查找时 `binary_search` 返回 `Err(0)`，`ix - 1` 下溢成 `usize::MAX`，panic `index out of bounds: the len is N but the index is 18446744073709551615`（鼠标选区 / 取词触发）。修法：在 `append_styled_no_source` 作为行首第一个内容时补种 `{rendered_index: 0, source_index}`，恢复「行首首个 mapping 落在 rendered 0」不变式；三处 binary_search 查找原样不动。空 padding 提前 return 不种，upstream / 无 pad 路径逐字节不变。本 bug 由本 fork 的 inline-code padding 特性（`0855b07` `5cf32f4` `4483f95`）引入，upstream 默认空 padding 不受影响。
- `07275a7` (2026-06-23) 在 Perforce workspace 中按 Ctrl+Shift+N 打开新窗口偶发崩溃：`PerforcePanel::default_size` 仿照 Git panel 读取 Workspace entity 取持久化宽度，但 `default_size` 由 dock layout (`clamp_panel_size`) 调用，可能在 Workspace 已 lease 时执行，触发 double-lease panic。修法：改为返回 Git panel 的默认宽度 setting，不再触碰 Workspace entity。
- `4fd1cd8` (2026-06-23) [1] Perforce hunk-revert 后会残留幻影 diff hunk 并在下一次按键 panic：原路径走 git stage/unstage 会种入一条等待 `set_index_text` 重算的 pending hunk，而 Perforce 无 index，pending hunk 永远不被消解，其陈旧 buffer anchor 在已编辑 buffer 上 resolve 出越界 Point → `rope::point_to_offset` debug_panic。修法：Perforce hunk-revert 直接把 buffer 写回 `#have`，绕开 stage/unstage 机制。
- `4fd1cd8` (2026-06-23) [2] 在 diff multibuffer 中快速编辑时 sticky header 查询 panic（`point extends beyond row`）。Upstream 既存 bug——git diff + sticky_scroll 一样能触发，仅 debug build 触发（release 会自动 clamp）。修法：在 `outline_ranges_containing` 之前对 point 调 `clip_point(..., Bias::Left)`，越界才生效、合法点零开销。参见 [`zed-industries/zed#38556`](https://github.com/zed-industries/zed/issues/38556)、[`#54803`](https://github.com/zed-industries/zed/issues/54803)、[`#51077`](https://github.com/zed-industries/zed/pull/51077)、[`#58542`](https://github.com/zed-industries/zed/pull/58542)。
- `f551c6a` (2026-06-23) out-of-date badge 在真实工作区始终不显示：`p4 fstat -Ro` 的 `clientFile` 是本地路径（`C:\client\...`）而非 `//client/...`，原解析用错了映射函数；单测 fixture 同样写错故未暴露此 bug。同时修复 `p4 print #have` 在 Windows 返回 CRLF 而 Zed buffer 用 LF 导致整文件 diff base 全行不匹配——`load_committed_text` 现在规范化 #have 文本的行尾。
- `9844648` (2026-06-23) 行内 blame 在大文件上 CPU 100% 永不返回：旧的 `remap_annotation_to_content` LCS DP 表是 O(n*m)——50k 行 JSON ≈ 25 亿单元 / 10 GB，每次光标移动重跑。修法：buffer 未编辑时 identity-map（跳过 LCS），LCS 表上限 4M 单元，超阈值回退为 depot 对齐的 blame。
- `9844648` (2026-06-23) inline hunk `Revert` 弹出 `operation not supported` toast：Perforce 的 `set_index_text` 现为 no-op `Ok` 而非 unsupported 错误。
- `9844648` (2026-06-23) Changes 面板文件右键菜单新增 `Open File`；文件历史现在追踪 integrate/copy 历史（`p4 filelog -h`）；项目面板 out-of-date 刷新不再 gate 在同步 `is_perforce()` 的 lazy peek 上。
- `ba8f493` (2026-06-22) 打开 workspace-Y 文件夹却被识别成 workspace-X client（关键定位 bug）：原 `p4 info` 工作目录不在 workspace 内，`.p4config` 未被读取，落回 `p4 set P4CLIENT`。修法：用 Zed 已发现的 marker 文件绝对路径作为 `P4CONFIG`，每次 `p4` 调用都注入；`detect` 与 `PerforceRepository::new` 都从发现到的 `dot_git_abs_path` 接收 marker。
- `cdded05` (2026-06-22) 集成终端与后端的 `p4` 解析到错误 client：`p4` 还按 `PWD` 环境变量查找 `.p4config`，Zed 继承的陈旧 `PWD` 导致 `.p4config` 未被读取。修法：终端启动时把 `PWD` 钉到终端实际 cwd，后端每次 `p4` 调用都把 `PWD` 钉到 workspace 根目录（与 vscode-perforce 一致）。
- `56746ac` (2026-06-22) Perforce 面板的 dock 图标偶发不显示（死锁）：`is_perforce()` 走 `Shared::peek()`，状态未被驱动时返回 None，而 panel 因为 inactive 又不能驱动状态。修法：新增异步 `is_perforce_resolved`（await `repository_state`），结果缓存到 `active_is_perforce`，repo 变更触发 `cx.notify()` 让 dock 在 panel inactive 时也重新评估图标。
- `b9ef3df` (2026-06-22) `MarkdownStyle::default` 等未启用 inline-code padding 的调用方因 `current_source_index` 被无条件 +1 而产生的行尾复制偏移；padding 为空时不再 bump，确保 byte-identical fallback。
- `dda2c56` (2026-06-18) Perforce 行内 blame 在脏 buffer 上整体下移：`p4 annotate` 只能注解 `#have`，原直接返回 depot 对齐的注解。修法：通过 LCS 把 depot 注解投影到当前 buffer 内容（等价于 git `--contents -`），本地新增/编辑行不再显示别人的署名；blame 缓存改为只缓存 mtime 稳定的 depot 输入（annotation + #have + descriptions），廉价 remap 每次 blame 现算。
- `7f33103` (2026-06-17) 另存为到工作区时未触发 `p4 add`（`DiskState::New` 在 save-as 路径下不会被置位）——改为检查目标路径是否已存在于 worktree；同时修复删除已 open-for-add/edit 的文件时报 `can't delete (already opened ...)`：opened-for-add → `p4 revert`；opened-for-edit → `p4 revert -k` + `p4 delete`；未 open → 直接 `p4 delete`。
- `c09c597` (2026-06-17) 项目面板新建文件不自动 `p4 add`：原 hook 挂在 save 路径且依赖 `DiskState::New`，但项目面板创建路径不走 save。修法：改挂 `Project::create_entry`（与 `delete_entry` 对称）。
- `723fecf` (2026-06-17) `add_on_file_create` 从未真正生效：原顺序是 `p4 add` 在 `write_file` 前，而 `p4 add` 需要文件已在磁盘上。修法：拆分顺序——`Edit` 在 write 前（把同步只读文件改可写），`Add` 在 write 后；同时通过 `observe_release` 清理 `editOnFileModified` 去重集，避免编辑后未保存就关闭 buffer 时的条目泄漏。
- `a0e094c` (2026-06-17) 同一文件被反复发起 `p4 edit`（每次保存都打一次，服务器返回 `currently opened for edit`）：通过磁盘只读位预筛——可写文件认为已 open，跳过 `p4 edit`；首次 edit 打开文件后，后续每次保存都是 no-op。
- `4aea55b` `1cdfa8a` `32aad61` `4253120` `df9ff30` (2026-06-17) Perforce blame 行对齐与署名的连续多轮修复：annotate 行号在缺少行尾换行时不再串行（改用 `-ztag -F` 一记录一行）；跨 stream 时跟随分支历史（`-i`）；署名匹配 p4merge（`-dw`、`lower` change）；时间戳带 `+0000` 时区不再显示 `Just now`；CommitTooltip 的 gutter hover 显示 `@change` 而非 synthetic oid 十六进制。
- `3d246d7` (2026-06-16) Perforce auto-checkout 与磁盘写入的竞态：gpui Task 是 eager 立即执行，旧代码在 await pre-save `p4 edit` 之前就构造了 write task，导致 write 撞上仍只读的同步文件而失败。修法：把 write task 挪到 spawn 内部、`pre_save.await` 之后。

### Security

- `e193c40` (2026-06-22) **关键安全修复**——`detect` 原本无条件接受 `p4 info` 报告的任何 client，不校验它是否对应 Zed 实际打开的文件夹。当 `.p4config` 未生效时，环境 / registry 的 `P4CLIENT` 会泄漏到不相关的 workspace 上，Zed 据此启用 Perforce 并对错误 client 自动 checkout 文件（实测：在打开 workspace-Y 文件夹时执行了 `p4 move` 到 workspace-X stream）。修法：`detect` 强制校验打开的文件夹必须是 resolved client root 本身或其子目录（`workspace_root_matches`，对 Windows 路径分隔符 / 大小写做归一化、组件边界对齐），不匹配时拒绝构建 Perforce 后端并 log 出 actionable error。
- `0b7c813` (2026-06-17) 测试 fixture 中所有真实用户名 / change 编号 / 时间戳 / 描述全部替换为合成占位符；仓库内不再包含真实人员或 workspace 信息。
