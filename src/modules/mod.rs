use crate::{Error, Result, TimerInfo};

use log::warn;

/// A decision each module has to take before a timer is executed:
/// Should it be?
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Progress {
    Continue,
    Abort,
    Stop,
}

/// A generic module that controls whether timers should execute or
/// not (outside of the normal timer)
pub trait Module {
    /// Decides if a timer should be allowed to execute
    fn pre_timer(&mut self, _timer: TimerInfo) -> Result<Progress> {
        Ok(Progress::Continue)
    }

    /// Decides what happens after a timer has executed
    fn post_timer(&mut self, _timer: TimerInfo) -> Result<Progress> {
        Ok(Progress::Continue)
    }

    /// Is called when there's a potentially recoverable error. Can
    /// re-throw an unrecoverable error.
    fn warning(&mut self, _error: &Error) -> Result<()> {
        Ok(())
    }

    /// If this is called, the counting was reset - clear any cache
    /// here
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The default module is also the unit type because why not
impl Module for () {
    fn warning(&mut self, error: &Error) -> Result<()> {
        warn!("{} (Debug: {:?})", error, error);
        Ok(())
    }
}

impl Module for Box<dyn Module> {
    fn pre_timer(&mut self, timer: TimerInfo) -> Result<Progress> {
        (&mut **self).pre_timer(timer)
    }
    fn post_timer(&mut self, timer: TimerInfo) -> Result<Progress> {
        (&mut **self).post_timer(timer)
    }
    fn warning(&mut self, error: &Error) -> Result<()> {
        (&mut **self).warning(error)
    }
    fn reset(&mut self) -> Result<()> {
        (&mut **self).reset()
    }
}

/// Combine two timers using the type-system. Can be recursed for a
/// fixed-size amount of timers. Similar to iterator.chain.
impl<A, B> Module for (A, B)
where
    A: Module,
    B: Module,
{
    fn pre_timer(&mut self, timer: TimerInfo) -> Result<Progress> {
        let status = self.0.pre_timer(timer)?;
        if status != Progress::Continue {
            return Ok(status);
        }
        self.1.pre_timer(timer)
    }
    fn post_timer(&mut self, timer: TimerInfo) -> Result<Progress> {
        let status = self.0.post_timer(timer)?;
        if status != Progress::Continue {
            return Ok(status);
        }
        self.1.post_timer(timer)
    }
    fn warning(&mut self, error: &Error) -> Result<()> {
        self.0.warning(error)?;
        self.1.warning(error)
    }
    fn reset(&mut self) -> Result<()> {
        self.0.reset()?;
        self.1.reset()
    }
}

/// Combine multiple modules with a dynamic size
impl<M: Module> Module for Vec<M> {
    fn pre_timer(&mut self, timer: TimerInfo) -> Result<Progress> {
        for module in self {
            let status = module.pre_timer(timer)?;
            if status != Progress::Continue {
                return Ok(status);
            }
        }
        Ok(Progress::Continue)
    }
    fn post_timer(&mut self, timer: TimerInfo) -> Result<Progress> {
        for module in self {
            let status = module.post_timer(timer)?;
            if status != Progress::Continue {
                return Ok(status);
            }
        }
        Ok(Progress::Continue)
    }
    fn warning(&mut self, error: &Error) -> Result<()> {
        for module in self {
            module.warning(error)?;
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        for module in self {
            module.reset()?;
        }
        Ok(())
    }
}

pub mod stop_at;
#[cfg(feature = "pulse")]
pub mod pulse;
pub mod xcb;

pub use self::stop_at::StopAt;
#[cfg(feature = "pulse")]
pub use self::pulse::NotWhenAudio;
pub use self::xcb::Xcb;
