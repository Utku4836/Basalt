// This file has local integration changes; see vendor/CHANGES.md.
//! Owned process and job handles for the embedding application's lifecycle controller.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;
use std::{io, mem, ptr};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessId, TerminateProcess, WaitForSingleObject,
};

pub struct ProcessControl {
    process: OwnedHandle,
    job: Option<OwnedHandle>,
}

impl ProcessControl {
    /// Own handles created by the embedding application's native process launcher.
    pub fn from_owned(process: OwnedHandle, job: OwnedHandle) -> Self {
        Self {
            process,
            job: Some(job),
        }
    }
    pub(crate) fn duplicate(process: HANDLE, job: Option<&OwnedHandle>) -> io::Result<Self> {
        Ok(Self {
            process: duplicate(process)?,
            job: job
                .map(|handle| duplicate(handle.as_raw_handle().cast()))
                .transpose()?,
        })
    }

    pub fn pid(&self) -> u32 {
        unsafe { GetProcessId(self.process.as_raw_handle().cast()) }
    }

    pub fn exit_status(&self) -> io::Result<Option<ExitStatus>> {
        self.wait_exit(Duration::ZERO)
    }

    pub fn wait_exit(&self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let handle = self.process.as_raw_handle().cast();
        let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        match unsafe { WaitForSingleObject(handle, millis) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn terminate(&self, code: u32) -> io::Result<()> {
        let succeeded = unsafe {
            match &self.job {
                Some(job) => TerminateJobObject(job.as_raw_handle().cast(), code),
                None => {
                    if self.exit_status()?.is_some() {
                        return Ok(());
                    }
                    TerminateProcess(self.process.as_raw_handle().cast(), code)
                }
            }
        };
        if succeeded == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub fn create_process_job() -> io::Result<OwnedHandle> {
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // The successful API call transfers a unique job handle to this RAII owner.
    let job = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

pub(crate) fn create_job(process: HANDLE) -> io::Result<OwnedHandle> {
    let job = create_process_job()?;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle().cast(), process) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn duplicate(raw: HANDLE) -> io::Result<OwnedHandle> {
    let current = unsafe { GetCurrentProcess() };
    let mut result = ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            current,
            raw,
            current,
            &mut result,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(result.cast()) })
}
