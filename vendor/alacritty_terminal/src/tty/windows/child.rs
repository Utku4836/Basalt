// This file has local integration changes; see vendor/CHANGES.md.
use std::ffi::c_void;
use std::io::Error;
use std::num::NonZeroU32;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use polling::os::iocp::{CompletionPacket, PollerIocpExt};
use polling::{Event, Poller};

use windows_sys::Win32::Foundation::{BOOLEAN, FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, INFINITE, RegisterWaitForSingleObject, TerminateProcess,
    UnregisterWaitEx, WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE,
};

use crate::tty::ChildEvent;

struct Interest {
    poller: Arc<Poller>,
    event: Event,
}

struct ChildExitSender {
    sender: mpsc::Sender<ChildEvent>,
    interest: Arc<Mutex<Option<Interest>>>,
    child_handle: AtomicPtr<c_void>,
    fired: Arc<AtomicBool>,
}

/// WinAPI callback to run when child process exits.
extern "system" fn child_exit_callback(ctx: *mut c_void, timed_out: BOOLEAN) {
    if timed_out != 0 {
        return;
    }

    // The watcher keeps this allocation alive until unregister has waited for callbacks.
    let event_tx = unsafe { &*ctx.cast::<ChildExitSender>() };

    let mut exit_code = 0_u32;
    let child_handle = event_tx.child_handle.load(Ordering::Relaxed) as HANDLE;
    let status = unsafe { GetExitCodeProcess(child_handle, &mut exit_code) };
    let exit_status = if status == FALSE {
        None
    } else {
        Some(ExitStatus::from_raw(exit_code))
    };
    event_tx.sender.send(ChildEvent::Exited(exit_status)).ok();
    event_tx.fired.store(true, Ordering::Release);

    let interest = event_tx.interest.lock().unwrap();
    if let Some(interest) = interest.as_ref() {
        interest
            .poller
            .post(CompletionPacket::new(interest.event))
            .ok();
    }
}

pub struct ChildExitWatcher {
    wait_handle: AtomicPtr<c_void>,
    event_rx: mpsc::Receiver<ChildEvent>,
    interest: Arc<Mutex<Option<Interest>>>,
    child_handle: AtomicPtr<c_void>,
    pid: Option<NonZeroU32>,
    context: AtomicPtr<c_void>,
    fired: Arc<AtomicBool>,
    owned_handle: Option<OwnedHandle>,
}

impl ChildExitWatcher {
    pub fn new(child_handle: HANDLE) -> Result<ChildExitWatcher, Error> {
        let (event_tx, event_rx) = mpsc::channel();

        let mut wait_handle: HANDLE = ptr::null_mut();
        let interest = Arc::new(Mutex::new(None));
        let fired = Arc::new(AtomicBool::new(false));
        let sender_ref = Box::new(ChildExitSender {
            sender: event_tx,
            interest: interest.clone(),
            child_handle: AtomicPtr::from(child_handle),
            fired: fired.clone(),
        });
        let context = Box::into_raw(sender_ref);

        let success = unsafe {
            RegisterWaitForSingleObject(
                &mut wait_handle,
                child_handle,
                Some(child_exit_callback),
                context.cast(),
                INFINITE,
                WT_EXECUTEINWAITTHREAD | WT_EXECUTEONLYONCE,
            )
        };

        if success == 0 {
            let error = Error::last_os_error();
            unsafe {
                drop(Box::from_raw(context));
            }
            Err(error)
        } else {
            let pid = unsafe { NonZeroU32::new(GetProcessId(child_handle)) };
            Ok(ChildExitWatcher {
                event_rx,
                interest,
                pid,
                child_handle: AtomicPtr::from(child_handle),
                wait_handle: AtomicPtr::from(wait_handle),
                context: AtomicPtr::from(context.cast()),
                fired,
                owned_handle: None,
            })
        }
    }

    /// Take ownership of a newly created process handle.
    pub fn new_owned(handle: OwnedHandle) -> Result<Self, Error> {
        let raw = handle.as_raw_handle().cast();
        match Self::new(raw) {
            Ok(mut watcher) => {
                watcher.owned_handle = Some(handle);
                Ok(watcher)
            }
            Err(error) => {
                unsafe {
                    TerminateProcess(raw, 1);
                }
                Err(error)
            }
        }
    }

    pub fn event_rx(&self) -> &mpsc::Receiver<ChildEvent> {
        &self.event_rx
    }

    pub fn register(&self, poller: &Arc<Poller>, event: Event) {
        *self.interest.lock().unwrap() = Some(Interest {
            poller: poller.clone(),
            event,
        });
        if self.fired.load(Ordering::Acquire) {
            poller.post(CompletionPacket::new(event)).ok();
        }
    }

    pub fn deregister(&self) {
        *self.interest.lock().unwrap() = None;
    }

    /// Retrieve the process handle of the underlying child process.
    ///
    /// This function does **not** pass ownership of the raw handle to you,
    /// and the handle is only guaranteed to be valid while the hosted application
    /// has not yet been destroyed.
    ///
    /// If you terminate the process using this handle, the terminal will get a
    /// timeout error, and the child watcher will emit an `Exited` event.
    pub fn raw_handle(&self) -> HANDLE {
        self.child_handle.load(Ordering::Relaxed) as HANDLE
    }

    /// Retrieve the Process ID associated to the underlying child process.
    pub fn pid(&self) -> Option<NonZeroU32> {
        self.pid
    }
}

impl Drop for ChildExitWatcher {
    fn drop(&mut self) {
        unsafe {
            if UnregisterWaitEx(
                self.wait_handle.load(Ordering::Relaxed) as HANDLE,
                INVALID_HANDLE_VALUE,
            ) != 0
            {
                drop(Box::from_raw(
                    self.context
                        .load(Ordering::Relaxed)
                        .cast::<ChildExitSender>(),
                ));
            } else {
                // Never release a callback context or owned process while an OS callback may run.
                log::error!(
                    "Could not unregister child wait: {}",
                    Error::last_os_error()
                );
                if let Some(handle) = self.owned_handle.take() {
                    std::mem::forget(handle);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::PTY_CHILD_EVENT_TOKEN;
    use super::*;

    #[test]
    pub fn event_is_emitted_when_child_exits() {
        const WAIT_TIMEOUT: Duration = Duration::from_millis(200);

        let poller = Arc::new(Poller::new().unwrap());

        let mut child = Command::new("cmd.exe").spawn().unwrap();
        let child_exit_watcher = ChildExitWatcher::new(child.as_raw_handle() as HANDLE).unwrap();
        child_exit_watcher.register(&poller, Event::readable(PTY_CHILD_EVENT_TOKEN));

        child.kill().unwrap();

        // Poll for the event or fail with timeout if nothing has been sent.
        let mut events = polling::Events::new();
        poller.wait(&mut events, Some(WAIT_TIMEOUT)).unwrap();
        assert_eq!(events.iter().next().unwrap().key, PTY_CHILD_EVENT_TOKEN);
        // Verify that at least one `ChildEvent::Exited` was received.
        let expected_status = ExitStatus::from_raw(1);
        assert_eq!(
            child_exit_watcher.event_rx().try_recv(),
            Ok(ChildEvent::Exited(Some(expected_status)))
        );
    }
}
