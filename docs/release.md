# 发布流程

## 1. 版本号

一次发布需要同步三处版本：

| 文件 | 字段 |
| --- | --- |
| `src-tauri/tauri.conf.json` | `version` |
| `src-tauri/Cargo.toml` | `[package] version` |
| `ui/package.json` | `version` |

`Cargo.lock` 会在下一次 `cargo build` 时自动更新，一并提交。

## 2. 触发打包

推送 `main` 或手动触发 `workflow_dispatch`：只上传 Actions artifact，不创建 Release。

推送 `v*` tag：打包完成后自动创建 GitHub Release，附上所有安装包。

```bash
git tag -a v0.1.0 -m "Panopticon v0.1.0"
git push origin v0.1.0
```

不要在已有 tag 上改内容后 `push --force`；重新发版请递增版本号并打新 tag。

## 3. CI 矩阵

| Job | Runner | 产物 |
| --- | --- | --- |
| `check` | ubuntu-latest | `cargo fmt --check`、`clippy -D warnings`、`cargo test --workspace` |
| `bundle linux-x64` | ubuntu-latest | `.deb`、`.rpm`、`.AppImage` |
| `bundle windows-x64` | windows-latest | `.msi`、`-setup.exe`（NSIS） |
| `bundle macos-arm64` | macos-14 | `.dmg`、`.app` |
| `bundle macos-x64` | macos-15-intel | `.dmg`、`.app` |

- PR 与功能分支只跑 `check`，不消耗 macOS 分钟数，也不产出安装包。
- `concurrency` 按 `workflow + ref` 取消旧任务；tag 构建失败时先看失败平台的日志，`RUST_BACKTRACE=1` 已开启。
- artifact 名称为 `panopticon-<平台>`，保留 14 天。Release 会合并四个 artifact 后上传。

## 4. 签名（可选，缺 secret 时产出未签名包）

CI 只有在检测到对应 secret 时才执行签名步骤，否则构建仍然成功。密钥只从环境变量读取，不写入仓库、日志或产物。

### 4.1 Tauri 更新签名（自动更新 .sig）

| Secret | 说明 |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | `tauri signer generate` 生成的私钥全文（多行） |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 生成时设置的密码；未设密码可留空 |
| `TAURI_UPDATER_PUBKEY` | 同名 `.pub` 文件的内容；`createUpdaterArtifacts` 必须有它 |

私钥与公钥**同时存在**时 CI 才会追加
`--config '{"plugins":{"updater":{"pubkey":"..."}},"bundle":{"createUpdaterArtifacts":true}}'`，
产物旁出现 `.sig`。只配置私钥会给出 warning 并跳过签名，安装包仍会生成。
后续启用自动更新还需要把 endpoint 写进 `plugins.updater`。

生成密钥（只做一次，私钥务必离线备份，`.pub` 内容填入 secret）：

```bash
./ui/node_modules/.bin/tauri signer generate -w ~/.tauri/panopticon.key
cat ~/.tauri/panopticon.key.pub
```

本地验证（使用临时密钥，产物应出现 `.deb.sig`）：

```bash
TAURI_SIGNING_PRIVATE_KEY="$(cat /tmp/pan.key)" \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD=pw \
./ui/node_modules/.bin/tauri build --bundles deb \
  --config /tmp/updater.json   # {"plugins":{"updater":{"pubkey":"..."}},"bundle":{"createUpdaterArtifacts":true}}
```

### 4.2 Windows 代码签名

| Secret | 说明 |
| --- | --- |
| `WINDOWS_CERTIFICATE_BASE64` | `.pfx` 文件的 base64（`base64 -w0 cert.pfx`） |
| `WINDOWS_CERTIFICATE_PASSWORD` | `.pfx` 密码 |

CI 会导入到 `Cert:\CurrentUser\My`，用 SHA-256 + DigiCert 时间戳对 MSI/NSIS 签名。证书必须是代码签名用途。

### 4.3 Apple 签名与公证

| Secret | 说明 |
| --- | --- |
| `APPLE_CERTIFICATE_BASE64` | Developer ID Application `.p12` 的 base64 |
| `APPLE_CERTIFICATE_PASSWORD` | `.p12` 密码 |
| `APPLE_SIGNING_IDENTITY` | 例如 `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | 公证用 Apple ID |
| `APPLE_PASSWORD` | App 专用密码（不是账号密码） |
| `APPLE_TEAM_ID` | 10 位 Team ID |

也可以用 App Store Connect API key 代替 `APPLE_ID`/`APPLE_PASSWORD`（Tauri 支持 `APPLE_API_KEY`、`APPLE_API_ISSUER`、`APPLE_API_KEY_PATH`），届时在 workflow 的 env 里增补这三项即可。

缺 secret 时 macOS 产物为未签名 dmg，首次打开会被 Gatekeeper 拦截，处理方式见 `docs/dogfooding.md`。

## 5. 产物校验

Release 页面应包含：

- `Panopticon_<version>_amd64.deb` / `.rpm` / `.AppImage`
- `Panopticon_<version>_x64-setup.exe` / `_x64_en-US.msi`
- `Panopticon_<version>_{aarch64,x64}.dmg`
- 可选：`.sig`、`.app.tar.gz`

本地校验示例：

```bash
dpkg-deb -I bundle/deb/*.deb          # 包元数据
dpkg-deb -c bundle/deb/*.deb | head  # 文件清单
ldd /usr/bin/panopticon | grep "not found"   # 安装后应为空
```

RPM 在没有 `rpm` 命令时，可用仓库里的最小解析脚本或 Python `rpmfile` 校验；至少确认头部魔数、包名和 payload 可解压。

## 6. 发布后检查

1. Actions 全绿，四个平台 artifact 都存在。
2. Release 附件文件名与版本号一致，大小非 0。
3. 抽一台机器安装、启动，确认首屏能显示索引列表。
4. 记录已知限制（例如滚动基准只在 Linux 上自动跑）。
