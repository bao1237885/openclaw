//! Job-object escape for the Windows shell.
//!
//! Windows attaches a new process to a job object whenever its launcher is
//! itself job-scoped, and launchers that manage background work (agent
//! terminals, installers, packagers) normally create those jobs with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Every member is then terminated as
//! soon as the job owner exits - which is how a client that was started from
//! such a launcher disappears while its owner restarts, even though the shell
//! never asked to quit and the lifecycle witness records no exit of ours.
//!
//! `CREATE_BREAKAWAY_FROM_JOB` is granted only when the job sets
//! `JOB_OBJECT_LIMIT_BREAKAWAY_OK`. When it is not granted the shell hands the
//! start-up over to a broker that lives outside the job (the shell), because a
//! process cannot escape a job that never granted breakaway - retrying the same
//! `CreateProcess` flag would only repeat the refusal.
//!
//! Jobs nest, and a desktop launcher uses two of them: an inner job that grants
//! breakaway for the command it runs, wrapped in the launcher's own
//! kill-on-close job. Breakaway therefore only clears the inner job, leaving the
//! shell in the outer job that actually kills it when the launcher restarts. A
//! process that finds itself job-scoped straight after a granted breakaway
//! escalates to the broker instead of concluding that escape is impossible.

/// Set on the re-launched child so a job that silently keeps its members cannot
/// cause an endless respawn loop.
pub(crate) const MARKER: &str = "STARSHIP_JOB_BREAKAWAY";

pub(crate) fn ensure_outside_job() {
    #[cfg(target_os = "windows")]
    platform::ensure_outside_job();
}

