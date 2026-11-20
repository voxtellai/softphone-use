//! The native window, drawn with Rune: a glass composer to dial, answer and switch modes, the
//! calls, and each call's levels, events and SIP ladder.

mod debug;
mod demo;
mod icons;
mod simple;
mod view;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rune::{AppSpec, Chrome, KeyEvent, TextEdit, Tx, WindowOptions};
use tokio::sync::watch;

use crate::audio::Mode;
use crate::call::{CallSnapshot, CallState, Level};
use crate::phone::{Phone, PhoneStatus};
use crate::trace::SipMessage;

pub struct Ui {
    phone: Option<Arc<Phone>>,
    shutdown: Option<watch::Receiver<bool>>,
    status: PhoneStatus,
    /// Newest first.
    calls: Vec<CallSnapshot>,
    selected: Option<String>,
    levels: (Vec<Level>, Vec<Level>),
    sip: Vec<SipMessage>,
    open_sip: HashSet<usize>,
    dial: TextEdit,
    /// The mode the next call is placed or answered in.
    mode: Mode,
    agent_open: bool,
    instructions: TextEdit,
    voice: &'static str,
    muted: bool,
    /// Live levels of the call in progress, 0 to 1, in and out.
    live: (f32, f32),
    /// The same in dBFS, for the labels.
    live_db: (Option<f64>, Option<f64>),
    /// Recent live levels, oldest first, in and out: the audio lanes.
    lanes: (VecDeque<f32>, VecDeque<f32>),
    error: Option<String>,
    /// The debug layout: every call's facts, levels, events and SIP.
    debug: bool,
    /// The key under the pointer, which lifts.
    hovered: Option<String>,
}

/// Window sizes for the two layouts.
const SIMPLE_SIZE: (f32, f32) = (440.0, 680.0);
const DEBUG_SIZE: (f32, f32) = (1100.0, 760.0);

impl Ui {
    fn empty() -> Ui {
        Ui {
            phone: None,
            shutdown: None,
            status: PhoneStatus::default(),
            calls: Vec::new(),
            selected: None,
            levels: Default::default(),
            sip: Vec::new(),
            open_sip: HashSet::new(),
            dial: TextEdit::new(""),
            mode: Mode::Human,
            agent_open: false,
            instructions: TextEdit::new(""),
            voice: "ara",
            muted: false,
            live: (0.0, 0.0),
            live_db: (None, None),
            lanes: Default::default(),
            error: None,
            debug: false,
            hovered: None,
        }
    }

    /// The call the composer shows: the newest one not yet ended.
    fn active(&self) -> Option<&CallSnapshot> {
        self.calls.iter().find(|c| c.state != CallState::Ended)
    }

    /// The mode on the keys: the answered call's, else the one the next call uses.
    fn current_mode(&self) -> Mode {
        self.active().filter(|c| c.state == CallState::Answered).and_then(|c| c.mode).unwrap_or(self.mode)
    }

    /// Adds the live levels to the lanes while a call carries audio, and clears them after.
    fn push_lanes(&mut self) -> bool {
        const KEEP: usize = 160;
        if self.active().is_some_and(|c| c.state == CallState::Answered) {
            self.lanes.0.push_back(self.live.0);
            self.lanes.1.push_back(self.live.1);
            while self.lanes.0.len() > KEEP {
                self.lanes.0.pop_front();
                self.lanes.1.pop_front();
            }
            true
        } else if !self.lanes.0.is_empty() {
            self.lanes = Default::default();
            true
        } else {
            false
        }
    }

    fn selected_call(&self) -> Option<&CallSnapshot> {
        let id = self.selected.as_ref()?;
        self.calls.iter().find(|c| &c.id == id)
    }

    fn act(&mut self, r: anyhow::Result<()>) {
        self.error = r.err().map(|e| e.to_string());
    }

