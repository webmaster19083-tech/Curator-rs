//! Synchronous bounded subprocess execution, only called on blocking threads.
use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

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
