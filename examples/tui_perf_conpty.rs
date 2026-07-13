//! Windows-only off-screen ConPTY launcher for the performance harness.
//!
//! The child receives an exact terminal geometry and a private, empty input pipe. A Job Object
//! with `KILL_ON_JOB_CLOSE` contains the complete child tree, while the ConPTY output is drained
//! so a chatty TUI cannot block on pipe backpressure. The launcher emits an atomic proof record
//! that the PowerShell orchestrator can bind to the measured owner PID.

#[cfg(not(windows))]
fn main() {
    eprintln!("tui_perf_conpty is only supported on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
mod windows_launcher {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, c_void};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;
    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW,
        TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    const SCHEMA: &str = "ytt.tui-perf.conpty.v1";
    const TIMEOUT_EXIT_CODE: u32 = 124;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(
            read_pipe: *mut HANDLE,
            write_pipe: *mut HANDLE,
            attributes: *const SECURITY_ATTRIBUTES,
            size: u32,
        ) -> i32;
    }

    #[derive(Debug)]
    struct Args {
        width: i16,
        height: i16,
        timeout_ms: u32,
        proof: PathBuf,
        environment_json: PathBuf,
        working_directory: PathBuf,
        command: Vec<String>,
    }

    impl Args {
        fn parse() -> Result<Self, String> {
            let mut width = None;
            let mut height = None;
            let mut timeout_secs = 0u32;
            let mut proof = None;
            let mut environment_json = None;
            let mut working_directory = None;
            let raw = std::env::args().skip(1).collect::<Vec<_>>();
            let separator = raw
                .iter()
                .position(|value| value == "--")
                .ok_or_else(|| "missing `--` before the child command".to_string())?;
            let command = raw[separator + 1..].to_vec();
            if command.is_empty() {
                return Err("child command must not be empty".to_string());
            }
            let mut index = 0;
            while index < separator {
                let name = &raw[index];
                index += 1;
                let value = raw
                    .get(index)
                    .ok_or_else(|| format!("{name} requires a value"))?;
                index += 1;
                match name.as_str() {
                    "--width" => width = Some(parse_dimension(value, "width")?),
                    "--height" => height = Some(parse_dimension(value, "height")?),
                    "--timeout-secs" => {
                        timeout_secs = value
                            .parse::<u32>()
                            .ok()
                            .filter(|seconds| *seconds > 0)
                            .ok_or_else(|| {
                                "--timeout-secs must be a positive integer".to_string()
                            })?;
                    }
                    "--proof" => proof = Some(PathBuf::from(value)),
                    "--environment-json" => environment_json = Some(PathBuf::from(value)),
                    "--working-directory" => working_directory = Some(PathBuf::from(value)),
                    other => return Err(format!("unknown argument `{other}`")),
                }
            }
            let timeout_ms = timeout_secs
                .checked_mul(1_000)
                .ok_or_else(|| "--timeout-secs is too large".to_string())?;
            Ok(Self {
                width: width.ok_or_else(|| "--width is required".to_string())?,
                height: height.ok_or_else(|| "--height is required".to_string())?,
                timeout_ms,
                proof: proof.ok_or_else(|| "--proof is required".to_string())?,
                environment_json: environment_json
                    .ok_or_else(|| "--environment-json is required".to_string())?,
                working_directory: working_directory
                    .ok_or_else(|| "--working-directory is required".to_string())?,
                command,
            })
        }
    }

    fn parse_dimension(value: &str, label: &str) -> Result<i16, String> {
        value
            .parse::<i16>()
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or_else(|| format!("--{label} must be in 1..={}", i16::MAX))
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE, label: &str) -> Result<Self, String> {
            if handle.is_null() {
                Err(last_error(label))
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> HANDLE {
            self.0
        }

        fn into_raw(mut self) -> HANDLE {
            let handle = self.0;
            self.0 = null_mut();
            handle
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this wrapper uniquely owns a valid Win32 handle.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct PseudoConsole(HPCON);

    impl Drop for PseudoConsole {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: this wrapper uniquely owns the HPCON returned by CreatePseudoConsole.
                unsafe { ClosePseudoConsole(self.0) };
            }
        }
    }

    struct AttributeList {
        _storage: Vec<u8>,
        pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.pointer.is_null() {
                // SAFETY: the list was initialized exactly once and remains backed by storage.
                unsafe { DeleteProcThreadAttributeList(self.pointer) };
            }
        }
    }

    pub fn main() -> Result<i32, String> {
        let args = Args::parse()?;
        if args.proof.exists() {
            return Err(format!(
                "proof output already exists: {}",
                args.proof.display()
            ));
        }
        let started_unix_ns = unix_ns()?;
        let (input_read, input_write) = create_pipe("ConPTY input pipe")?;
        let (output_read, output_write) = create_pipe("ConPTY output pipe")?;

        // Only the launcher owns these two endpoints. The ConPTY owns their peers.
        set_not_inheritable(input_write.raw(), "ConPTY input writer")?;
        set_not_inheritable(output_read.raw(), "ConPTY output reader")?;

        let mut hpcon = 0;
        let size = COORD {
            X: args.width,
            Y: args.height,
        };
        // SAFETY: both pipe handles are live for the call and hpcon points to valid storage.
        let result = unsafe {
            CreatePseudoConsole(size, input_read.raw(), output_write.raw(), 0, &mut hpcon)
        };
        if result < 0 {
            return Err(format!(
                "CreatePseudoConsole failed with HRESULT 0x{result:08x}"
            ));
        }
        let pseudo_console = PseudoConsole(hpcon);
        drop(input_read);
        drop(output_write);

        let attribute_list = create_attribute_list(hpcon)?;
        let job = create_kill_on_close_job()?;
        let environment = load_environment(&args.environment_json)?;
        let working_directory = args.working_directory.canonicalize().map_err(|error| {
            format!(
                "canonicalize working directory {}: {error}",
                args.working_directory.display()
            )
        })?;
        let process = create_suspended_process(
            &args.command,
            &environment,
            &working_directory,
            &attribute_list,
        )?;
        let process_handle = OwnedHandle::new(process.hProcess, "child process handle")?;
        let thread_handle = OwnedHandle::new(process.hThread, "child thread handle")?;
        // SAFETY: job and process are live kernel handles; the child is still suspended.
        if unsafe { AssignProcessToJobObject(job.raw(), process_handle.raw()) } == 0 {
            // SAFETY: the child is still suspended and uniquely represented by process_handle.
            unsafe { TerminateProcess(process_handle.raw(), 2) };
            return Err(last_error("AssignProcessToJobObject"));
        }
        // SAFETY: hThread belongs to the newly created suspended primary thread.
        if unsafe { ResumeThread(thread_handle.raw()) } == u32::MAX {
            // SAFETY: the child is now contained by the kill-on-close job.
            unsafe { TerminateJobObject(job.raw(), 2) };
            return Err(last_error("ResumeThread"));
        }

        let output_handle = output_read.into_raw() as usize;
        let drain = std::thread::spawn(move || -> Result<u64, String> {
            // SAFETY: ownership of this live pipe handle moved into the thread exactly once.
            let mut output = unsafe { File::from_raw_handle(output_handle as HANDLE) };
            let mut bytes = 0u64;
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match output.read(&mut buffer) {
                    Ok(0) => return Ok(bytes),
                    Ok(count) => bytes = bytes.saturating_add(count as u64),
                    Err(error) => return Err(format!("drain ConPTY output: {error}")),
                }
            }
        });

        // Keep the private input writer open but never write a byte: controlled empty input.
        // SAFETY: process_handle remains live for the call and timeout_ms is a valid wait bound.
        let wait_result = unsafe { WaitForSingleObject(process_handle.raw(), args.timeout_ms) };
        let timed_out = match wait_result {
            WAIT_OBJECT_0 => false,
            WAIT_TIMEOUT => {
                // SAFETY: the job contains the suspended-before-assignment child tree.
                if unsafe { TerminateJobObject(job.raw(), TIMEOUT_EXIT_CODE) } == 0 {
                    return Err(last_error("TerminateJobObject"));
                }
                // SAFETY: process_handle remains live until after this wait.
                unsafe { WaitForSingleObject(process_handle.raw(), INFINITE) };
                true
            }
            other => {
                return Err(format!(
                    "WaitForSingleObject returned unexpected status {other}"
                ));
            }
        };
        let mut exit_code = 0u32;
        // SAFETY: process_handle refers to a signaled process and exit_code is writable.
        if unsafe { GetExitCodeProcess(process_handle.raw(), &mut exit_code) } == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }

        drop(input_write);
        drop(thread_handle);
        drop(process_handle);
        // Close the kill-on-close job before waiting for pipe EOF so a stray descendant cannot
        // retain the pseudoconsole and deadlock the output drain after the sampler has exited.
        drop(job);
        drop(pseudo_console);
        let output_bytes = drain
            .join()
            .map_err(|_| "ConPTY output drain thread panicked".to_string())??;
        let finished_unix_ns = unix_ns()?;
        let proof = json!({
            "schema": SCHEMA,
            "child_pid": process.dwProcessId,
            "geometry": {"width": args.width, "height": args.height},
            "private_conpty": true,
            "inherited_parent_console": false,
            "controlled_empty_input": true,
            "job_kill_on_close": true,
            "command": args.command,
            "environment_policy": "explicit_unicode_environment_block_v1",
            "environment_json": args.environment_json,
            "environment_sha256": sha256_file(&args.environment_json)?,
            "environment_keys": environment.keys().collect::<Vec<_>>(),
            "working_directory": args.working_directory,
            "canonical_working_directory": working_directory,
            "timeout_ms": args.timeout_ms,
            "timed_out": timed_out,
            "exit_code": exit_code,
            "output_bytes": output_bytes,
            "started_unix_ns": started_unix_ns,
            "finished_unix_ns": finished_unix_ns,
        });
        write_atomic_json(&args.proof, &proof)?;
        Ok(exit_code as i32)
    }

    fn create_pipe(label: &str) -> Result<(OwnedHandle, OwnedHandle), String> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut read = null_mut();
        let mut write = null_mut();
        // SAFETY: pointers target writable handle storage and attributes is fully initialized.
        if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
            return Err(last_error(label));
        }
        Ok((
            OwnedHandle::new(read, label)?,
            OwnedHandle::new(write, label)?,
        ))
    }

    fn set_not_inheritable(handle: HANDLE, label: &str) -> Result<(), String> {
        // SAFETY: handle is live and owned by this process.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            Err(last_error(label))
        } else {
            Ok(())
        }
    }

    fn create_attribute_list(hpcon: HPCON) -> Result<AttributeList, String> {
        let mut bytes = 0usize;
        // SAFETY: a null first call is the documented size query for one attribute.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(last_error("InitializeProcThreadAttributeList size query"));
        }
        let mut storage = vec![0u8; bytes];
        let pointer = storage.as_mut_ptr().cast::<c_void>();
        // SAFETY: storage has the exact requested size and remains alive in AttributeList.
        if unsafe { InitializeProcThreadAttributeList(pointer, 1, 0, &mut bytes) } == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        // SAFETY: pointer names a valid list and lpvalue points to a live HPCON value.
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                (&hpcon as *const HPCON).cast::<c_void>(),
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            // SAFETY: initialization succeeded, but ownership has not yet moved to a guard.
            unsafe { DeleteProcThreadAttributeList(pointer) };
            return Err(last_error("UpdateProcThreadAttribute(PSEUDOCONSOLE)"));
        }
        Ok(AttributeList {
            _storage: storage,
            pointer,
        })
    }

    fn create_kill_on_close_job() -> Result<OwnedHandle, String> {
        // SAFETY: null arguments request an unnamed job with default security.
        let job = OwnedHandle::new(
            unsafe { CreateJobObjectW(null(), null()) },
            "CreateJobObjectW",
        )?;
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: information points to the declared structure for this information class.
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(last_error("SetInformationJobObject(KILL_ON_JOB_CLOSE)"));
        }
        Ok(job)
    }

    fn create_suspended_process(
        command: &[String],
        environment: &BTreeMap<String, String>,
        working_directory: &Path,
        attributes: &AttributeList,
    ) -> Result<PROCESS_INFORMATION, String> {
        let command_line = command
            .iter()
            .map(|argument| quote_windows_argument(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let mut wide = OsStr::new(&command_line)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let environment_block = unicode_environment_block(environment)?;
        let working_directory = working_directory
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes.pointer;
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: command line is writable/NUL-terminated; startup and process match the flags.
        if unsafe {
            CreateProcessW(
                null(),
                wide.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                environment_block.as_ptr().cast::<c_void>(),
                working_directory.as_ptr(),
                (&startup as *const STARTUPINFOEXW).cast(),
                &mut process,
            )
        } == 0
        {
            return Err(last_error("CreateProcessW(ConPTY)"));
        }
        Ok(process)
    }

    fn load_environment(path: &Path) -> Result<BTreeMap<String, String>, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read environment {}: {error}", path.display()))?;
        let environment: BTreeMap<String, String> = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse environment {}: {error}", path.display()))?;
        if environment.is_empty() {
            return Err("explicit child environment must not be empty".to_string());
        }
        for (name, value) in &environment {
            if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0')
            {
                return Err(format!("invalid child environment entry {name:?}"));
            }
        }
        Ok(environment)
    }

    fn unicode_environment_block(
        environment: &BTreeMap<String, String>,
    ) -> Result<Vec<u16>, String> {
        let mut entries = environment
            .iter()
            .map(|(name, value)| (name.to_uppercase(), format!("{name}={value}")))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err("child environment contains case-insensitive duplicate keys".to_string());
        }
        let mut block = Vec::new();
        for (_sort_key, entry) in entries {
            block.extend(OsStr::new(&entry).encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    fn quote_windows_argument(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|character| matches!(character, ' ' | '\t' | '"'))
        {
            return argument.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
                continue;
            }
            if character == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(character);
            }
            backslashes = 0;
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn unix_ns() -> Result<u128, String> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .map_err(|error| format!("system clock precedes Unix epoch: {error}"))
    }

    fn sha256_file(path: &Path) -> Result<String, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read {} for SHA-256: {error}", path.display()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn write_atomic_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create proof directory {}: {error}", parent.display()))?;
        }
        let temporary = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        serde_json::to_writer(&mut file, value)
            .map_err(|error| format!("serialize {}: {error}", path.display()))?;
        file.write_all(b"\n")
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("flush {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("publish {}: {error}", path.display()))
    }

    fn last_error(operation: &str) -> String {
        format!("{operation}: {}", std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn main() {
    let exit_code = match windows_launcher::main() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("tui_perf_conpty: {message}");
            2
        }
    };
    std::process::exit(exit_code);
}
