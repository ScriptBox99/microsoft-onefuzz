// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::Result;
use tokio::fs;
use tokio::process::Command;
use tokio::task::spawn_blocking;

pub async fn collect_exception_info(
    args: &[impl AsRef<OsStr>],
    env: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
) -> Result<Option<DotnetExceptionInfo>> {
    let tmp_dir = spawn_blocking(tempfile::tempdir).await??;

    let dump_path = tmp_dir.path().join(DUMP_FILE_NAME);

    let dump = match collect_dump(args, env, &dump_path).await? {
        Some(dump) => dump,
        None => {
            return Ok(None);
        }
    };

    let exception = dump.exception().await?;

    // Remove temp dir without blocking.
    spawn_blocking(move || tmp_dir).await?;

    Ok(exception)
}

const DUMP_FILE_NAME: &str = "tmp.dmp";

// Assumes `dotnet` >= 6.0.
//
// See: https://docs.microsoft.com/en-us/dotnet/core/diagnostics/dumps
const ENABLE_MINIDUMP_VAR: &str = "DOTNET_DbgEnableMiniDump";
const MINIDUMP_TYPE_VAR: &str = "DOTNET_DbgMiniDumpType";
const MINIDUMP_NAME_VAR: &str = "DOTNET_DbgMiniDumpName";

const MINIDUMP_ENABLE: &str = "1";
const MINIDUMP_TYPE_NORMAL: &str = "1";

// Invoke target with .NET runtime environment vars set to create minidumps.
//
// Returns the newly-created dump file, when present.
async fn collect_dump(
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    env: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
    dump_path: impl AsRef<Path>,
) -> Result<Option<DotnetDumpFile>> {
    let dump_path = dump_path.as_ref();

    let mut cmd = Command::new("dotnet");
    cmd.arg("exec");
    cmd.args(args);

    cmd.envs(env);

    cmd.env(ENABLE_MINIDUMP_VAR, MINIDUMP_ENABLE);
    cmd.env(MINIDUMP_TYPE_VAR, MINIDUMP_TYPE_NORMAL);
    cmd.env(MINIDUMP_NAME_VAR, dump_path);

    let mut child = cmd.spawn()?;
    let exit_status = child.wait().await?;

    if exit_status.success() {
        warn!("dotnet target exited normally when attempting to collect minidump");
    }

    let metadata = fs::metadata(dump_path).await;

    if metadata.is_ok() {
        // File exists and is readable if metadata is.
        let dump = DotnetDumpFile::new(dump_path.to_owned());

        Ok(Some(dump))
    } else {
        Ok(None)
    }
}

pub struct DotnetDumpFile {
    path: PathBuf,
}

impl DotnetDumpFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn exception(&self) -> Result<Option<DotnetExceptionInfo>> {
        let output = self.exec_sos_command(SOS_PRINT_EXCEPTION).await?;
        let text = String::from_utf8_lossy(&output.stdout);
        let exception = parse_sos_print_exception_output(&text).ok();

        Ok(exception)
    }

    async fn exec_sos_command(&self, sos_cmd: &str) -> Result<Output> {
        let mut cmd = Command::new("dotnet");

        // Run `dotnet-analyze` with a single SOS command on startup, then exit
        // the otherwise-interactive SOS session.
        let dump_path = self.path.display().to_string();
        cmd.args(["dump", "analyze", &dump_path, "-c", sos_cmd, "-c", SOS_EXIT]);

        let output = cmd.spawn()?.wait_with_output().await?;

        if !output.status.success() {
            bail!(
                "SOS session for command `{}` exited with error {}: {}",
                sos_cmd,
                output.status,
                String::from_utf8_lossy(&output.stdout),
            );
        }

        Ok(output)
    }
}

pub struct DotnetExceptionInfo {
    pub exception: String,
    pub message: String,
    pub inner_exception: Option<String>,
    pub call_stack: Vec<String>,
    pub hresult: String,
}

pub fn parse_sos_print_exception_output(text: &str) -> Result<DotnetExceptionInfo> {
    use std::io::*;

    use regex::Regex;

    lazy_static::lazy_static! {
        pub static ref EXCEPTION_TYPE: Regex = Regex::new(r#"^Exception type:\s+(.+)$"#).unwrap();
        pub static ref EXCEPTION_MESSAGE: Regex = Regex::new(r#"^Message:\s+(.*)$"#).unwrap();
        pub static ref INNER_EXCEPTION: Regex = Regex::new(r#"^InnerException:\s+(.*)$"#).unwrap();
        pub static ref STACK_FRAME: Regex = Regex::new(r#"^\s*([[:xdigit:]]+) ([[:xdigit:]]+) (.+)$"#).unwrap();
        pub static ref HRESULT: Regex = Regex::new(r#"^HResult:\s+([[:xdigit:]]+)$"#).unwrap();
    }

    let reader = BufReader::new(text.as_bytes());

    let mut exception: Option<String> = None;
    let mut message: Option<String> = None;
    let mut inner_exception: Option<String> = None;
    let mut call_stack: Vec<String> = vec![];
    let mut hresult: Option<String> = None;

    for line in reader.lines() {
        let line = match &line {
            Ok(line) => line,
            Err(err) => {
                warn!("error parsing line: {}", err);
                continue;
            }
        };

        if let Some(captures) = EXCEPTION_TYPE.captures(line) {
            if let Some(c) = captures.get(1) {
                exception = Some(c.as_str().to_owned());
                continue;
            }
        }

        if let Some(captures) = EXCEPTION_MESSAGE.captures(line) {
            if let Some(c) = captures.get(1) {
                message = Some(c.as_str().to_owned());
                continue;
            }
        }

        if let Some(captures) = INNER_EXCEPTION.captures(line) {
            if let Some(c) = captures.get(1) {
                inner_exception = Some(c.as_str().to_owned());
                continue;
            }
        }

        if let Some(captures) = STACK_FRAME.captures(line) {
            if let Some(c) = captures.get(3) {
                let frame = c.as_str().to_owned();
                call_stack.push(frame);
                continue;
            }
        }

        if let Some(captures) = HRESULT.captures(line) {
            if let Some(c) = captures.get(1) {
                hresult = Some(c.as_str().to_owned());
                continue;
            }
        }
    }

    let exception = exception.ok_or_else(|| format_err!("missing exception type"))?;
    let message = message.ok_or_else(|| format_err!("missing exception message"))?;

    let inner_exception = inner_exception.ok_or_else(|| format_err!("missing inner exception"))?;
    let inner_exception = if inner_exception == "<none>" {
        None
    } else {
        Some(inner_exception)
    };

    let hresult = hresult.ok_or_else(|| format_err!("missing exception hresult"))?;

    if call_stack.is_empty() {
        bail!("missing call_stack");
    }

    Ok(DotnetExceptionInfo {
        exception,
        message,
        inner_exception,
        call_stack,
        hresult,
    })
}

// https://docs.microsoft.com/en-us/dotnet/core/diagnostics/dotnet-dump#analyze-sos-commands
const SOS_EXIT: &str = "exit";
const SOS_PRINT_EXCEPTION: &str = "printexception -lines";

#[cfg(test)]
mod tests;