#requires -Version 5.1
<#
.SYNOPSIS
    DepotManager 一键发布脚本：同步版本号 -> tauri build -> git tag -> GitHub Release。

.DESCRIPTION
    1. （可选）将 -Version 写入 package.json / tauri.conf.json / Cargo.toml 并提交
    2. 执行 bun run tauri build（产出 NSIS .exe 与 MSI .msi 安装包）
    3. 创建并推送 git tag v<version>
    4. 通过 GitHub REST API 创建 Release 并上传安装包
    GitHub 令牌取自 Windows 凭据管理器（git credential fill），需有 repo 权限。

.EXAMPLE
    .\scripts\release.ps1
    .\scripts\release.ps1 -Version 0.2.0
    .\scripts\release.ps1 -Version 0.2.0 -Notes "修复若干问题" -Force
#>
[CmdletBinding()]
param(
    # 目标版本号（缺省 = 使用 tauri.conf.json 当前版本）
    [string]$Version,
    # Release 说明（缺省自动生成）
    [string]$Notes,
    # 跳过构建（仅重新打 tag / 上传已有产物）
    [switch]$SkipBuild,
    # 覆盖已存在的同名 tag / Release
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot   = Resolve-Path (Join-Path $PSScriptRoot '..')
$TauriConf  = Join-Path $RepoRoot 'src-tauri\tauri.conf.json'
$CargoToml  = Join-Path $RepoRoot 'src-tauri\Cargo.toml'
$PackageJson = Join-Path $RepoRoot 'package.json'
$BundleDir  = Join-Path $RepoRoot 'src-tauri\target\release\bundle'

function Write-Step([string]$msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }

# PS5.1 的 Get-Content/Set-Content 默认按 ANSI 处理无 BOM 文件，这里统一用显式 UTF-8 读写
$script:Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
function Read-Utf8([string]$Path) { [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8) }
function Write-Utf8([string]$Path, [string]$Text) { [System.IO.File]::WriteAllText($Path, $Text, $script:Utf8NoBom) }

# git 的正常输出（push 进度、"Everything up-to-date" 等）也写到 stderr；
# PS5.1 + EAP=Stop 管道场景会误报 NativeCommandError。统一封装：
# 函数内局部 EAP=Continue，按退出码判断成败。
# 注意：调用时以 '-' 开头的参数必须加引号（如 '-d'），否则会被 PowerShell
# 当作本函数的参数名吞掉。
function Invoke-Git {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$GitArgs)
    $ErrorActionPreference = 'Continue'
    $out = & git -C $RepoRoot @GitArgs 2>&1
    if ($LASTEXITCODE -ne 0) { throw "git $($GitArgs -join ' ') 失败: $out" }
    $out
}

# ---------- 0. 解析仓库与版本 ----------

Write-Step '解析仓库信息'
$remoteUrl = ((Invoke-Git remote get-url origin) | Select-Object -First 1).Trim()
if ($remoteUrl -match 'github\.com[:/](?<owner>[^/]+)/(?<repo>[^/.]+)(\.git)?$') {
    $Owner = $Matches.owner; $Repo = $Matches.repo
} else {
    throw "无法从 origin 解析 GitHub 仓库: $remoteUrl"
}
Write-Host "    仓库: $Owner/$Repo"

$conf = Read-Utf8 $TauriConf | ConvertFrom-Json
$currentVersion = $conf.version
if (-not $Version) { $Version = $currentVersion }
if ($Version -notmatch '^\d+\.\d+\.\d+(-[\w\.]+)?$') { throw "版本号格式不正确: $Version" }
$Tag = "v$Version"
Write-Host "    版本: $Version (tag: $Tag)"

# ---------- 1. 同步版本号 ----------

if ($Version -ne $currentVersion) {
    Write-Step "同步版本号 $currentVersion -> $Version"

    $conf.version = $Version
    Write-Utf8 $TauriConf (($conf | ConvertTo-Json -Depth 32) + "`n")

    $cargo = Read-Utf8 $CargoToml
    $cargo = $cargo -replace '(?m)^(version\s*=\s*")[^"]+(")', "`${1}$Version`$2"
    Write-Utf8 $CargoToml $cargo

    $pkg = Read-Utf8 $PackageJson | ConvertFrom-Json
    $pkg.version = $Version
    Write-Utf8 $PackageJson (($pkg | ConvertTo-Json -Depth 32) + "`n")

    Invoke-Git add '--' $TauriConf $CargoToml $PackageJson | Out-Null
    Invoke-Git commit '-m' "chore(release): $Tag" | Out-Null
    Invoke-Git push origin HEAD | Out-Null
    Write-Host '    版本号已提交并推送'
}

# ---------- 2. 构建 ----------

