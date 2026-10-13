//! Windows job ownership is assigned atomically by the process creation attributes.

use std::{
    fs::File,
    io, mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};

use alacritty_terminal::tty::windows::{
    control::{ProcessControl, create_process_job},
    escaped_command_line,
};
use windows_sys::Win32::{
    Foundation::{
        GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    },
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, UpdateProcThreadAttribute,
        },
    },
};
use windows_sys::w;

use super::CommandRequest;
use crate::{Error, ErrorKind, Result};

pub(super) struct OwnedProcess {
    pub control: ProcessControl,
    pub stdout: File,
    pub stderr: File,
}

pub(super) fn spawn(request: &CommandRequest) -> Result<OwnedProcess> {
    spawn_inner(request).map_err(|error| {
        Error::new(
            ErrorKind::Process,
            format!("Create controlled process: {error}"),
        )
    })
}

fn spawn_inner(request: &CommandRequest) -> io::Result<OwnedProcess> {
    let invocation =
        super::command::resolve(&request.program, &request.arguments, &request.environment)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let program = wide(invocation.program.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Executable must be Unicode")
    })?)?;
    let cwd = wide(request.cwd.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Working directory must be Unicode",
        )
    })?)?;
    let command_line = if invocation.verbatim {
        let base = escaped_command_line(invocation.program.to_str().unwrap(), &[]);
        format!("{base} {}", invocation.arguments.join(" "))
    } else {
        escaped_command_line(invocation.program.to_str().unwrap(), &invocation.arguments)
    };
    let mut command = wide(&command_line)?;
    if command.len() > 32767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Command line is too long",
        ));
    }
    let mut environment = Vec::new();
    for (key, value) in &request.environment {
        environment.extend(format!("{key}={value}").encode_utf16());
        environment.push(0);
    }
    if environment.is_empty() {
        environment.push(0);
    }
    environment.push(0);

    let security = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let input_raw = unsafe {
        CreateFileW(
            w!("NUL"),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &security,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if input_raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let input = unsafe { OwnedHandle::from_raw_handle(input_raw.cast()) };
    let (stdout, child_stdout) = pipe(&security)?;
    let (stderr, child_stderr) = pipe(&security)?;
    let job = create_process_job()?;
    let stdio = [
        input.as_raw_handle().cast::<std::ffi::c_void>(),
        child_stdout.as_raw_handle().cast(),
        child_stderr.as_raw_handle().cast(),
    ];
    let jobs = [job.as_raw_handle().cast::<std::ffi::c_void>()];
    let mut attributes = Attributes::new()?;
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            stdio.as_ptr().cast_mut().cast(),
            mem::size_of_val(&stdio),
        )?;
        attributes.set(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr().cast_mut().cast(),
            mem::size_of_val(&jobs),
        )?;
    }
    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = input.as_raw_handle().cast();
    startup.StartupInfo.hStdOutput = child_stdout.as_raw_handle().cast();
    startup.StartupInfo.hStdError = child_stderr.as_raw_handle().cast();
    startup.lpAttributeList = attributes.storage.as_mut_ptr().cast();
    let mut process: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    // Both lists and their uniquely owned handles outlive this synchronous call.
    // Job-list assignment happens before the child can spawn descendants.
    if unsafe {
        CreateProcessW(
            program.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let process_handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess.cast()) };
    let _thread_handle = unsafe { OwnedHandle::from_raw_handle(process.hThread.cast()) };
    Ok(OwnedProcess {
        control: ProcessControl::from_owned(process_handle, job),
        stdout: File::from(stdout),
        stderr: File::from(stderr),
    })
}

fn pipe(security: &SECURITY_ATTRIBUTES) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut reader: HANDLE = ptr::null_mut();
    let mut writer: HANDLE = ptr::null_mut();
    if unsafe { CreatePipe(&mut reader, &mut writer, security, 65536) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let reader = unsafe { OwnedHandle::from_raw_handle(reader.cast()) };
    let writer = unsafe { OwnedHandle::from_raw_handle(writer.cast()) };
    if unsafe { SetHandleInformation(reader.as_raw_handle().cast(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((reader, writer))
}

struct Attributes {
    storage: Box<[usize]>,
}
impl Attributes {
    fn new() -> io::Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0; size.div_ceil(mem::size_of::<usize>())].into_boxed_slice();
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 2, 0, &mut size)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { storage })
    }
    unsafe fn set(
        &mut self,
        key: usize,
        value: *mut std::ffi::c_void,
        length: usize,
    ) -> io::Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.storage.as_mut_ptr().cast(),
                0,
                key,
                value,
                length,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast());
        }
    }
}
fn wide(value: &str) -> io::Result<Vec<u16>> {
    if value.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in process path",
        ));
    }
    Ok(value.encode_utf16().chain([0]).collect())
}
