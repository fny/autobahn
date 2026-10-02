//! The menu bar app: `autobahn tray`.
//!
//! A thin view over the status report. The icon is the summary — green
//! when every session is synchronized, yellow when any is in conflict, red
//! when any is halted or unreachable, grey when nothing is running — and
//! the menu is the detail: each group, each destination with its state,
//! and for each conflict the three ways to settle it, which run the same
//! `resolve` command a terminal would. Nothing here touches state
//! directly; everything goes through the same functions the CLI uses, so
//! the app cannot disagree with `status` or get a resolution wrong.
//!
//! The report is polled every few seconds. It reads a handful of small
//! files, so polling costs nothing, and it avoids inventing a push
//! protocol for the one client. A *transition* — a session entering
//! conflict, halting, or going unreachable, and a return to synchronized —
//! raises a desktop notification; the icon colour is the steady-state
//! signal.

use std::path::PathBuf;

use anyhow::{Context, Result};
use muda::MenuEvent;
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};

/// What wakes the event loop: a poll timer, or a menu choice.
#[derive(Debug)]
enum Wake {
    Tick,
    Menu(muda::MenuId),
    /// A queued action finished, so the menu and the icon are stale.
    Done,
}

use crate::menubar::{Bar, POLL};

/// Runs the tray until quit.
pub fn run(config: Option<PathBuf>, state_root: PathBuf) -> Result<()> {
    let mut builder = EventLoop::<Wake>::with_user_event();
    // A menu bar app has no dock icon and no windows.
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build().context("unable to create the event loop")?;

    // A windowless loop does not wake itself on a timer reliably, so it
    // is woken explicitly: a thread ticks it for polls, and menu choices
    // are forwarded as they happen rather than found on the next tick.
    let ticker: EventLoopProxy<Wake> = event_loop.create_proxy();
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL);
        if let Err(error) = ticker.send_event(Wake::Tick) {
            eprintln!("tick: {error}");
            return;
        }
    });
    let clicks: EventLoopProxy<Wake> = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = clicks.send_event(Wake::Menu(event.id));
    }));

    run_loop(event_loop, config, state_root)
}

fn run_loop(
    event_loop: EventLoop<Wake>,
    config: Option<PathBuf>,
    state_root: PathBuf,
) -> Result<()> {
    let waker: EventLoopProxy<Wake> = event_loop.create_proxy();
    let mut bar = Bar::start(config, state_root, move || {
        let _ = waker.send_event(Wake::Done);
    })?;
    event_loop
        .run_app(&mut bar)
        .context("the event loop failed")?;
    Ok(())
}

impl winit::application::ApplicationHandler<Wake> for Bar {
    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, wake: Wake) {
        if std::env::var_os("AUTOBAHN_TRAY_DEBUG").is_some() {
            eprintln!("wake: {wake:?}");
        }
        match wake {
            Wake::Tick => self.refresh(),
            Wake::Done => self.finished(),
            Wake::Menu(id) => {
                if self.chose(&id).is_some() {
                    event_loop.exit();
                }
            }
        }
    }

    fn resumed(&mut self, _: &winit::event_loop::ActiveEventLoop) {
        // The item goes in the bar once the loop runs, which is a macOS
        // requirement, and only once.
        self.appear();
    }

    fn window_event(
        &mut self,
        _: &winit::event_loop::ActiveEventLoop,
        _: winit::window::WindowId,
        _: winit::event::WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        // Everything arrives as a user event; between them, sleep.
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}
