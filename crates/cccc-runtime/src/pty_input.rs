//! Input serialization is separate from Session lifecycle synchronization.
use crate::RuntimeError;
use crate::cancellation::lock_interruptibly;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// A write failure or cancellation must retain evidence of earlier bytes.
pub(crate) struct WriteProgress {
    pub written: usize,
    pub result: io::Result<bool>,
}

pub(crate) trait PtyInput: Write + Send {
    fn write_all_interruptible(
        &mut self,
        data: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<bool> {
        self.write_all_interruptible_tracked(data, cancelled).result
    }

    fn write_all_interruptible_tracked(
        &mut self,
        mut data: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> WriteProgress {
        let mut written = 0;
        while !data.is_empty() {
            if cancelled() {
                return WriteProgress {
                    written,
                    result: Ok(false),
                };
            }
            match self.write(data) {
                Ok(0) => {
                    return WriteProgress {
                        written,
                        result: Err(io::ErrorKind::WriteZero.into()),
                    };
                }
                Ok(count) => {
                    written += count;
                    data = &data[count..];
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return WriteProgress {
                        written,
                        result: Err(error),
                    };
                }
            }
        }
        WriteProgress {
            written,
            result: self.flush().map(|()| true),
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct NativeInput(pub(crate) Box<dyn Write + Send>);
#[cfg(not(target_os = "linux"))]
impl Write for NativeInput {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
#[cfg(not(target_os = "linux"))]
impl PtyInput for NativeInput {}

pub(crate) type SharedPtyWriter = Arc<Mutex<Box<dyn PtyInput>>>;

/// Actor input, unlike a terminal's protocol responses, retires native turn
/// observations before writing. Kept outside Session/lifecycle synchronization.
pub(crate) struct PreparedInput {
    writer: SharedPtyWriter,
    observer: Option<Arc<crate::devin_pty::Tracker>>,
}

impl PreparedInput {
    pub(crate) fn new(
        writer: SharedPtyWriter,
        observer: Option<Arc<crate::devin_pty::Tracker>>,
    ) -> Self {
        Self { writer, observer }
    }

    fn before(&self, data: &[u8]) {
        if !data.is_empty()
            && let Some(observer) = &self.observer
        {
            observer.invalidate();
        }
    }

    pub(crate) fn write(&self, data: &[u8]) -> Result<(), RuntimeError> {
        self.before(data);
        write_input(&self.writer, data)
    }

    pub(crate) fn write_interruptibly(
        &self,
        data: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, RuntimeError> {
        self.before(data);
        write_input_interruptibly(&self.writer, data, cancelled)
    }

    // Only manager::submission may use this: its input gate and logical epoch
    // cover every payload/key fragment, and uncertain completion invalidates it.
    pub(crate) fn write_submission_part(
        &self,
        data: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<WriteProgress, RuntimeError> {
        write_input_interruptibly_tracked(&self.writer, data, cancelled)
    }
}

pub(crate) fn write_input(writer: &SharedPtyWriter, data: &[u8]) -> Result<(), RuntimeError> {
    let mut writer = writer.lock().map_err(|_| RuntimeError::Poisoned)?;
    writer.write_all(data)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn write_input_interruptibly(
    writer: &SharedPtyWriter,
    data: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, RuntimeError> {
    let Some(mut writer) = lock_interruptibly(writer, cancelled)? else {
        return Ok(false);
    };
    writer
        .write_all_interruptible(data, cancelled)
        .map_err(RuntimeError::from)
}

pub(crate) fn write_input_interruptibly_tracked(
    writer: &SharedPtyWriter,
    data: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<WriteProgress, RuntimeError> {
    let Some(mut writer) = lock_interruptibly(writer, cancelled)? else {
        return Ok(WriteProgress {
            written: 0,
            result: Ok(false),
        });
    };
    Ok(writer.write_all_interruptible_tracked(data, cancelled))
}
