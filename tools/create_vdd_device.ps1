<#
    create_vdd_device.ps1 — 为 IddCx 间接显示驱动创建 root-enumerated 设备节点。

    背景：
    MttVDD.inf 的硬件 ID 是 "Root\MttVDD"，属于软件枚举设备（SWD / root-enumerated）。
    `pnputil /add-driver /install` 只会把驱动包发布到 DriverStore，并对**已存在**的
    匹配设备安装驱动 —— 它不会凭空创建 root-enumerated 设备节点。
    本脚本直接调用 SetupAPI 完成「创建 → 注册 → 绑定驱动」三步，等价于 WDK 的
    `devcon install MttVDD.inf Root\MttVDD`，但不需要安装 WDK。

    用法（必须在管理员 PowerShell 中运行）：
        powershell -NoProfile -ExecutionPolicy Bypass -File create_vdd_device.ps1

    可选参数：
        -InfPath  <inf 路径>     默认取本仓库 tools\vdd-driver\VirtualDisplayDriver\MttVDD.inf
        -HardwareId <hw id>      默认 Root\MttVDD
#>

[CmdletBinding()]
param(
    [string]$InfPath = "",
    [string]$HardwareId = "Root\MttVDD"
)

$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------- 管理员检查
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host "[FAIL] 需要管理员权限。请右键『以管理员身份运行』PowerShell 后重试。" -ForegroundColor Red
    exit 5
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
if ([string]::IsNullOrWhiteSpace($InfPath)) {
    $InfPath = Join-Path $repoRoot "tools\vdd-driver\VirtualDisplayDriver\MttVDD.inf"
}
if (-not (Test-Path $InfPath)) {
    Write-Host "[FAIL] INF 不存在: $InfPath" -ForegroundColor Red
    exit 2
}
Write-Host "[INFO] INF : $InfPath"
Write-Host "[INFO] HWID: $HardwareId"

# ---------------------------------------------------------------- SetupAPI 绑定
$setupApiCode = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class SetupApi
{
    [StructLayout(LayoutKind.Sequential)]
    public struct SP_DEVINFO_DATA
    {
        public int cbSize;
        public Guid ClassGuid;
        public uint DevInst;
        public IntPtr Reserved;
    }

    public const int DICD_GENERATE_ID      = 0x00000001;
    public const int SPDRP_HARDWAREID      = 1;
    public const int SPDRP_FRIENDLYNAME    = 12;
    public const uint DICS_FLAG_GLOBAL     = 0x00000001;
    public const int DIF_REGISTERDEVICE    = 0x00000019;
    public const uint INSTALLFLAG_FORCE    = 0x00000001;
    public const uint SPAPI_E_NO_SUCH_DEVINST = 0x80070104;

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern IntPtr SetupDiCreateDeviceInfoList(IntPtr ClassGuid, IntPtr hwndParent);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool SetupDiCreateDeviceInfo(
        IntPtr DeviceInfoSet, string DeviceName, ref Guid ClassGuid, string DeviceDescription,
        IntPtr PhysicalDeviceObject, int CreationFlags, IntPtr DeviceInfoData,
        out IntPtr DeviceInfoData);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool SetupDiSetDeviceRegistryProperty(
        IntPtr DeviceInfoData, int Property, byte[] PropertyBuffer, int PropertyBufferSize, uint Flags);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool SetupDiCallClassInstaller(int InstallFunction, IntPtr DeviceInfoSet, IntPtr DeviceInfoData);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool SetupDiGetDeviceInstanceId(
        IntPtr DeviceInfoSet, IntPtr DeviceInfoData, StringBuilder DeviceInstanceId,
        int DeviceInstanceIdSize, out int RequiredSize);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool SetupDiGetDeviceRegistryProperty(
        IntPtr DeviceInfoData, int Property, out uint PropertyRegDataType,
        byte[] PropertyBuffer, int PropertyBufferSize, out int RequiredSize);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool SetupDiDestroyDeviceInfoList(IntPtr DeviceInfoSet);

    [DllImport("setupapi.dll", CharSet = CharSet.Ansi, SetLastError = true)]
    public static extern bool UpdateDriverForPlugAndPlayDevices(
        IntPtr hwndParent, string DeviceId, string DriverInfPath, uint InstallFlags,
        out IntPtr rebootRequired);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint GetLastError();

    public static string Describe(int err)
    {
        return new Win32Exception((int)err).Message;
    }
}
'@

if (-not ("SetupApi" -as [type])) {
    Add-Type -TypeDefinition $setupApiCode -Language CSharp
}

# 显示适配器类 GUID
$displayClass = New-Object Guid "4d36e968-e325-11ce-bfc1-08002be10318"

