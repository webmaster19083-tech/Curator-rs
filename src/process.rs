//! Synchronous bounded subprocess execution, only called on blocking threads.
use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// Construct a process command through one small seam.  Platform-specific
/// process-tree handling lives at call sites; classifiers use this helper so
/// their worker invocation follows the same executable resolution policy.
/// Build asynchronous child processes with the same Unicode and windowing
/// behavior everywhere Curator launches an external helper.  Centralizing it
/// avoids invisible console windows on Windows and prevents a Python worker
/// from changing JSON encoding according to the machine locale.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8");
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Owns a Windows job object for one externally launched process.  Closing
/// the job terminates every inherited descendant, which is more dependable
/// than asking `taskkill` to discover a tree after the parent is already
/// shutting down.
#[cfg(windows)]
pub struct ProcessTreeGuard(windows_sys::Win32::Foundation::HANDLE);

// A Windows HANDLE is process-wide and CloseHandle is safe from any thread;
// the guard only owns that handle and never exposes the raw pointer.
#[cfg(windows)]
unsafe impl Send for ProcessTreeGuard {}

#[cfg(windows)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Put a freshly spawned process in a kill-on-close job.  This is best-effort
/// because some managed environments deliberately disallow nested jobs; the
/// caller retains its direct-child and taskkill fallbacks in that case.
#[cfg(windows)]
pub fn guard_process_tree(pid: u32) -> Option<ProcessTreeGuard> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::null;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    unsafe {
        let job: HANDLE = CreateJobObjectW(null(), null());
        if job.is_null() {
            return None;
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured == 0 {
            let _ = CloseHandle(job);
            return None;
        }
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            let _ = CloseHandle(job);
            return None;
        }
        let assigned = AssignProcessToJobObject(job, process);
        let _ = CloseHandle(process);
        if assigned == 0 {
            let _ = CloseHandle(job);
            return None;
        }
        Some(ProcessTreeGuard(job))
    }
}

pub fn output_timeout(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    // Files avoid pipe deadlock and unbounded reader threads on broken subprocesses.
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = cmd
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()?;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(result.err().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "subprocess timeout")
                }));
            }
        }
    };
    use std::io::{Seek, SeekFrom};
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.take(65536).read_to_end(&mut out)?;
    stderr.take(65536).read_to_end(&mut err)?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hung_subprocess_is_killed_and_reaped() {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("powershell.exe");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        let start = Instant::now();
        assert_eq!(
            output_timeout(&mut cmd, Duration::from_millis(200))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