#[cfg(target_os = "windows")]
mod platform {
    use std::process::{Command, Stdio};

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x0000_0800;
    const JOB_OBJECT_BASIC_PROCESS_ID_LIST: i32 = 3;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    /// The re-launch must not inherit a console, or closing that console would
    /// take the shell down again through `CTRL_CLOSE_EVENT`.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    /// The broker is a console program; without this it paints a black window
    /// on top of the UI it is about to recreate.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// A blocked broker would freeze start-up, so the hand-off is abandoned
    /// (and the shell keeps running in the job) rather than waited on forever.
    const BROKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
    /// A broker hand-off and the escape it performs are the same launch; a
    /// second escape inside this window would be a respawn loop.
    const ESCAPE_THROTTLE_MS: u64 = 8_000;
    /// Buffer capacity for the job's process-id list. Jobs that manage a whole
    /// agent session can hold more members than the reported prefix.
    const MAX_LISTED_MEMBERS: usize = 256;
    const MAX_REPORTED_MEMBERS: usize = 16;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn IsProcessInJob(process: isize, job: isize, result: *mut i32) -> i32;
        fn QueryInformationJobObject(
            job: isize,
            class: i32,
            info: *mut std::ffi::c_void,
            len: u32,
            returned: *mut u32,
        ) -> i32;
    }

    /// Kernel-side shape of the job this process was born into, captured before
    /// any window exists so the log can explain a launch that never survives.
    #[derive(Clone, Copy, Default)]
    struct JobShape {
        in_job: bool,
        kill_on_close: bool,
        breakaway_ok: bool,
    }

    fn job_shape() -> JobShape {
        // SAFETY: every call passes null handles, which makes the kernel answer
        // about the calling process, and a buffer sized by the struct itself.
        unsafe {
            let mut shape = JobShape::default();
            let mut in_job = 0i32;
            if IsProcessInJob(GetCurrentProcess(), 0, &mut in_job) == 0 || in_job == 0 {
                return shape;
            }
            shape.in_job = true;
            let mut info = ExtendedLimitInformation::default();
            let mut returned = 0u32;
            if QueryInformationJobObject(
                0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut info as *mut ExtendedLimitInformation).cast(),
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
                &mut returned,
            ) == 0
            {
                return shape;
            }
            let flags = info.basic_limit_information.limit_flags;
            shape.kill_on_close = flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0;
            shape.breakaway_ok = flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0;
            shape
        }
    }

    /// Member processes of the surrounding job, for the lifecycle log only.
    /// WebView2 and the launcher both create jobs, so the member list is what
    /// tells the two apart when a launch is being investigated after the fact.
    fn job_members() -> String {
        // `JOBOBJECT_BASIC_PROCESS_ID_LIST` is two DWORDs (assigned, listed)
        // followed by `ULONG_PTR` process ids. Reading the header as a u64
        // merges both counts into one bogus number, so the counts are read as
        // the DWORDs the kernel actually writes.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct ProcessIdList {
            assigned: u32,
            listed: u32,
            ids: [u64; MAX_LISTED_MEMBERS],
        }

        let mut list = ProcessIdList {
            assigned: 0,
            listed: 0,
            ids: [0; MAX_LISTED_MEMBERS],
        };
        let mut returned = 0u32;
        // SAFETY: the buffer holds the header plus as many ids as the kernel is
        // allowed to copy into it. A job with more members than that answers
        // ERROR_MORE_DATA; the extra ids are diagnostic, not load-bearing.
        let ok = unsafe {
            QueryInformationJobObject(
                0,
                JOB_OBJECT_BASIC_PROCESS_ID_LIST,
                (&mut list as *mut ProcessIdList).cast(),
                std::mem::size_of::<ProcessIdList>() as u32,
                &mut returned,
            )
        };
        if ok == 0 && list.assigned == 0 && list.listed == 0 {
            return "members=?".to_string();
        }
        let assigned = list.assigned as usize;
        let listed = (list.listed as usize).min(MAX_REPORTED_MEMBERS);
        let ids: Vec<String> = list.ids[..listed]
            .iter()
            .map(|pid| pid.to_string())
            .collect();
        format!("members={assigned} [{},...]", ids.join(","))
    }

    fn state_file(name: &str) -> Option<std::path::PathBuf> {
        let base = std::env::var("LOCALAPPDATA").ok()?;
        let dir = std::path::PathBuf::from(base).join("ai.starship.client");
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.join(name))
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default()
    }

    /// Milliseconds since the previous escape hand-off, if one is still fresh.
    /// Returns the recorded mode and how long ago it happened.
    fn recent_escape() -> Option<(String, u64)> {
        let path = state_file("job-escape.json")?;
        let raw = std::fs::read_to_string(path).ok()?;
        let at: u64 = raw
            .split(|c: char| !c.is_ascii_digit())
            .find(|part| !part.is_empty())?
            .parse()
            .ok()?;
        let mode = raw
            .split_once("\"mode\":\"")
            .and_then(|(_, rest)| rest.split_once('"').map(|(mode, _)| mode.to_string()))
            .unwrap_or_else(|| "unknown".to_string());
        let age = now_ms().saturating_sub(at);
        (age < ESCAPE_THROTTLE_MS).then_some((mode, age))
    }

    fn record_escape(mode: &str) {
        if let Some(path) = state_file("job-escape.json") {
            let _ = std::fs::write(
                path,
                format!("{{\"at\":{},\"mode\":\"{mode}\",\"pid\":{}}}", now_ms(), std::process::id()),
            );
        }
    }

    fn relaunch(program: &std::ffi::OsStr, marker: bool) -> Result<u32, std::io::Error> {
        let mut command = Command::new(program);
        if marker {
            command
                .args(std::env::args_os().skip(1))
                .env(crate::windows_job::MARKER, "1");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS);
        }
        command.spawn().map(|child| child.id())
    }

    /// PowerShell single-quoted literal: every character is taken verbatim, and
    /// an embedded quote is written twice. Paths carry spaces, so the command
    /// line the broker hands to WMI is quoted as well.
    fn ps_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    /// `-EncodedCommand` takes the script as base64 of UTF-16LE, which keeps the
    /// executable path out of every command-line parser between here and
    /// PowerShell.
    fn encode_utf16le_base64(script: &str) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes: Vec<u8> = script
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let mut block = [0u8; 3];
            block[..chunk.len()].copy_from_slice(chunk);
            let packed = u32::from_be_bytes([0, block[0], block[1], block[2]]);
            for index in 0..4 {
                let sextet = (packed >> (18 - index * 6)) & 0x3f;
                // A chunk of `len` bytes fills `len + 1` sextets; the rest are
                // padding, so a full chunk keeps all four.
                encoded.push(if index <= chunk.len() {
                    ALPHABET[sextet as usize] as char
                } else {
                    '='
                });
            }
        }
        encoded
    }

    fn powershell_path() -> std::ffi::OsString {
        if let Some(root) = std::env::var_os("SystemRoot") {
            let candidate = std::path::PathBuf::from(root)
                .join("System32/WindowsPowerShell/v1.0/powershell.exe");
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
        std::ffi::OsString::from("powershell.exe")
    }

    /// Re-launch through the WMI service. `explorer.exe <exe>` reads like a
    /// hand-off but is not one: when the shell is already running, the
    /// `explorer.exe` this starts only forwards the request, and the client
    /// lands back inside the caller's job. The lifecycle log recorded
    /// `job-escape-ineffective mode=broker` on every attempt down that path.
    ///
    /// The WMI provider host (`WmiPrvSE.exe`, a child of `DcomLaunch`) is
    /// outside every launcher job, so a process it creates is born without the
    /// caller's job - measured with `IsProcessInJob`, which answers `no` for a
    /// WMI-created child of a caller that is itself in a kill-on-close job.
    /// PowerShell only carries the request; the created client is not its
    /// child, so this process can exit without taking the client with it.
    fn broker_relaunch(executable: &std::path::Path) -> Result<u32, String> {
        use std::os::windows::process::CommandExt;

        let exe = executable.to_string_lossy();
        let directory = executable
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_default();
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n\
             $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = {command}; CurrentDirectory = {directory} }}\n\
             if ($r.ReturnValue -ne 0) {{ exit 9 }}\n\
             Write-Output $r.ProcessId\n",
            command = ps_literal(&format!("\"{exe}\"")),
            directory = ps_literal(&directory),
        );

        let mut command = Command::new(powershell_path());
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &encode_utf16le_base64(&script),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // A blocked broker must not freeze start-up, and the broker outliving
        // the wait would only mean the client appears a moment later; the
        // recorded hand-off keeps that from becoming a respawn loop.
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut command = command;
            let _ = sender.send(command.output().map_err(|error| {
                format!("spawn-failed os_error={:?}", error.raw_os_error())
            }));
        });
        let output = match receiver.recv_timeout(BROKER_TIMEOUT) {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(format!("timeout after {}s", BROKER_TIMEOUT.as_secs())),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let pid = stdout
            .lines()
            .rev()
            .map(str::trim)
            .find_map(|line| line.parse::<u32>().ok());
        match pid {
            Some(pid) => Ok(pid),
            None => Err(format!(
                "no-pid status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    }

    pub(super) fn ensure_outside_job() {
        let escaped = std::env::var_os(crate::windows_job::MARKER).is_some();
        let shape = job_shape();
        // Windows puts processes into jobs for many reasons that have nothing to
        // do with a launcher's lifetime (WebView2 renderers, conhost, the Task
        // Scheduler). Only `KILL_ON_JOB_CLOSE` decides whether this process dies
        // with its launcher, so the probe is recorded for every job-scoped
        // launch and the escape happens for that flag alone.
        if shape.in_job {
            crate::shell_lifecycle::event(&format!(
                "job-probe in_job=true kill_on_close={} breakaway_ok={} escaped={escaped} {}",
                shape.kill_on_close,
                shape.breakaway_ok,
                job_members()
            ));
        }
        if !shape.kill_on_close {
            if escaped {
                crate::shell_lifecycle::event("job-escape-ok (outside kill-on-close job)");
            }
            return;
        }
        let diagnostic = format!(
            "job-detected kill_on_close=true breakaway_ok={} escaped={escaped} {}",
            shape.breakaway_ok,
            job_members()
        );
        // Launchers like a desktop app nest their jobs: escaping the inner
        // breakaway-allowing job lands the shell in the launcher's own
        // kill-on-close job, which is the one that actually kills it when the
        // launcher restarts. The recorded hand-off is what tells "I am the
        // child of that escape" apart from an unrelated launch, so a trapped
        // child can escalate to the broker without an unbounded respawn loop.
        let handoff = recent_escape();
        match &handoff {
            // The broker already ran and this process is still inside a
            // kill-on-close job, so there is no stronger hand-off left to try.
            // `broker` is the retired `explorer.exe` mode: an install that
            // predates the WMI broker may have left it behind.
            Some((mode, age)) if mode == "wmi" || mode == "broker" => {
                crate::shell_lifecycle::event(&format!(
                    "job-escape-ineffective mode={mode} age_ms={age} {diagnostic}"
                ));
                return;
            }
            // A fresh hand-off that this process did not come from: another
            // escape is already in flight for it.
            Some((_, age)) if !escaped => {
                crate::shell_lifecycle::event(&format!(
                    "job-escape-throttled age_ms={age} {diagnostic}"
                ));
                return;
            }
            _ => {}
        }
        // `CREATE_BREAKAWAY_FROM_JOB` only works against a job that granted it.
        // Asking anyway costs a refused spawn and a log line, so the flag is
        // only used when the job advertises the right to break away; the broker
        // handles the jobs that do not.
        let breakaway_first = handoff.is_none() && shape.breakaway_ok;
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        crate::shell_lifecycle::event(&diagnostic);

        if breakaway_first {
            record_escape("breakaway");
            match relaunch(executable.as_os_str(), true) {
                Ok(child) => {
                    // Nothing has been created yet - no window, no WebView2
                    // profile lock - so handing over is invisible.
                    crate::shell_lifecycle::event(&format!("job-breakaway-relaunch child={child}"));
                    std::process::exit(0);
                }
                Err(error) => {
                    crate::shell_lifecycle::event(&format!(
                        "job-breakaway-refused os_error={:?} (brokering through WMI)",
                        error.raw_os_error()
                    ));
                }
            }
        }

        record_escape("wmi");
        match broker_relaunch(&executable) {
            Ok(child) => {
                crate::shell_lifecycle::event(&format!(
                    "job-broker-relaunch child={child} broker=powershell-wmi"
                ));
                std::process::exit(0);
            }
            // Refusing to start would be worse than being killable, so the shell
            // keeps the documented in-job behaviour when every escape fails.
            Err(error) => {
                crate::shell_lifecycle::event(&format!("job-broker-refused {error} (staying in the job)"))
            }
        }
    }

    // Expected values are the output of an independent encoder
    // (`[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($s))`).
    // A previous revision dropped a padding character, so PowerShell rejected
    // the whole `-EncodedCommand` and the escape silently fell back to an
    // in-job start.
    #[cfg(test)]
    mod tests {
        use super::encode_utf16le_base64;

        #[test]
        fn encodes_utf16le_base64_with_correct_padding() {
            assert_eq!(encode_utf16le_base64("A"), "QQA=");
            assert_eq!(encode_utf16le_base64("AB"), "QQBCAA==");
            assert_eq!(encode_utf16le_base64("ABC"), "QQBCAEMA");
            assert_eq!(encode_utf16le_base64(""), "");
        }

        #[test]
        fn encodes_paths_with_spaces_and_quotes() {
            assert_eq!(
                encode_utf16le_base64("& 'C:\\Program Files\\Starship\\starship.exe'"),
                "JgAgACcAQwA6AFwAUAByAG8AZwByAGEAbQAgAEYAaQBsAGUAcwBcAFMAdABhAHIAcwBoAGkAcABcAHMAdABhAHIAcwBoAGkAcAAuAGUAeABlACcA"
            );
        }
    }
}
