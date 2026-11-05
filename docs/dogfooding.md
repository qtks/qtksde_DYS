# Dogfooding 指南

## 1. 安装包在哪

- **正式版本**：GitHub Releases（<https://github.com/qtks/qtksde_DYS/releases>），按平台下载。
- **未发版的构建**：对应提交的 Actions run 页面 → Artifacts → `panopticon-<平台>.zip`，解压后取安装包。

| 平台 | 文件 | 安装方式 |
| --- | --- | --- |
| Linux (x86_64) | `Panopticon_<v>_amd64.deb` | `sudo dpkg -i`，缺依赖时 `sudo apt-get -f install` |
| Linux (x86_64) | `Panopticon-<v>-1.x86_64.rpm` | `sudo rpm -i`（或 dnf/zypper） |
| Linux (任意) | `Panopticon_<v>_amd64.AppImage` | `chmod +x` 后直接运行；无 FUSE 时 `--appimage-extract-and-run` |
| Windows (x64) | `Panopticon_<v>_x64-setup.exe` / `.msi` | 双击安装，当前用户目录，不需要管理员权限 |
| macOS (Apple Silicon) | `Panopticon_<v>_aarch64.dmg` | 打开 dmg，拖进 Applications |
| macOS (Intel) | `Panopticon_<v>_x64.dmg` | 同上 |

## 2. 首次启动注意

**macOS（未签名时）**
1. 应用被 Gatekeeper 拦下时，在 Finder 里右键 → 打开 → 再点"打开"。
2. 或执行 `xattr -dr com.apple.quarantine /Applications/Panopticon.app`。
3. 若提示"已损坏"，先确认下载来源，再重复第 2 步。

**Windows**
1. SmartScreen 提示"Windows 已保护你的电脑" → 更多信息 → 仍要运行。
2. 安装后首次启动若防火墙弹窗，只允许本地回环（应用只监听 `127.0.0.1`）。
3. 端口 8787 被占用时应用会自动换端口，不需要手动关别的程序。

**Linux**
- deb/rpm 安装后在应用菜单找 Panopticon；AppImage 需要可执行权限。
- 无桌面环境（headless）下不适用，WebKitGTK 需要图形会话。

**通用**
- 首次启动会自动创建空索引库，界面显示"暂无卷"，这是正常的；先执行扫描再浏览。
- 索引库默认在启动目录的 `index.db`，可用 `PANOPTICON_DB` 指定其他路径。

## 3. 重点反馈类型

请优先记录下面几类问题，它们对应本轮验收的薄弱点：

1. **扫描崩溃 / 卡顿**：哪个目录、文件数量级、卡住的位置、是否可复现；是否有错误弹窗或终端输出。
2. **搜索结果不对**：查询词、期望结果、实际前 5 条；属于"找不到"还是"排序不对"；是否中文模糊查询。
3. **重复文件误报**：被判重复的文件路径对、真实大小、是否确实相同；特别关注 0 字节文件和硬链接。
4. **建议清单误判**：建议把哪些目录归档到一起、为什么你认为不合理（相邻工程、临时目录、构建产物等）。
5. **其他**：项目识别错误（把普通目录当工程、或漏掉真工程）、摘要内容离谱、界面卡死、安装失败。

尽量附上：操作系统与版本、安装包类型、索引规模（文件数）、是否能稳定复现。

## 4. 反馈模板（可直接复制）

```text
【环境】OS/版本：
【安装包】deb / rpm / AppImage / exe / msi / dmg：
【索引规模】约 __ 万文件，__ 个目录
【问题类型】扫描崩溃/卡顿 | 搜索不准 | 重复误报 | 建议误判 | 项目识别 | 其他
【复现步骤】
1.
2.
3.
【期望结果】
【实际结果】
【可复现】每次 / 偶尔 / 只出现一次
【附件】截图、日志、相关路径（可脱敏）
```

## 5. 记录去向

- 每个问题开一个 issue，标题用 `[dogfood] <问题类型> <一句话>`。
- 崩溃和卡顿类问题附上终端输出；如果是从命令行启动，可直接复制 stderr。
- 每周汇总一次：新增问题数、阻塞问题、已修复问题，作为下一轮优先级依据。