    fn dial_now(&mut self) {
        let to = self.dial.text.trim().to_string();
        if to.is_empty() {
            return;
        }
        let Some(p) = self.phone.clone() else { return };
        let (mode, instructions, voice) = (self.mode, self.instructions.text.trim().to_string(), self.voice);
        match p.dial(&to, mode, &instructions, voice) {
            Ok(c) => {
                self.selected = Some(c.id.clone());
                self.dial = TextEdit::new("");
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn toggle_debug(&mut self, tx: &mut Tx) {
        self.debug = !self.debug;
        tx.resize_window(if self.debug { DEBUG_SIZE } else { SIMPLE_SIZE });
    }

    /// Puts a number from the call list in the dial field.
    fn redial(&mut self, number: &str) {
        if self.active().is_none() {
            self.dial = TextEdit::new(number);
        }
    }

    fn answer(&mut self, id: &str, mode: Mode) {
        let Some(p) = self.phone.clone() else { return };
        if let Some(c) = p.call(id) {
            p.set_ai(&c, self.instructions.text.trim(), self.voice);
            c.muted.store(self.muted, Ordering::Relaxed);
        }
        self.selected = Some(id.to_string());
        self.act(p.answer(id, mode));
    }

    fn hangup(&mut self, id: &str) {
        if let Some(p) = self.phone.clone() {
            self.act(p.hangup(id, "window"));
        }
    }

    fn choose_mode(&mut self, m: Mode) {
        self.mode = m;
        let answered = self.active().filter(|c| c.state == CallState::Answered).map(|c| c.id.clone());

        if let Some(id) = answered {
            self.answer(&id, m);
        }
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let (Some(p), Some(c)) = (&self.phone, self.active())
            && let Some(call) = p.call(&c.id)
        {
            call.muted.store(self.muted, Ordering::Relaxed);
        }
    }

    /// Copies the phone's state in; true when anything on screen changed.
    fn refresh(&mut self) -> bool {
        let Some(p) = self.phone.clone() else { return false };
        let status = p.status();
        let calls: Vec<CallSnapshot> = p.calls().iter().map(|c| c.snapshot()).collect();
        if self.selected.as_ref().is_none_or(|id| !calls.iter().any(|c| &c.id == id)) {
            self.selected = calls.first().map(|c| c.id.clone());
        }
        let (levels, sip) = match self.selected.as_ref().and_then(|id| p.call(id)) {
            Some(c) => (c.levels(), c.sip(&p.trace)),
            None => Default::default(),
        };
        let live_db = calls
            .iter()
            .find(|c| c.state == CallState::Answered)
            .and_then(|c| p.call(&c.id))
            .map(|c| {
                let g = c.lock();
                (g.live_in, g.live_out)
            })
            .unwrap_or((None, None));
        let live = (glow(live_db.0), glow(live_db.1));
        self.live_db = live_db;
        let changed = status != self.status || calls != self.calls || levels != self.levels || sip != self.sip || live != self.live;
        if sip.len() != self.sip.len() && self.sip.is_empty() {
            self.open_sip.clear();
        }
        (self.status, self.calls, self.levels, self.sip, self.live) = (status, calls, levels, sip, live);
        changed
    }
}

/// A level in dBFS as a bar height from 0 to 1: nothing below -60, full at -12.
fn glow(dbfs: Option<f64>) -> f32 {
    dbfs.map_or(0.0, |d| ((d as f32 + 60.0) / 48.0).clamp(0.0, 1.0))
}

fn poll(s: &mut Ui, _: f64, tx: &mut Tx) -> bool {
    if s.shutdown.as_ref().is_some_and(|r| *r.borrow()) {
        tx.quit();
        return false;
    }
    let changed = s.refresh() | s.push_lanes();
    let busy = s.active().is_some();
    tx.wake_after(Duration::from_millis(if busy { 80 } else { 250 }));
    changed
}

fn on_key(s: &mut Ui, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.is_command('d') {
        s.toggle_debug(tx);
        return true;
    }
    false
}

fn spec() -> AppSpec<Ui> {
    AppSpec {
        min_size: Some((380.0, 480.0)),
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: demo::SCENES,
        ..AppSpec::new("Softphone", SIMPLE_SIZE, Ui::empty, view::view)
    }
}

/// Content under a transparent titlebar, with the traffic lights centred on a 52-point band.
fn app() -> rune::App<Ui> {
    rune::App::new(spec())
        .chrome(Chrome { full_size_content: true, traffic_lights: Some((18.0, 19.0)), drag_regions: true })
        .window_options(WindowOptions { transparent: cfg!(target_os = "macos"), ..WindowOptions::default() })
}

pub fn is_tooling(args: &[String]) -> bool {
    args.iter().any(|a| matches!(a.as_str(), "--snap" | "--tree" | "--script" | "--list-scenes" | "--frames" | "--video"))
}

/// Opens the window on a running phone; returns when it closes.
pub fn run(phone: Arc<Phone>, args: Vec<String>, shutdown: watch::Receiver<bool>) {
    app()
        .init(move || {
            let mut ui = Ui::empty();
            ui.phone = Some(phone);
            ui.shutdown = Some(shutdown);
            ui.refresh();
            ui
        })
        .args(args)
        .run();
}

/// Rune's headless tooling with demo data: `softphone --snap ui.png --scene in-call`.
pub fn run_demo(args: Vec<String>) {
    app().args(args).run();
}