# ---------------------------------------------------------------- 工具函数
function Get-DeviceNode([string]$InstanceId) {
    $out = pnputil /enum-devices /instanceid "$InstanceId" 2>&1 | Out-String
    if ($out -match "找不到任何设备|No devices were found") { return $null }
    return $out
}

function Get-DevInfoData([IntPtr]$Set, [IntPtr]$Info) {
    $d = New-Object SetupApi+SP_DEVINFO_DATA
    $d.cbSize = [Runtime.InteropServices.Marshal]::SizeOf([type][SetupApi+SP_DEVINFO_DATA])
    $ptr = [Runtime.InteropServices.Marshal]::AllocHGlobal($d.cbSize)
    try {
        [Runtime.InteropServices.Marshal]::StructureToPtr($d, $ptr, $false)
        $buf = New-Object byte[] $d.cbSize
        [Runtime.InteropServices.Marshal]::Copy($ptr, $buf, 0, $d.cbSize)
        return $buf
    } finally {
        [Runtime.InteropServices.Marshal]::FreeHGlobal($ptr)
    }
}

# 在已存在的设备信息集里按实例 ID 找到 devInfo
function Find-DevInfo([IntPtr]$Set, [string]$InstanceId) {
    $found = [IntPtr]::Zero
    $devInfo = New-Object SetupApi+SP_DEVINFO_DATA
    $devInfo.cbSize = [Runtime.InteropServices.Marshal]::SizeOf([type][SetupApi+SP_DEVINFO_DATA])
    for ($i = 0; $true; $i++) {
        if (-not [SetupApi]::SetupDiGetDeviceInstanceId($Set, $devInfo, $null, 0, [ref]$i)) { }
        $need = $i
        if ($need -le 0) { break }
        $sb = New-Object System.Text.StringBuilder $need
        if ([SetupApi]::SetupDiGetDeviceInstanceId($Set, $devInfo, $sb, $need, [ref]$i)) {
            if ($sb.ToString() -ieq $InstanceId) { $found = $devInfo; break }
        } else {
            break
        }
    }
    return $found
}

# ---------------------------------------------------------------- 主流程
Write-Host "=== 步骤 1/4：确认驱动包已发布到 DriverStore ==="
$drivers = pnputil /enum-drivers 2>&1 | Out-String
if ($drivers -match "mttvdd\.inf") {
    Write-Host "  [OK] 驱动包已安装（oem*.inf / mttvdd.inf）" -ForegroundColor Green
} else {
    Write-Host "  [..] 驱动包未安装，先执行 pnputil /add-driver ... /install" -ForegroundColor Yellow
    pnputil /add-driver "$InfPath" /install | Out-Null
}

Write-Host "=== 步骤 2/4：检查设备节点是否存在 ==="
$node = Get-DeviceNode $HardwareId
$created = $false

if ($node) {
    Write-Host "  [OK] 节点已存在：$HardwareId" -ForegroundColor Green
} else {
    Write-Host "  [..] 节点不存在，开始用 SetupAPI 创建（root-enumerated SWD）" -ForegroundColor Yellow

    $set = [SetupApi]::SetupDiCreateDeviceInfoList([IntPtr]::Zero, [IntPtr]::Zero)
    if ($set -eq [IntPtr]::Zero) {
        $e = [SetupApi]::GetLastError()
        Write-Host "  [FAIL] SetupDiCreateDeviceInfoList 失败: $([SetupApi]::Describe($e))" -ForegroundColor Red
        exit 3
    }

    try {
        # 2.1 创建设备信息（设备名对 SWD 用 devcon 惯例名）
        $devInfo = [IntPtr]::Zero
        $ok = [SetupApi]::SetupDiCreateDeviceInfo(
            $set, "Root\MttVDD", [ref]$displayClass, "Virtual Display Driver",
            [IntPtr]::Zero, [SetupApi]::DICD_GENERATE_ID, [IntPtr]::Zero, [ref]$devInfo)
        if (-not $ok) {
            $e = [SetupApi]::GetLastError()
            Write-Host "  [FAIL] SetupDiCreateDeviceInfo 失败: $([SetupApi]::Describe($e))" -ForegroundColor Red
            exit 3
        }

        # 2.2 写入 HardwareID（REG_MULTI_SZ；同时写两个 hw id 兼容 INF 的两条模型行）
        $hwIds = "$HardwareId`0MttVDD`0`0"
        $bytes = [Text.Encoding]::Unicode.GetBytes($hwIds)
        $ok = [SetupApi]::SetupDiSetDeviceRegistryProperty(
            $devInfo, [SetupApi]::SPDRP_HARDWAREID, $bytes, $bytes.Length, [SetupApi]::DICS_FLAG_GLOBAL)
        if (-not $ok) {
            $e = [SetupApi]::GetLastError()
            Write-Host "  [FAIL] 写 HardwareID 失败: $([SetupApi]::Describe($e))" -ForegroundColor Red
            exit 3
        }

        # 2.3 写友好名称
        $fname = [Text.Encoding]::Unicode.GetBytes("Virtual Display Driver`0")
        [void][SetupApi]::SetupDiSetDeviceRegistryProperty(
            $devInfo, [SetupApi]::SPDRP_FRIENDLYNAME, $fname, $fname.Length, [SetupApi]::DICS_FLAG_GLOBAL)

        # 2.4 注册设备（DIF_REGISTERDEVICE）
        $ok = [SetupApi]::SetupDiCallClassInstaller([SetupApi]::DIF_REGISTERDEVICE, $set, $devInfo)
        if (-not $ok) {
            $e = [SetupApi]::GetLastError()
            Write-Host "  [FAIL] DIF_REGISTERDEVICE 失败: $([SetupApi]::Describe($e))" -ForegroundColor Red
            exit 3
        }
        Write-Host "  [OK] 设备已注册" -ForegroundColor Green
        $created = $true
    } finally {
        [void][SetupApi]::SetupDiDestroyDeviceInfoList($set)
    }
}

