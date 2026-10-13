// This file has local integration changes; see vendor/CHANGES.md.
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{Error, ErrorKind, Result};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{FreeLibrary, HANDLE, S_OK};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};
use windows_sys::core::HRESULT;
use windows_sys::s;

use crate::event::{OnResize, WindowSize};
use crate::tty::Options;
use crate::tty::windows::blocking::{UnblockedReader, UnblockedWriter};
use crate::tty::windows::child::ChildExitWatcher;
use crate::tty::windows::control::create_job;
use crate::tty::windows::{Pty, cmdline, win32_string};

const PIPE_CAPACITY: usize = crate::event_loop::READ_BUFFER_SIZE;
type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

struct ConptyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
    module: Option<isize>,
}
impl ConptyApi {
    fn new(runtime: Option<&Path>) -> Result<Self> {
        match runtime {
            Some(path) => Self::load_conpty(path),
            None => {
                info!("Using Windows API for pseudoconsole");
                Ok(Self {
                    create: CreatePseudoConsole,
                    resize: ResizePseudoConsole,
                    close: ClosePseudoConsole,
                    module: None,
                })
            }
        }
    }
    fn load_conpty(path: &Path) -> Result<Self> {
        if !path.is_absolute()
            || !path.is_file()
            || !path
                .parent()
                .is_some_and(|parent| parent.join("OpenConsole.exe").is_file())
            || path.as_os_str().encode_wide().any(|unit| unit == 0)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "ConPTY requires an absolute DLL path and its paired OpenConsole host",
            ));
        }
        type LoadedFn = unsafe extern "system" fn() -> isize;
        unsafe {
            let path = win32_string(path);
            let module = LoadLibraryExW(
                path.as_ptr(),
                ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if module.is_null() {
                return Err(Error::last_os_error());
            }
            let functions = (
                GetProcAddress(module, s!("ConptyCreatePseudoConsole")),
                GetProcAddress(module, s!("ConptyResizePseudoConsole")),
                GetProcAddress(module, s!("ConptyClosePseudoConsole")),
            );
            if let (Some(create), Some(resize), Some(close)) = functions {
                info!("Using explicit Microsoft ConPTY runtime");
                Ok(Self {
                    create: mem::transmute::<LoadedFn, CreatePseudoConsoleFn>(create),
                    resize: mem::transmute::<LoadedFn, ResizePseudoConsoleFn>(resize),
                    close: mem::transmute::<LoadedFn, ClosePseudoConsoleFn>(close),
                    module: Some(module as isize),
                })
            } else {
                FreeLibrary(module);
                Err(Error::new(
                    ErrorKind::Unsupported,
                    "ConPTY runtime is missing required exports",
                ))
            }
        }
    }
}
impl Drop for ConptyApi {
    fn drop(&mut self) {
        if let Some(module) = self.module {
            unsafe {
                FreeLibrary(module as _);
            }
        }
    }
}

/// Owns the pseudoconsole and its asynchronous close operation.
pub struct Conpty {
    pub handle: HPCON,
    api: Arc<ConptyApi>,
    pub(crate) job: Option<OwnedHandle>,
    closing: Option<JoinHandle<()>>,
}
impl Conpty {
    pub fn start_close(&mut self) -> Result<()> {
        if self.handle == 0 {
            return Ok(());
        }
        if let Some(job) = &self.job {
            if unsafe { TerminateJobObject(job.as_raw_handle().cast(), 0) } == 0 {
                return Err(Error::last_os_error());
            }
        }
        let handle = self.handle;
        let api = self.api.clone();
        // Windows 10 may wait for output drain. Keep its read/parser thread running.
        let closing = thread::Builder::new()
            .name("pty-console-close".into())
            .spawn(move || unsafe { (api.close)(handle) })?;
        self.handle = 0;
        self.closing = Some(closing);
        Ok(())
    }
    pub fn try_resize(&self, size: WindowSize) -> Result<()> {
        validate_size(size)?;
        if self.handle == 0 {
            return Err(ErrorKind::BrokenPipe.into());
        }
        let result = unsafe { (self.api.resize)(self.handle, size.into()) };
        if result == S_OK {
            Ok(())
        } else {
            Err(Error::other(format!(
                "ResizePseudoConsole failed: {result:#x}"
            )))
        }
    }
}
impl Drop for Conpty {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { (self.api.close)(self.handle) }
        }
        if let Some(closing) = self.closing.take() {
            if closing.join().is_err() {
                log::error!("Pseudoconsole close thread failed");
            }
        }
    }
}
struct AttributeList {
    storage: Box<[usize]>,
}
impl AttributeList {
    fn new() -> Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(Error::last_os_error());
        }
        let mut storage = vec![0; size.div_ceil(mem::size_of::<usize>())].into_boxed_slice();
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut size)
        } == 0
        {
            return Err(Error::last_os_error());
        }
        Ok(Self { storage })
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast());
        }
    }
}

