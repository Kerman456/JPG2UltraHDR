# 构建 Android APK（Windows + PowerShell）
#
# 用法：
#   .\build-android.ps1              # 构建 debug APK
#   .\build-android.ps1 -Release     # 构建 release APK 并签名
#
# 依赖：JDK 21、Android SDK（platform-tools / platforms;android-35 /
#       build-tools;35.0.0）、NDK 27.3.13750724、cargo-ndk、rustup target
#       aarch64-linux-android、Gradle 8.5。
#
# SDK / NDK / JDK 的位置会按以下顺序解析，都不需要改脚本：
#   1. 命令行参数（-AndroidHome / -NdkVersion / -JdkHome / -GradlePath）
#   2. 环境变量（ANDROID_HOME / ANDROID_SDK_ROOT / ANDROID_NDK_HOME / JAVA_HOME）
#   3. 本机默认安装位置（D:\Android、C:\Program Files\Microsoft\jdk-21*）
#   4. Gradle 优先用 android\gradlew，其次 PATH 里的 gradle

param(
    [switch]$Release,
    [string]$GradlePath = "",
    [string]$NdkVersion = "",
    [string]$AndroidHome = "",
    [string]$JdkHome = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

# ─── 解析 Android SDK ───────────────────────────────────────────────
if (-not $AndroidHome) {
    $AndroidHome = if ($env:ANDROID_HOME) { $env:ANDROID_HOME }
                   elseif ($env:ANDROID_SDK_ROOT) { $env:ANDROID_SDK_ROOT }
                   elseif (Test-Path "D:\Android") { "D:\Android" }
                   elseif (Test-Path "$env:LOCALAPPDATA\Android\Sdk") { "$env:LOCALAPPDATA\Android\Sdk" }
                   else { "" }
}
if (-not $AndroidHome -or -not (Test-Path $AndroidHome)) {
    throw "找不到 Android SDK。请设置 ANDROID_HOME，或用 -AndroidHome 指定。"
}

# ─── 解析 NDK：不指定版本时取 sdk\ndk 下最新的一个 ─────────────────
if (-not $NdkVersion) {
    $ndkDir = Join-Path $AndroidHome "ndk"
    if (Test-Path $ndkDir) {
        $NdkVersion = (Get-ChildItem $ndkDir -Directory |
            Sort-Object Name -Descending | Select-Object -First 1).Name
    }
}

# ─── 解析 JDK ───────────────────────────────────────────────────────
if (-not $JdkHome) {
    $JdkHome = if ($env:JAVA_HOME) { $env:JAVA_HOME }
               else {
                   $c = Get-ChildItem "C:\Program Files\Microsoft" -Directory -ErrorAction SilentlyContinue |
                        Where-Object Name -like "jdk-21*" | Select-Object -First 1
                   if ($c) { $c.FullName }
                   elseif (Test-Path "C:\Program Files\Java") {
                       $j = Get-ChildItem "C:\Program Files\Java" -Directory -ErrorAction SilentlyContinue |
                            Where-Object Name -like "jdk-21*" | Select-Object -First 1
                       if ($j) { $j.FullName }
                   }
               }
}
if ($JdkHome -and (Test-Path $JdkHome)) { $env:JAVA_HOME = $JdkHome }
elseif (-not $env:JAVA_HOME) { throw "找不到 JDK 21。请设置 JAVA_HOME，或用 -JdkHome 指定。" }

$env:PATH = "$env:JAVA_HOME\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"
$env:ANDROID_HOME = $AndroidHome
$env:ANDROID_SDK_ROOT = $AndroidHome
if ($NdkVersion) { $env:ANDROID_NDK_HOME = Join-Path (Join-Path $AndroidHome "ndk") $NdkVersion }
$env:APP_LIB_NAME = "jpg2ultrahdr_android"
$env:APP_PACKAGE = "com.kerman456.jpg2ultrahdr"

Write-Host "==> SDK: $AndroidHome" -ForegroundColor DarkGray
if ($NdkVersion) { Write-Host "==> NDK: $NdkVersion" -ForegroundColor DarkGray }
Write-Host "==> JDK: $env:JAVA_HOME" -ForegroundColor DarkGray


$profile = if ($Release) { "release" } else { "dev" }
$task = if ($Release) { "assembleRelease" } else { "assembleDebug" }

Write-Host "==> 编译 Rust（arm64-v8a, $profile）" -ForegroundColor Cyan
Remove-Item -Recurse -Force "android\app\src\main\jniLibs" -ErrorAction SilentlyContinue
$ndkArgs = @("-t", "arm64-v8a", "-o", "android\app\src\main\jniLibs", "build")
if ($Release) { $ndkArgs += "--release" }
& cargo ndk @ndkArgs
if ($LASTEXITCODE -ne 0) { throw "cargo ndk 失败" }

Write-Host "==> Gradle $task" -ForegroundColor Cyan
Set-Location "android"

# 优先用仓库自带的 wrapper，其次才用 -GradlePath / PATH 里的 gradle
if (-not $GradlePath) {
    if (Test-Path "gradlew.bat") { $GradlePath = ".\gradlew.bat" }
    else {
        $g = Get-Command gradle -ErrorAction SilentlyContinue
        if ($g) { $GradlePath = $g.Source }
        elseif (Test-Path "D:\gradle-8.5\bin\gradle.bat") { $GradlePath = "D:\gradle-8.5\bin\gradle.bat" }
        else { throw "找不到 Gradle。请用 -GradlePath 指定 gradle.bat / gradlew.bat。" }
    }
}
Write-Host "==> Gradle: $GradlePath" -ForegroundColor DarkGray
& $GradlePath $task --no-daemon
$gradleOk = ($LASTEXITCODE -eq 0)

# wrapper 首次运行需要联网下载 Gradle 发行包；本机若被 TLS 拦截会失败，
# 这时自动退回本机已解压的 Gradle，不打断整个构建。
if (-not $gradleOk -and $GradlePath -like "*gradlew*") {
    $fallback = $null
    $g = Get-Command gradle -ErrorAction SilentlyContinue
    if ($g) { $fallback = $g.Source }
    elseif (Test-Path "D:\gradle-8.5\bin\gradle.bat") { $fallback = "D:\gradle-8.5\bin\gradle.bat" }
    if ($fallback) {
        Write-Host "==> wrapper 失败（多半是拉不到发行包），改用本机 Gradle: $fallback" -ForegroundColor Yellow
        & $fallback $task --no-daemon
        $gradleOk = ($LASTEXITCODE -eq 0)
    }
}
if (-not $gradleOk) { throw "gradle 失败" }
Set-Location $root

if ($Release) {
    # build-tools 版本跟着 SDK 里实际装的走
    $btRoot = Join-Path $AndroidHome "build-tools"
    $buildTools = if (Test-Path $btRoot) {
        (Get-ChildItem $btRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1).FullName
    } else { $null }
    if (-not $buildTools) { throw "找不到 build-tools。请检查 Android SDK 安装。" }

    $unsigned = "android\app\build\outputs\apk\release\app-release-unsigned.apk"
    # 版本号只在 app\build.gradle 里写一次
    $ver = (Select-String -Path "android\app\build.gradle" -Pattern 'versionName\s+"([^"]+)"' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    $apk = "android\app\build\outputs\apk\release\JPG2UltraHDR-$ver-arm64.apk"
    $keystore = "signing\release.jks"
    # 口令可用环境变量覆盖；默认值是为了兼容本机已有的密钥
    $ksPass = if ($env:JPG2ULTRAHDR_KS_PASS) { $env:JPG2ULTRAHDR_KS_PASS } else { "jpg2ultrahdr" }

    if (-not (Test-Path $keystore)) {
        Write-Host "==> 生成签名密钥 $keystore" -ForegroundColor Cyan
        New-Item -ItemType Directory -Force -Path (Split-Path $keystore) | Out-Null
        & keytool -genkeypair -keystore $keystore -storetype JKS -alias jpg2ultrahdr `
            -keyalg RSA -keysize 2048 -validity 10000 `
            -storepass $ksPass -keypass $ksPass `
            -dname "CN=Kerman456, OU=Dev, O=JPG2UltraHDR, L=Shanghai, ST=Shanghai, C=CN"
    }

    Write-Host "==> 对齐并签名（build-tools $(Split-Path $buildTools -Leaf)）" -ForegroundColor Cyan
    Remove-Item $apk -ErrorAction SilentlyContinue
    & "$buildTools\zipalign.exe" -f -p 4 $unsigned $apk
    & "$buildTools\apksigner.bat" sign --ks $keystore --ks-pass "pass:$ksPass" `
        --key-pass "pass:$ksPass" --ks-key-alias jpg2ultrahdr $apk
    & "$buildTools\apksigner.bat" verify $apk
    $final = $apk
} else {
    $final = "android\app\build\outputs\apk\debug\app-debug.apk"
}

$file = Get-Item $final
Write-Host ("==> 完成：{0}  {1:N0} 字节" -f $file.FullName, $file.Length) -ForegroundColor Green