Write-Host "=== 步骤 3/4：绑定驱动（UpdateDriverForPlugAndPlayDevices）==="
$reboot = [IntPtr]::Zero
$ok = [SetupApi]::UpdateDriverForPlugAndPlayDevices(
    [IntPtr]::Zero, $HardwareId, $InfPath, [SetupApi]::INSTALLFLAG_FORCE, [ref]$reboot)
if ($ok) {
    Write-Host "  [OK] 驱动已绑定到 $HardwareId" -ForegroundColor Green
} else {
    $e = [SetupApi]::GetLastError()
    Write-Host "  [WARN] UpdateDriverForPlugAndPlayDevices 失败: $([SetupApi]::Describe($e))" -ForegroundColor Yellow
    Write-Host "         改用 pnputil 兜底..." -ForegroundColor Yellow
    pnputil /add-driver "$InfPath" /install | Out-Null
    pnputil /restart-device "$HardwareId" 2>&1 | Out-String | Write-Host
}
if ($reboot -ne [IntPtr]::Zero) {
    Write-Host "  [WARN] 官方提示需要重启才能完成安装" -ForegroundColor Yellow
}

Write-Host "=== 步骤 4/4：自检 ==="
Start-Sleep -Seconds 2

$node = Get-DeviceNode $HardwareId
if ($node) {
    Write-Host "  [OK] ROOT\MttVDD 节点已存在" -ForegroundColor Green
    ($node -split "`r?`n" | Where-Object { $_ -match "状态|Status|设备描述|Instance ID|驱动程序" } | ForEach-Object { "      $_" }) | Write-Host
} else {
    Write-Host "  [FAIL] 节点仍未出现" -ForegroundColor Red
}

$umdfDll = Join-Path $env:SystemRoot "System32\drivers\UMDF\MttVDD.dll"
if (Test-Path $umdfDll) {
    $len = (Get-Item $umdfDll).Length
    Write-Host "  [OK] UMDF 驱动已落地: $umdfDll ($len 字节)" -ForegroundColor Green
} else {
    Write-Host "  [WARN] UMDF 驱动未复制到 System32\drivers\UMDF（可能需要重启）" -ForegroundColor Yellow
}

$wudf = Get-Service Wudfsvc -ErrorAction SilentlyContinue
if ($wudf) {
    Write-Host "  [INFO] UMDF 宿主服务 Wudfsvc: $($wudf.Status)" -ForegroundColor Yellow
    if ($wudf.Status -ne "Running") {
        try { Start-Service Wudfsvc -ErrorAction Stop; Write-Host "  [OK] 已启动 Wudfsvc" -ForegroundColor Green } catch { Write-Host "  [WARN] 启动 Wudfsvc 失败: $($_.Exception.Message)" -ForegroundColor Yellow }
    }
}

Write-Host ""
Write-Host "=== Display 类设备清单 ===" -ForegroundColor Cyan
$disp = pnputil /enum-devices /class Display 2>&1 | Out-String
($disp -split "`r?`n" | Where-Object { $_ -match "设备描述|Instance ID" } | ForEach-Object { "  $_" }) | Write-Host

Write-Host ""
if ($created) {
    Write-Host "完成：设备节点已创建。请执行下面的命令确认虚拟屏已出现：" -ForegroundColor Green
} else {
    Write-Host "完成：驱动已重新绑定。请执行下面的命令确认虚拟屏已出现：" -ForegroundColor Green
}
Write-Host "  rs-host displays"
Write-Host "如果显示列表里出现新的『Virtual Display Driver』且分辨率不是 0x0，即为成功。"
Write-Host "若仍无该设备，请重启一次 Windows 后再跑一次本脚本。"