pub fn new(config: &Options, size: WindowSize) -> Result<Pty> {
    validate_size(size)?;
    if config
        .env
        .iter()
        .any(|(key, value)| key.is_empty() || key.contains(['\0', '=']) || value.contains('\0'))
        || config.shell.as_ref().is_some_and(|shell| {
            shell.program.contains('\0') || shell.args.iter().any(|arg| arg.contains('\0'))
        })
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid child arguments or environment",
        ));
    }
    let api = Arc::new(ConptyApi::new(config.conpty_runtime.as_deref())?);
    let (conout, output_peer) = miow::pipe::anonymous(0)?;
    let (input_peer, conin) = miow::pipe::anonymous(0)?;
    let mut handle = 0;
    let result = unsafe {
        (api.create)(
            size.into(),
            input_peer.as_raw_handle().cast(),
            output_peer.as_raw_handle().cast(),
            0,
            &mut handle,
        )
    };
    if result != S_OK {
        return Err(Error::other(format!(
            "CreatePseudoConsole failed: {result:#x}"
        )));
    }
    drop(input_peer);
    drop(output_peer);
    let mut conout = UnblockedReader::new(conout, PIPE_CAPACITY);
    let conin = UnblockedWriter::new(conin, PIPE_CAPACITY);
    let mut console = Conpty {
        handle,
        api,
        job: None,
        closing: None,
    };
    let result = (|| {
        let mut attributes = AttributeList::new()?;
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.storage.as_mut_ptr().cast(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                handle as *mut std::ffi::c_void,
                mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(Error::last_os_error());
        }
        let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
        startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = attributes.storage.as_mut_ptr().cast();
        let mut command = win32_string(&cmdline(config));
        if command.len() > 32767 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Child command line is too long",
            ));
        }
        let application = config
            .shell
            .as_ref()
            .filter(|shell| Path::new(&shell.program).is_absolute())
            .map(|shell| win32_string(&shell.program));
        let cwd = config.working_directory.as_ref().map(win32_string);
        let environment = convert_custom_env(&config.env, !config.clear_environment);
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_SUSPENDED
            | if environment.is_some() {
                CREATE_UNICODE_ENVIRONMENT
            } else {
                0
            };
        let mut process_info: PROCESS_INFORMATION = unsafe { mem::zeroed() };
        if unsafe {
            CreateProcessW(
                application
                    .as_ref()
                    .map_or(ptr::null(), |value| value.as_ptr()),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                flags,
                environment
                    .as_ref()
                    .map_or(ptr::null(), |block| block.as_ptr().cast()),
                cwd.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
                &startup.StartupInfo,
                &mut process_info,
            )
        } == 0
        {
            return Err(Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess.cast()) };
        let thread = unsafe { OwnedHandle::from_raw_handle(process_info.hThread.cast()) };
        if config.manage_process_tree {
            match create_job(process.as_raw_handle().cast()) {
                Ok(job) => console.job = Some(job),
                Err(error) => {
                    unsafe {
                        TerminateProcess(process.as_raw_handle().cast(), 1);
                    }
                    return Err(error);
                }
            }
        }
        let watcher = ChildExitWatcher::new_owned(process)?;
        if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
            let error = Error::last_os_error();
            unsafe {
                TerminateProcess(watcher.raw_handle(), 1);
            }
            return Err(error);
        }
        Ok(watcher)
    })();
    match result {
        Ok(watcher) => Ok(Pty::new(console, conout, conin, watcher)),
        Err(error) => {
            conout.discard_remaining();
            if let Err(close) = console.start_close() {
                log::error!("Failed-spawn console cleanup: {close}");
            }
            drop(console);
            Err(error)
        }
    }
}
fn validate_size(size: WindowSize) -> Result<()> {
    if size.num_cols == 0
        || size.num_lines == 0
        || size.num_cols > i16::MAX as u16
        || size.num_lines > i16::MAX as u16
    {
        Err(Error::new(
            ErrorKind::InvalidInput,
            "Invalid pseudoconsole dimensions",
        ))
    } else {
        Ok(())
    }
}
fn convert_custom_env(custom: &HashMap<String, String>, inherit: bool) -> Option<Vec<u16>> {
    if custom.is_empty() && inherit {
        return None;
    }
    let mut block = Vec::new();
    let mut keys = HashSet::new();
    let mut entries: Vec<(OsString, OsString)> = Vec::new();
    for (key, value) in custom {
        if keys.insert(OsStr::new(key).to_ascii_uppercase()) {
            entries.push((key.into(), value.into()));
        } else {
            warn!("Omitting duplicate environment key '{key}'");
        }
    }
    if inherit {
        for (key, value) in std::env::vars_os() {
            if keys.insert(key.to_ascii_uppercase()) {
                entries.push((key, value));
            }
        }
    }
    entries.sort_by_key(|(key, _)| key.to_ascii_uppercase());
    for (key, value) in entries {
        add_windows_env_key_value_to_block(&mut block, &key, &value);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Some(block)
}
fn add_windows_env_key_value_to_block(block: &mut Vec<u16>, key: &OsStr, value: &OsStr) {
    block.extend(key.encode_wide());
    block.push('=' as u16);
    block.extend(value.encode_wide());
    block.push(0);
}
impl OnResize for Conpty {
    fn on_resize(&mut self, size: WindowSize) {
        if let Err(error) = self.try_resize(size) {
            log::error!("Pseudoconsole resize failed: {error}");
        }
    }
}
impl From<WindowSize> for COORD {
    fn from(size: WindowSize) -> Self {
        Self {
            X: size.num_cols as i16,
            Y: size.num_lines as i16,
        }
    }
}
