//! Fork: the Windows reader. A hidden PowerShell child walks the UI Automation tree of the Teams
//! windows once a second and prints who shows as speaking (`logic::parse_line`). A separate
//! process means a hung or crashing UI Automation call can never block or crash Meetily: the
//! reader just stops answering, the circuit breaker opens and the child is killed.
//!
//! NOT VERIFIED AGAINST A LIVE TEAMS MEETING: how Teams exposes the speaking indicator depends on
//! the client version and language, so the match is a best-effort name pattern.

use super::logic::Reader;
use std::time::Duration;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$ae = [System.Windows.Automation.AutomationElement]
$root = $ae::RootElement
$walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
$rx = [regex]'(?i)^(.{1,60}?),?\s*(is speaking|speaking|sta parlando)\b'
[Console]::Out.WriteLine("T")
while ($true) {
  $t0 = Get-Date
  try {
    $procs = @(Get-Process -Name 'ms-teams','Teams' -ErrorAction SilentlyContinue)
    if ($procs.Count -eq 0) { [Console]::Out.WriteLine("X") }
    else {
      $names = New-Object System.Collections.Generic.HashSet[string]
      $deadline = (Get-Date).AddMilliseconds(1500)
      foreach ($p in $procs) {
        $cond = New-Object System.Windows.Automation.PropertyCondition($ae::ProcessIdProperty, $p.Id)
        foreach ($win in $root.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)) {
          $stack = New-Object System.Collections.Stack
          $stack.Push($win); $count = 0
          while ($stack.Count -gt 0 -and $count -lt 2000 -and (Get-Date) -lt $deadline) {
            $el = $stack.Pop(); $count++
            $n = $el.Current.Name
            if ($n) { $m = $rx.Match($n); if ($m.Success) { [void]$names.Add($m.Groups[1].Value.Trim()) } }
            $c = $walker.GetFirstChild($el)
            while ($c -ne $null) { $stack.Push($c); $c = $walker.GetNextSibling($c) }
          }
        }
      }
      [Console]::Out.WriteLine("T`t" + ($names -join "`t"))
    }
  } catch { [Console]::Out.WriteLine("E") }
  Start-Sleep -Milliseconds 1000
}
"#;

    pub struct Powershell {
        child: Child,
        lines: Receiver<String>,
        script: std::path::PathBuf,
        ready: bool,
    }

    impl Powershell {
        pub fn start() -> Result<Self, String> {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let script = std::env::temp_dir().join(format!("meetily-teams-speaker-{}-{}.ps1", std::process::id(), n));
            std::fs::write(&script, SCRIPT).map_err(|e| e.to_string())?;
            let mut child = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("cannot start PowerShell: {}", e))?;
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                return Err("no output pipe".into());
            };
            let (sender, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) if sender.send(line).is_ok() => {}
                        _ => break,
                    }
                }
            });
            Ok(Self { child, lines, script, ready: false })
        }
    }

    impl Reader for Powershell {
        fn read(&mut self, timeout: Duration) -> Result<Option<Vec<String>>, String> {
            loop {
                let line = match self.lines.recv_timeout(timeout) {
                    Ok(line) => line,
                    Err(RecvTimeoutError::Timeout) => return Err("no answer in time".into()),
                    Err(RecvTimeoutError::Disconnected) => return Err("reader exited".into()),
                };
                if !self.ready {
                    // First line only says the script is running (Add-Type succeeded).
                    self.ready = true;
                    if line.trim() == "T" {
                        continue;
                    }
                }
                if line.trim() == "E" {
                    return Err("UI Automation error".into());
                }
                return super::super::logic::parse_line(&line);
            }
        }
    }

    impl Drop for Powershell {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.script);
        }
    }
}

/// Starts the reader, or says why it cannot run.
pub fn open() -> Result<Box<dyn Reader + Send>, String> {
    #[cfg(windows)]
    {
        imp::Powershell::start().map(|reader| Box::new(reader) as Box<dyn Reader + Send>)
    }
    #[cfg(not(windows))]
    {
        let _ = Duration::from_secs(0);
        Err("Teams active speaker is Windows only".into())
    }
}
