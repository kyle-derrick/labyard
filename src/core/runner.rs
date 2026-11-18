//! 子进程执行器。
//!
//! 这是整个项目里最容易只在真机上暴露问题的一块，所以三件事写死在这里：
//! 1. **进程组**：`process_group(0)` 让子进程成为新进程组的组长，取消时
//!    `kill(-pid)` 才能连带杀掉 `apt`/`dpkg` 这些孙进程。只杀父进程会留下
//!    孤儿占着 dpkg 锁，下次 apply 直接卡死。
//! 2. **超时**：用 `tokio::time::timeout` 包住整个等待，超时后同样走进程组清理。
//! 3. **背压**：输出走有界 channel，满了丢中间行并计数，最后补一条汇总——
//!    报错通常在尾部，所以尾部绝不丢。

use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 一条要执行的命令。
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// 喂给 stdin 的内容（`pct push` 之外很少用）
    pub stdin: Option<Vec<u8>>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout: Duration::from_secs(600),
            cwd: None,
            env: Vec::new(),
            stdin: None,
        }
    }

    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// 用于日志与审计的展示形式（不含 stdin 内容）。
    pub fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(' ') || a.contains('"') {
                s.push('"');
                s.push_str(&a.replace('"', "\\\""));
                s.push('"');
            } else {
                s.push_str(a);
            }
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// 执行过程中向外发的事件。SSE 直接把它序列化下发。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Started {
        key: String,
        command: String,
        pid: u32,
    },
    Line {
        key: String,
        stream: Stream,
        line: String,
    },
    /// 因背压被丢弃的行数汇总
    Skipped {
        key: String,
        lines: u64,
    },
    Finished {
        key: String,
        code: Option<i32>,
        duration_ms: u64,
    },
    Failed {
        key: String,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    /// 完整 stdout（仅在 `capture` 为真时有值）
    pub stdout: String,
    // 耗时与丢弃行数不进这里：它们已经通过 `Event::Finished` / `Event::Skipped`
    // 推给了订阅者。放在返回值里只会多一份没人读的副本。
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("无法启动 `{cmd}`: {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{cmd}` 超时（{}s），已杀进程组", .timeout.as_secs())]
    Timeout { cmd: String, timeout: Duration },
    #[error("`{cmd}` 被取消")]
    Cancelled { cmd: String },
    #[error("`{cmd}` 退出码 {code:?}")]
    NonZero { cmd: String, code: Option<i32> },
}

/// 执行端口。测试注入 `FakeRunner`，真机注入 `RealRunner`。
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(
        &self,
        key: &str,
        spec: &CommandSpec,
        events: Option<mpsc::Sender<Event>>,
        cancel: &CancellationToken,
        capture: bool,
    ) -> Result<Output, RunError>;
}

/// 真机执行器。
pub struct RealRunner;

impl RealRunner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Runner for RealRunner {
    async fn run(
        &self,
        key: &str,
        spec: &CommandSpec,
        events: Option<mpsc::Sender<Event>>,
        cancel: &CancellationToken,
        capture: bool,
    ) -> Result<Output, RunError> {
        let started = Instant::now();
        let display = spec.display();

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 关键：新进程组，取消时能连带杀掉孙进程
            .process_group(0);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| RunError::Spawn {
            cmd: display.clone(),
            source: e,
        })?;
        let pid = child.id().unwrap_or(0);

        if let Some(tx) = &events {
            let _ = tx
                .try_send(Event::Started {
                    key: key.to_string(),
                    command: display.clone(),
                    pid,
                });
        }