if (-not $SkipBuild) {
    Write-Step '构建（bun run tauri build，需要数分钟）'
    Push-Location $RepoRoot
    try {
        & bun run tauri build
        if ($LASTEXITCODE -ne 0) { throw "tauri build 失败 (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}

# ---------- 3. 收集产物 ----------

Write-Step '收集安装包产物'
$artifacts = @()
foreach ($sub in 'nsis', 'msi') {
    $dir = Join-Path $BundleDir $sub
    if (Test-Path $dir) {
        $artifacts += Get-ChildItem $dir -File |
            Where-Object { $_.Extension -in '.exe', '.msi' -and $_.Name -notmatch 'sig$' }
    }
}
if (-not $artifacts) { throw "未找到安装包产物（$BundleDir）。去掉 -SkipBuild 重试？" }
$artifacts | ForEach-Object { Write-Host ("    {0}  ({1:N1} MB)" -f $_.Name, ($_.Length / 1MB)) }

# ---------- 4. git tag ----------

Write-Step "创建并推送 tag $Tag"
$tagExists = Invoke-Git tag '-l' $Tag
if ($tagExists) {
    if (-not $Force) { throw "tag $Tag 已存在。加 -Force 覆盖。" }
    Invoke-Git tag '-d' $Tag | Out-Null
    try { Invoke-Git push origin ":refs/tags/$Tag" | Out-Null } catch { Write-Host '    （远端无此 tag，跳过删除）' }
}
Invoke-Git tag '-a' $Tag '-m' "Release $Tag" | Out-Null
Invoke-Git push origin $Tag | Out-Null
# 确保主分支也是最新
Invoke-Git push origin HEAD | Out-Null

# ---------- 5. GitHub 凭据 ----------

Write-Step '获取 GitHub 凭据（Windows 凭据管理器）'
# 注意：PowerShell 管道/Process.StandardInput 会给 stdin 加 BOM，导致 git 报
# "missing protocol field"，因此经临时文件 + cmd 重定向喂给 git。
$credReq = Join-Path $env:TEMP "git-cred-req-$PID.txt"
try {
    Write-Utf8 $credReq "protocol=https`nhost=github.com`n`n"
    $cred = & cmd /c "git credential fill < `"$credReq`""
} finally {
    Remove-Item $credReq -Force -ErrorAction SilentlyContinue
}
$Token = ($cred | Where-Object { $_ -match '^password=' }) -replace '^password=', ''
if (-not $Token) { throw '未找到 github.com 的凭据，请先 git push 一次以缓存凭据' }
$Headers = @{
    Authorization = "Bearer $Token"
    Accept        = 'application/vnd.github+json'
    'User-Agent'  = 'depotmanager-release-script'
}
$ApiBase = "https://api.github.com/repos/$Owner/$Repo"

# ---------- 6. 创建 / 复用 Release ----------

Write-Step "创建 GitHub Release $Tag"
$release = $null
try {
    $release = Invoke-RestMethod -Headers $Headers -Uri "$ApiBase/releases/tags/$Tag"
} catch {
    # PS5.1 抛 WebException，PS7 抛 HttpResponseException；统一从 Response 取状态码
    $code = $null
    if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
    if ($code -ne 404) { throw }
}
if ($release) {
    if (-not $Force) { throw "Release $Tag 已存在。加 -Force 覆盖。" }
    Write-Host '    已存在同名 Release，-Force 生效：删除后重建'
    Invoke-RestMethod -Headers $Headers -Method Delete -Uri "$ApiBase/releases/$($release.id)" | Out-Null
    $release = $null
}
if (-not $release) {
    if (-not $Notes) {
        $Notes = @"
## DepotManager $Tag

Steam 历史版本下载器（Tauri 2 + Vue 3 + 纯 Rust 自研 Steam 协议栈）。

- 搜索浏览 Steam 游戏，查看分支 / Depot / 历史 manifest
- 指定版本高速并行下载、断点续传、仅校验修复、正则文件过滤
- 登录账号自动领取免费游戏许可；付费游戏需登录拥有该游戏的账号

**下载**：`depotmanager_${Version}_x64-setup.exe`（NSIS 安装包）或 `.msi`。
"@
    }
    $body = @{
        tag_name   = $Tag
        name       = "DepotManager $Tag"
        body       = $Notes
        draft      = $false
        prerelease = ($Version -match '-')
    } | ConvertTo-Json
    $release = Invoke-RestMethod -Headers $Headers -Method Post -Uri "$ApiBase/releases" -Body $body -ContentType 'application/json; charset=utf-8'
}
Write-Host "    Release ID: $($release.id)"

# ---------- 7. 上传产物 ----------

$uploadBase = $release.upload_url -replace '\{\?.*$', ''
foreach ($file in $artifacts) {
    Write-Step "上传 $($file.Name)"
    # 同名资源先删除（-Force 重跑时）；空列表在 PS5.1 下可能返回无属性的空对象，逐一判空
    $assets = @(Invoke-RestMethod -Headers $Headers -Uri "$ApiBase/releases/$($release.id)/assets?per_page=100")
    $existing = @()
    foreach ($a in $assets) {
        if ($null -ne $a -and $a.PSObject.Properties['name'] -and $a.name -eq $file.Name) { $existing += $a }
    }
    foreach ($a in $existing) {
        Invoke-RestMethod -Headers $Headers -Method Delete -Uri "$ApiBase/releases/assets/$($a.id)" | Out-Null
    }
    $ctype = if ($file.Extension -eq '.msi') { 'application/x-msi' } else { 'application/vnd.microsoft.portable-executable' }
    $uri = "${uploadBase}?name=$([uri]::EscapeDataString($file.Name))"
    Invoke-RestMethod -Headers $Headers -Method Post -Uri $uri -InFile $file.FullName -ContentType $ctype | Out-Null
    Write-Host '    完成'
}

Write-Host "`n✅ 发布完成: https://github.com/$Owner/$Repo/releases/tag/$Tag" -ForegroundColor Green
