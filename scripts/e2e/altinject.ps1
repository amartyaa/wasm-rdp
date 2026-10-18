# Inject a genuine OS-level Alt+Tab and report whether the foreground window
# changed. keybd_event feeds the same input queue as real hardware, so the
# window manager's shortcut handler sees it -- unlike CDP synthetic keys.
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Native {
    [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder t, int n);
    public static string Foreground() {
        var sb = new StringBuilder(300);
        GetWindowText(GetForegroundWindow(), sb, 300);
        return sb.ToString();
    }
}
"@
$VK_MENU = 0x12; $VK_TAB = 0x09; $KEYUP = 0x2

$before = [Native]::Foreground()
[Native]::keybd_event($VK_MENU, 0x38, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 120
[Native]::keybd_event($VK_TAB, 0x0F, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 60
[Native]::keybd_event($VK_TAB, 0x0F, $KEYUP, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 400
[Native]::keybd_event($VK_MENU, 0x38, $KEYUP, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 700
$after = [Native]::Foreground()

Write-Output "FG_BEFORE=$before"
Write-Output "FG_AFTER=$after"
Write-Output ("SWITCHED=" + $(if ($before -eq $after) { "no" } else { "YES" }))