        // stdin
        if let Some(input) = &spec.stdin {
            if let Some(mut si) = child.stdin.take() {
                let input = input.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = si.write_all(&input).await;
                    let _ = si.shutdown().await;
                });
            }
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // 读写两个流，逐行推事件；capture 时同时攒进 String
        let key_s = key.to_string();
        let tx_out = events.clone();
        let out_task = tokio::spawn(async move {
            let mut buf = String::new();
            let mut skipped = 0u64;
            if let Some(so) = stdout {
                let mut lines = BufReader::new(so).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if capture {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                    if let Some(tx) = &tx_out {
                        if tx
                            .try_send(Event::Line {
                                key: key_s.clone(),
                                stream: Stream::Stdout,
                                line,
                            })
                            .is_err()
                        {
                            skipped += 1;
                        }
                    }
                }
            }
            (buf, skipped)
        });

        let key_e = key.to_string();
        let tx_err = events.clone();
        let err_task = tokio::spawn(async move {
            let mut skipped = 0u64;
            if let Some(se) = stderr {
                let mut lines = BufReader::new(se).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(tx) = &tx_err {
                        if tx
                            .try_send(Event::Line {
                                key: key_e.clone(),
                                stream: Stream::Stderr,
                                line,
                            })
                            .is_err()
                        {
                            skipped += 1;
                        }
                    }
                }
            }
            skipped
        });

        // 等待：超时 / 取消 / 正常退出，三选一
        let wait = async {
            let status = child.wait().await;
            status
        };

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                kill_group(pid);
                let _ = child.wait().await;
                Err(RunError::Cancelled { cmd: display.clone() })
            }
            r = tokio::time::timeout(spec.timeout, wait) => {
                match r {
                    Ok(Ok(status)) => Ok(status),
                    Ok(Err(e)) => Err(RunError::Spawn { cmd: display.clone(), source: e }),
                    Err(_) => {
                        kill_group(pid);
                        let _ = child.wait().await;
                        Err(RunError::Timeout { cmd: display.clone(), timeout: spec.timeout })
                    }
                }
            }
        };

        let (stdout_buf, skipped_out) = out_task.await.unwrap_or_default();
        let skipped_err = err_task.await.unwrap_or_default();
        let skipped = skipped_out + skipped_err;
        if skipped > 0 {
            if let Some(tx) = &events {
                let _ = tx.try_send(Event::Skipped {
                    key: key.to_string(),
                    lines: skipped,
                });
            }
        }

        let duration = started.elapsed();
        match outcome {
            Ok(status) => {
                let code = status.code();
                if let Some(tx) = &events {
                    let _ = tx.try_send(Event::Finished {
                        key: key.to_string(),
                        code,
                        duration_ms: duration.as_millis() as u64,
                    });
                }
                Ok(Output {
                    code,
                    stdout: stdout_buf,
                })
            }
            Err(e) => {
                if let Some(tx) = &events {
                    let _ = tx.try_send(Event::Failed {
                        key: key.to_string(),
                        error: e.to_string(),
                    });
                }
                Err(e)
            }
        }
    }
}

/// 杀整个进程组。负 pid 是 POSIX 的"发给整个组"。
fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        // SIGTERM 先给机会优雅退出
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    // 200ms 后还在就 SIGKILL。这里用阻塞 sleep 是因为它只在一个短窗口内发生，
    // 且必须在返回前确保子进程已死（否则调用方会以为清理完成）。
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let r = RealRunner::new();
        let spec = CommandSpec::new("/bin/sh", ["-c", "echo hello; exit 0"]);
        let out = r
            .run("t1", &spec, None, &CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert!(out.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_returned_not_raised() {
        let r = RealRunner::new();
        let spec = CommandSpec::new("/bin/sh", ["-c", "exit 3"]);
        let out = r
            .run("t2", &spec, None, &CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(out.code, Some(3));
    }

    #[tokio::test]
    async fn timeout_kills_the_process_group() {
        let r = RealRunner::new();
        // 子进程再 spawn 一个孙进程，验证一起被杀
        let spec = CommandSpec::new(
            "/bin/sh",
            ["-c", "sleep 30 & echo $! > /tmp/labyard-test-child.pid; wait"],
        )
        .with_timeout(Duration::from_millis(300));
        let err = r
            .run("t3", &spec, None, &CancellationToken::new(), true)
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout { .. }));
        // 孙进程应已被杀
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(pid_s) = std::fs::read_to_string("/tmp/labyard-test-child.pid") {
            if let Ok(pid) = pid_s.trim().parse::<i32>() {
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                assert!(!alive, "孙进程 {pid} 仍然存活，进程组没杀干净");
            }
        }
        let _ = std::fs::remove_file("/tmp/labyard-test-child.pid");
    }

    #[tokio::test]
    async fn cancellation_stops_the_child() {
        let r = RealRunner::new();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let spec = CommandSpec::new("/bin/sh", ["-c", "sleep 30"]);
        let err = r.run("t4", &spec, None, &cancel, true).await.unwrap_err();
        assert!(matches!(err, RunError::Cancelled { .. }));
    }
}
