# 发布与 Windows 代码签名

推送与 `Cargo.toml` 版本一致的 `v*` 标签会触发 `.github/workflows/release.yml`。流水线会重新测试、构建 `shiyue.exe` 与 `shiyue-cli.exe`、生成按用户安装的 Inno Setup `setup.exe`、打包便携 ZIP，并创建 GitHub Release。安装器和便携包都会带上应用图标。

## 签名配置

在 GitHub 仓库 Actions secrets 中设置：

- `WINDOWS_CERTIFICATE_BASE64`：PFX 文件的 Base64 内容。
- `WINDOWS_CERTIFICATE_PASSWORD`：PFX 密码。

配置证书后，流水线使用 Windows SDK `signtool` 和 RFC 3161 时间戳签署两个可执行文件及安装器。没有配置证书时仍会生成未签名 Release；这适合证书采购前使用，但 Windows 会继续显示未知发布者。

PowerShell 可用下面的命令生成 secret 内容（不要提交输出或 PFX）：

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes('publisher.pfx'))
```

## 发布命令

```powershell
cargo test --locked
git tag vX.Y.Z
git push origin vX.Y.Z
```

工作流会拒绝标签版本与 `Cargo.toml` 不一致的发布。
