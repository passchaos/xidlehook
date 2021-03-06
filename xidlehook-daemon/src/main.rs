#![warn(
    // Harden built-in lints
    missing_copy_implementations,
    missing_debug_implementations,

    // Harden clippy lints
    clippy::cargo_common_metadata,
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro,
    clippy::decimal_literal_representation,
    clippy::float_cmp_const,
    clippy::get_unwrap,
    clippy::integer_arithmetic,
    clippy::integer_division,
    clippy::pedantic,
)]

use std::{fs, rc::Rc, time::Duration};

use async_std::{future, prelude::*, sync, task};
use log::{trace, warn};
use nix::{libc, sys::signal::Signal};
use structopt::StructOpt;
use xidlehook_core::{
    modules::{StopAt, Xcb},
    Module, Xidlehook,
};

mod signal_handler;
mod socket;
mod timers;

use self::timers::CmdTimer;

struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[derive(StructOpt, Debug)]
pub struct Opt {
    /// Print the idle time to standard output. This is similar to xprintidle.
    #[structopt(long)]
    pub print: bool,
    /// Exit after the whole chain of timer commands have been invoked
    /// once
    #[structopt(long, conflicts_with("print"))]
    pub once: bool,
    /// Don't invoke the timer when the current application is
    /// fullscreen. Useful for preventing a lockscreen when watching
    /// videos.
    #[structopt(long, conflicts_with("print"))]
    pub not_when_fullscreen: bool,

    /// The duration is the number of seconds of inactivity which
    /// should trigger this timer.
    ///
    /// The command is what is invoked when the idle duration is
    /// reached. It's passed through \"/bin/sh -c\".
    ///
    /// The canceller is what is invoked when the user becomes active
    /// after the timer has gone off, but before the next timer (if
    /// any). Pass an empty string to not have one.
    #[structopt(long, conflicts_with("print"), required_unless("print"), value_names = &["duration", "command", "canceller"])]
    pub timer: Vec<String>,

    /// Don't invoke the timer when any audio is playing (PulseAudio specific)
    #[cfg(feature = "pulse")]
    #[structopt(long, conflicts_with("print"))]
    pub not_when_audio: bool,

    /// Listen to a unix socket at this address for events.
    /// Each event is one line of JSON data.
    #[structopt(long, conflicts_with("print"))]
    pub socket: Option<String>,
}

fn main() -> xidlehook_core::Result<()> {
    env_logger::init();

    let opt = Opt::from_args();

    let xcb = Rc::new(Xcb::new()?);

    if opt.print {
        let idle = xcb.get_idle()?;
        println!("{}", idle.as_millis());
        return Ok(());
    }

    let mut timers = Vec::new();
    let mut iter = opt.timer.iter().peekable();
    while iter.peek().is_some() {
        // clap-rs will ensure there are always a multiple of 3
        let duration: u64 = match iter.next().unwrap().parse() {
            Ok(duration) => duration,
            Err(err) => {
                eprintln!("error: failed to parse duration as number: {}", err);
                return Ok(());
            },
        };
        timers.push(CmdTimer::from_shell(
            Duration::from_secs(duration),
            iter.next().unwrap().into(),
            iter.next().unwrap().into(),
            String::new(),
        ));
    }

    let mut modules: Vec<Box<dyn Module>> = Vec::new();

    if opt.once {
        modules.push(Box::new(StopAt::completion()));
    }
    if opt.not_when_fullscreen {
        modules.push(Box::new(Rc::clone(&xcb).not_when_fullscreen()));
    }
    #[cfg(feature = "pulse")]
    {
        if opt.not_when_audio {
            modules.push(Box::new(xidlehook_core::modules::NotWhenAudio::new()?))
        }
    }

    let xidlehook = Xidlehook::new(timers).register(modules);
    App {
        opt,
        xcb,
        xidlehook,
    }
    .main_loop()
}

struct App {
    opt: Opt,
    xcb: Rc<Xcb>,
    xidlehook: Xidlehook<CmdTimer, ((), Vec<Box<dyn Module>>)>,
}
impl App {
    fn main_loop(&mut self) -> xidlehook_core::Result<()> {
        let (socket_tx, socket_rx) = sync::channel(4);
        let _scope = if let Some(address) = self.opt.socket.clone() {
            {
                let address = address.clone();
                task::spawn(async move {
                    if let Err(err) = socket::main_loop(&address, socket_tx).await {
                        warn!("Socket handling errored: {}", err);
                    }
                });
            }
            Some(Defer(move || {
                trace!("Removing unix socket {}", address);
                let _ = fs::remove_file(&address);
            }))
        } else {
            None
        };

        let (signal_tx, signal_rx) = sync::channel(1);
        let signal_thread = signal_handler::handle_signals(signal_tx)?;

        let mut socket_rx = Some(socket_rx);
        let mut signal_rx = Some(signal_rx);

        loop {
            enum Selected {
                Socket(Option<(socket::Message, sync::Sender<socket::Reply>)>),
                Signal(Option<Signal>),
                Exit(xidlehook_core::Result<()>),
            }

            let a = async {
                if let Some(ref rx) = socket_rx {
                    Selected::Socket(rx.recv().await)
                } else {
                    future::pending().await
                }
            };
            let b = async {
                if let Some(ref rx) = signal_rx {
                    Selected::Signal(rx.recv().await)
                } else {
                    future::pending().await
                }
            };

            let c = async {
                let status = self.xidlehook.main_async(&self.xcb).await;
                Selected::Exit(status)
            };
            let res = task::block_on(a.race(b).race(c));

            match res {
                Selected::Socket(data) => {
                    if let Some((msg, reply)) = data {
                        trace!("Got command over socket: {:#?}", msg);
                        let response = match self.handle_socket(msg)? {
                            Some(response) => response,
                            None => break,
                        };
                        task::block_on(reply.send(response));
                    } else {
                        socket_rx = None;
                    }
                },
                Selected::Signal(sig) => {
                    if let Some(sig) = sig {
                        trace!("Signal received: {}", sig);
                        break;
                    } else {
                        signal_rx = None;
                    }
                },
                Selected::Exit(res) => {
                    res?;
                    break;
                },
            }
        }

        // Call signal handler to pretend there's a signal - which will
        // cause thread to exit
        signal_handler::handler(Signal::SIGINT as i32 as libc::c_int);

        signal_thread.join().unwrap()?;

        Ok(())
    }
}
