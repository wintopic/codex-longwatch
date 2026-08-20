#![allow(unsafe_code)]

use thiserror::Error;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    },
};

#[derive(Debug, Error)]
pub enum ProcessJobError {
    #[error("failed to create or assign the Windows process job: {0}")]
    Native(String),
}

/// A Windows Job Object that terminates the assigned process tree when dropped.
#[derive(Debug)]
pub struct ProcessJob(HANDLE);

unsafe impl Send for ProcessJob {}

impl ProcessJob {
    /// Create a kill-on-close job and attach an already spawned child process.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows cannot create the job or attach the child.
    pub fn assign(process_id: u32) -> Result<Self, ProcessJobError> {
        let job = unsafe { CreateJobObjectW(None, None) }
            .map_err(|error| ProcessJobError::Native(error.to_string()))?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Err(error) = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } {
            unsafe {
                let _ = CloseHandle(job);
            }
            return Err(ProcessJobError::Native(error.to_string()));
        }

        let process = match unsafe {
            OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, process_id)
        } {
            Ok(process) => process,
            Err(error) => {
                unsafe {
                    let _ = CloseHandle(job);
                }
                return Err(ProcessJobError::Native(error.to_string()));
            }
        };
        let assigned = unsafe { AssignProcessToJobObject(job, process) };
        unsafe {
            let _ = CloseHandle(process);
        }
        if let Err(error) = assigned {
            unsafe {
                let _ = CloseHandle(job);
            }
            return Err(ProcessJobError::Native(error.to_string()));
        }
        Ok(Self(job))
    }
}

impl Drop for ProcessJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
