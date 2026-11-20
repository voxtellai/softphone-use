//! What both layouts share: the light and dark palettes, the recorder keys, the audio lanes,
//! the call log and the agent prompt.

use chrono::{DateTime, Datelike, Local, Utc};
use rune::shape::LineCap;
use rune::{Color, Cx, El, Family, Icon, Shadow, Shape, Spring, SystemMaterial, TextStyle, canvas, col, el, icon, row, text, text_input};

use super::Ui;
use super::icons;
use crate::ai::VOICES;
use crate::audio::Mode;
use crate::call::{CallSnapshot, CallState};

/// The colours of one appearance. Live media is the signal blue; red is only for ending a call.
pub struct Pal {
    pub dark: bool,
    pub bg: Color,
    pub ink: Color,
    pub ink2: Color,
    pub ink3: Color,
    pub hair: Color,
    pub life: Color,
    pub sig: Color,
    pub sig_bg: Color,
    pub sig_line: Color,
    pub end: Color,
    pub ok: Color,
    pub side: Color,
    pub insp: Color,
    pub card: Color,
    pub sel: Color,
    key_light: Color,
    key_shadow: Color,
    down_shadow: Color,
    pub hovered: Option<String>,
    led_off: Color,
}

impl Pal {
    pub fn new(dark: bool) -> Pal {
        if dark {
            Pal {
                dark: true,
                bg: rgb(0x16181b),
                ink: rgb(0xe9ebee),
                ink2: rgb(0x979ea7),
                ink3: rgb(0x5d646c),
                hair: rgb(0x272b30),
                life: rgb(0x3a3f46),
                sig: rgb(0x6f8fff),
                sig_bg: rgb(0x6f8fff).alpha(0.085),
                sig_line: rgb(0x6f8fff).alpha(0.2),
                end: rgb(0xff5b4f),
                ok: rgb(0x3fd17f),
                side: rgb(0x1b1e21),
                insp: rgb(0x191b1e),
                card: rgb(0x202328),
                sel: rgb(0x6f8fff).alpha(0.16),
                key_light: Color::WHITE.alpha(0.11),
                key_shadow: Color::BLACK.alpha(0.45),
                down_shadow: Color::BLACK.alpha(0.55),
                hovered: None,
                led_off: rgb(0x3b3e43),
            }
        } else {
            Pal {
                dark: false,
                bg: rgb(0xf4f5f3),
                ink: rgb(0x121417),
                ink2: rgb(0x5b6168),
                ink3: rgb(0x9aa0a6),
                hair: rgb(0xe0e3df),
                life: rgb(0xb9beb8),
                sig: rgb(0x1d4dff),
                sig_bg: rgb(0x1d4dff).alpha(0.065),
                sig_line: rgb(0x1d4dff).alpha(0.16),
                end: rgb(0xe5342b),
                ok: rgb(0x1a9c5b),
                side: rgb(0xe8eae6),
                insp: rgb(0xf9faf8),
                card: Color::WHITE,
                sel: rgb(0x1d4dff).alpha(0.1),
                key_light: Color::WHITE,
                key_shadow: rgb(0x14181e).alpha(0.08),
                down_shadow: Color::BLACK.alpha(0.13),
                hovered: None,
                led_off: rgb(0xd2d5cf),
            }
        }
    }
}

/// An sRGB hex colour; Rune's channels are linear light.
pub fn rgb(hex: u32) -> Color {
    let ch = |shift: u32| {
        let c = ((hex >> shift) & 0xff) as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    Color { r: ch(16), g: ch(8), b: ch(0), a: 1.0 }
}

pub fn style(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}

/// Figures that line up: timers, durations, levels.
pub fn digits(size: f32, weight: f32) -> TextStyle {
    TextStyle { features: "'tnum' 1", ..TextStyle::new(size, weight) }
}

pub fn mono(size: f32) -> TextStyle {
    TextStyle { family: Family::SystemMono, ..TextStyle::new(size, 400.0) }
}

/// A label engraved on the body: small, tracked capitals.
pub fn engraved(p: &Pal, label: &str) -> El {
    text(label.to_uppercase()).font(TextStyle { tracking: 1.6, ..style(10.0, 600.0) }).color(p.ink3)
}

pub fn hairline(p: &Pal) -> El {
    el().w_full().h(1.0).fill(p.hair)
}

pub fn view(s: &Ui, cx: &mut Cx<Ui>) -> El {
    let mut p = Pal::new(cx.system_dark());
    p.hovered = s.hovered.clone();
    let page = if s.debug { super::debug::view(s, cx, &p) } else { super::simple::view(s, cx, &p) };
    // The window is transparent: macOS blurs the desktop behind it, and the page's own
    // surfaces tint that, so it reads as frosted glass. Headless renders draw the fill instead.
    el().key("glass").w_full().h_full().fill(p.bg).system_material(SystemMaterial::Popover).child(page)
}

/// A surface colour over the frosted window: mostly opaque, so text keeps its contrast.
pub fn frost(c: Color) -> Color {
    c.alpha(0.9)
}

// Keys: caps with a side wall that sink when engaged, lift a little under the pointer, and
// light their LED when on.

/// What a cap is made of: its face, the hairline around it, the edge under it and its ink.
struct Face {
    top: Color,
    bottom: Color,
    edge: Color,
    lip: Color,
    ink: Color,
    glow: Option<Color>,
}

fn neutral(p: &Pal, down: bool, hover: bool) -> Face {
    match (p.dark, down) {
        (false, false) => Face {
            top: Color::WHITE,
            bottom: if hover { rgb(0xf6f7f4) } else { rgb(0xf0f1ed) },
            edge: Color::BLACK.alpha(0.1),
            lip: Color::BLACK.alpha(0.07),
            ink: rgb(0x4a5057),
            glow: None,
        },
        (false, true) => {
            Face { top: rgb(0xdfe1dc), bottom: rgb(0xe7e9e4), edge: Color::BLACK.alpha(0.17), lip: Color::WHITE.alpha(0.8), ink: rgb(0x121417), glow: None }
        }
        (true, false) => Face {
            top: if hover { rgb(0x3d4046) } else { rgb(0x35383e) },
            bottom: rgb(0x2a2d32),
            edge: Color::BLACK.alpha(0.5),
            lip: rgb(0x0d0e10),
            ink: rgb(0xb7bcc3),
            glow: None,
        },
        (true, true) => Face { top: rgb(0x222529), bottom: rgb(0x2a2d31), edge: Color::BLACK.alpha(0.5), lip: rgb(0x0d0e10), ink: rgb(0xe9ebee), glow: None },
    }
}

#[derive(Clone, Copy)]
pub enum Tint {
    Call,
    End,
}

fn tinted(t: Tint, hover: bool) -> Face {
    let [top, hover_top, bottom, lip] = match t {
        Tint::Call => [0x5479ff, 0x6687ff, 0x2a56f2, 0x1a3cc4],
        Tint::End => [0xf35d55, 0xf8716a, 0xdc382f, 0xa8241c],
    };
    Face {
        top: rgb(if hover { hover_top } else { top }),
        bottom: rgb(bottom),
        edge: rgb(lip).alpha(0.5),
        lip: rgb(lip),
        ink: Color::WHITE,
        glow: Some(rgb(bottom)),
    }
}

/// The cap. Dark keys stand on a 1.5 pt lip with a soft shadow; light keys read by their face
/// and hairline, with a 1 pt underline and only a breath of shadow. Down, a key sinks a point
/// and its light goes inside; hovered, it rises half a point.
fn cap(e: El, p: &Pal, f: &Face, down: bool, hover: bool, radius: f32) -> El {
    let e = e.radius(radius).motion(Spring::new(0.14, 0.0)).fill(f.bottom).gradient(f.top, f.bottom);
    let edge = Shadow { spread: 0.5, color: f.edge, ..Shadow::default() };
    if down {
        return e
            .offset(0.0, 1.0)
            .box_shadow(Shadow { y: 1.0, blur: 2.5, color: p.down_shadow, inset: true, ..Shadow::default() })
            .box_shadow(edge)
            .box_shadow(Shadow { y: 0.5, color: f.lip, ..Shadow::default() });
    }
    let lift = if hover { 0.5 } else { 0.0 };
    // Coloured keys catch less light along their top edge than white ones.
    let light = if f.glow.is_some() { Color::WHITE.alpha(0.2) } else { p.key_light };
    let e = e.offset(0.0, -lift).box_shadow(Shadow { y: 1.0, color: light, inset: true, ..Shadow::default() }).box_shadow(edge);
    match (p.dark, f.glow) {
        (true, _) => e.box_shadow(Shadow { y: 1.5 + lift, color: f.lip, ..Shadow::default() }).shadow(4.0 + lift, 10.0 + 4.0 * lift, p.key_shadow),
        (false, Some(g)) => e.box_shadow(Shadow { y: 1.0 + lift, color: f.lip, ..Shadow::default() }).shadow(3.0 + lift, 8.0 + 4.0 * lift, g.alpha(0.22)),
        (false, None) => e.box_shadow(Shadow { y: 1.0 + lift, color: f.lip, ..Shadow::default() }).shadow(1.5 + lift, 3.0 + 4.0 * lift, p.key_shadow),
    }
}

/// A key: the cap, its accessible button, press feedback and the hover lift.
fn key(p: &Pal, key: &str, label: &str, face: impl Fn(bool) -> Face, down: bool, radius: f32, content: El) -> El {
    let hover = !down && p.hovered.as_deref() == Some(key);
    let id = key.to_string();
    cap(content, p, &face(hover), down, hover, radius)
        .key(key.to_string())
        .role("button")
        .label(label)
        .focusable(true)
        .hit()
        .press_scale(0.985)
        .on_hover_change(move |s: &mut Ui, on: bool, _| {
            if on {
                s.hovered = Some(id.clone());
            } else if s.hovered.as_deref() == Some(id.as_str()) {
                s.hovered = None;
            }
        })
}

fn key_text(label: &str, size: f32, ink: Color) -> El {
    text(label.to_uppercase()).font(TextStyle { tracking: 1.3, ..style(size, 650.0) }).color(ink)
}

pub fn led(p: &Pal, on: bool) -> El {
    let l = el().size(5.0, 5.0).radius(2.5).fill(if on { p.sig } else { p.led_off }).motion(Spring::new(0.2, 0.0));
    if on {
        l.shadow(0.0, 7.0, p.sig.alpha(0.7))
    } else {
        l.box_shadow(Shadow { y: 0.5, blur: 1.0, color: Color::BLACK.alpha(0.25), inset: true, ..Shadow::default() })
    }
}

/// A mode key with its LED. `compact` is the toolbar size.
pub fn mode_key(p: &Pal, m: Mode, on: bool, compact: bool) -> El {
    let ink = neutral(p, on, false).ink;
    let content = if compact {
        row().h(28.0).px(8.0).gap(5.0).center().child(led(p, on)).child(key_text(m.label(), 9.0, ink))
    } else {
        col().h(46.0).w(0.0).grow(1.0).gap(7.0).pt(1.0).center().child(led(p, on)).child(key_text(m.label(), 10.0, ink))
    };
    key(p, &format!("mode-{}", m.as_str()), &format!("Use {}", m.label()), |h| neutral(p, on, h), on, if compact { 7.0 } else { 9.0 }, content)
        .checked(on)
        .on_press(move |s: &mut Ui| s.choose_mode(m))
}

pub fn mode_keys(p: &Pal, active: Mode, compact: bool) -> El {
    let r = row().gap(if compact { 6.0 } else { 8.0 }).center_y();
    let r = if compact { r } else { r.w_full() };
    r.children(Mode::ALL.into_iter().map(|m| mode_key(p, m, m == active, compact)))
}

/// A key with an icon and a label, like Mute and Prompt.
pub fn wide_key(p: &Pal, id: &str, label: &str, glyph: &'static Icon, down: bool) -> El {
    let ink = neutral(p, down, false).ink;
    let content = row().w(104.0).h(46.0).gap(7.0).center().child(icon(glyph, 15.0).color(ink)).child(key_text(label, 10.0, ink));
    key(p, id, label, |h| neutral(p, down, h), down, 9.0, content)
}

/// The key that places, answers or ends a call: as tall as the others, wider, and coloured.
pub fn main_key(p: &Pal, id: &str, label: &str, glyph: &'static Icon, t: Tint) -> El {
    let content = row().h(46.0).grow(1.0).gap(8.0).center().child(icon(glyph, 16.0).color(Color::WHITE)).child(key_text(label, 11.0, Color::WHITE));
    key(p, id, label, |h| tinted(t, h), false, 9.0, content)
}

/// A small toolbar key with a label only.
pub fn small_key(p: &Pal, id: &str, label: &str, down: bool) -> El {
    let ink = neutral(p, down, false).ink;
    key(p, id, label, |h| neutral(p, down, h), down, 7.0, row().h(28.0).px(10.0).center().child(key_text(label, 9.0, ink)))
}

/// A small coloured toolbar key: End call, Answer, Call.
pub fn small_tinted(p: &Pal, id: &str, label: &str, t: Tint) -> El {
    key(p, id, label, |h| tinted(t, h), false, 7.0, row().h(28.0).px(12.0).center().child(key_text(label, 9.0, Color::WHITE)))
}

pub fn icon_button(p: &Pal, key: &str, label: &str, glyph: &'static Icon, shortcut: &str) -> El {
    row()
        .key(key)
        .role("button")
        .label(label)
        .help(format!("{label}  {shortcut}"))
        .focusable(true)
        .hit()
        .size(28.0, 28.0)
        .radius(7.0)
        .center()
        .hover(p.ink.alpha(0.06))
        .child(icon(glyph, 16.0).color(p.ink2))
}

// Audio lanes: what we send (signal blue) and what we hear (ink), each flowing the way it travels.

/// Bars for one lane, oldest first. `live` fades older bars and runs the newest from the sender's side.
pub fn lane(p: &Pal, key: &str, levels: &[f32], out: bool, live: bool, w: f32, h: f32) -> El {
    const PITCH: f32 = 4.0;
    let color = if out { p.sig } else { p.ink };
    let n = ((w / PITCH).floor() as usize).max(1);
    let mid = h / 2.0;
    let mut shape = Shape::new();
    if live {
        let recent: Vec<f32> = levels.iter().rev().take(n).copied().collect();
        for (age, v) in recent.iter().enumerate() {
            let x = if out { 2.0 + age as f32 * PITCH } else { w - 2.0 - age as f32 * PITCH };
            let a = 0.22 + 0.78 * (1.0 - age as f32 / n as f32).powf(1.3);
            shape = bar(shape, x, mid, *v, h, color.alpha(a));
        }
        // The rest of the lane is quiet: a dotted line where no audio has flowed yet.
        for k in recent.len()..n {
            let x = if out { 2.0 + k as f32 * PITCH } else { w - 2.0 - k as f32 * PITCH };
            shape = bar(shape, x, mid, 0.0, h, color.alpha(0.25));
        }
    } else {
        let step = (w / levels.len().max(1) as f32).clamp(3.0, 8.0);
        for (i, v) in levels.iter().enumerate() {
            let x = 2.0 + i as f32 * step;
            if x > w - 2.0 {
                break;
            }
            shape = bar(shape, x, mid, *v, h, color.alpha(0.85));
        }
    }
    canvas(shape).key(key.to_string()).w_full().h(h)
}

fn bar(shape: Shape, x: f32, mid: f32, v: f32, h: f32, c: Color) -> Shape {
    let hh = (v.clamp(0.0, 1.0) * (h - 6.0) / 2.0).max(0.6);
    shape.line([x, mid - hh], [x, mid + hh], 2.2, LineCap::Round, c)
}

pub fn db(v: Option<f64>) -> String {
    v.map_or("–".into(), |d| format!("{} dB", d.round() as i64).replace('-', "−"))
}

/// The blue panel: one lane each way, with the current level in dBFS.
pub struct Lanes<'a> {
    pub out_label: String,
    pub in_label: String,
    pub out: &'a [f32],
    pub inn: &'a [f32],
    pub db: (Option<f64>, Option<f64>),
    pub live: bool,
    pub rtp: Option<String>,
    pub height: f32,
}

pub fn media(p: &Pal, cx: &mut Cx<Ui>, key: &str, l: Lanes) -> El {
    let w = cx.layout_size(&format!("{key}-out")).map_or(360.0, |[w, _]| w);
    let head = |dir: &str, who: String, v: String, c: Color| {
        row()
            .w_full()
            .center_y()
            .child(text(dir.to_string()).font(style(11.0, 600.0)).color(p.ink))
            .child(text(format!(" {who}")).font(style(11.0, 450.0)).color(p.ink2))
            .child(el().grow(1.0))
            .child(text(v).font(TextStyle { family: Family::SystemMono, ..digits(11.0, 450.0) }).color(c))
    };
    col()
        .key(key.to_string())
        .w_full()
        .px(14.0)
        .py(12.0)
        .gap(6.0)
        .radius(14.0)
        .fill(p.sig_bg)
        .border(1.0, p.sig_line)
        .children(l.rtp.map(|r| text(r).font(mono(10.5)).color(p.sig).wrap().pb(2.0)))
        .child(head("out", format!("{} →", l.out_label), if l.live { db(l.db.1) } else { String::new() }, p.sig))
        .child(lane(p, &format!("{key}-out"), l.out, true, l.live, w, l.height))
        .child(head("in", format!("← {}", l.in_label), if l.live { db(l.db.0) } else { String::new() }, p.ink).pt(4.0))
        .child(lane(p, &format!("{key}-in"), l.inn, false, l.live, w, l.height))
}

// The call log.

pub fn peer(c: &CallSnapshot) -> &str {
    if c.outbound { &c.to } else { &c.from }
}

pub fn clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M:%S").to_string()
}

/// A short time for a list: the time today, the weekday this week, else the date.
pub fn when(t: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    let days = (Local::now().date_naive() - local.date_naive()).num_days();
    match days {
        0 => local.format("%H:%M").to_string(),
        1..=6 => local.format("%a").to_string(),
        _ if local.year() == Local::now().year() => local.format("%b %-d").to_string(),
        _ => local.format("%Y-%m-%d").to_string(),
    }
}

pub fn mmss(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn duration(c: &CallSnapshot) -> Option<String> {
    let start = c.answered_at?;
    Some(mmss((c.ended_at.unwrap_or_else(Utc::now) - start).num_seconds()))
}

/// An incoming call nobody answered.
pub fn missed(c: &CallSnapshot) -> bool {
    !c.outbound && c.state == CallState::Ended && c.answered_at.is_none()
}

/// Who is talking for us, in a sentence: "Answered by Agent".
pub fn state_line(c: &CallSnapshot) -> String {
    match (c.state, c.outbound) {
        (CallState::Ringing, false) => "Incoming call".into(),
        (CallState::Ringing, true) => "Calling…".into(),
        (CallState::Answered, out) => {
            let who = c.mode.map_or("–", Mode::label);
            if out { format!("Connected as {who}") } else { format!("Answered by {who}") }
        }
        (CallState::Ended, _) if missed(c) => "Missed".into(),
        (CallState::Ended, _) => format!("Ended · {}", if c.end_reason.is_empty() { "hung up" } else { &c.end_reason }),
    }
}

pub fn direction_icon(p: &Pal, c: &CallSnapshot, live: bool) -> El {
    let (glyph, color) = match (c.outbound, missed(c)) {
        (true, _) => (&icons::ARROW_OUT, p.ink2),
        (false, true) => (&icons::ARROW_IN, p.end),
        (false, false) => (&icons::ARROW_IN, p.ink2),
    };
    icon(glyph, 13.0).color(if live { p.sig } else { color })
}

/// One call: when, which way, who and how long. Pressing it puts the number in the dial field.
pub fn log_row(p: &Pal, c: &CallSnapshot) -> El {
    let live = c.state != CallState::Ended;
    let number = peer(c).to_string();
    let lost = missed(c);
    let trailing = match c.state {
        CallState::Ended => duration(c).unwrap_or_else(|| "–".into()),
        CallState::Ringing => "ringing".into(),
        CallState::Answered => duration(c).unwrap_or_default(),
    };
    let num_color = if live { p.sig } else { p.ink };
    let name = row()
        .grow(1.0)
        .min_w(0.0)
        .gap(8.0)
        .center_y()
        .child(text(number.clone()).font(style(13.0, if live { 600.0 } else { 450.0 })).color(num_color).truncate())
        .children(lost.then(|| text("MISSED").font(TextStyle { tracking: 1.2, ..style(9.0, 650.0) }).color(p.end)));
    let redial = number.clone();
    row()
        .key(format!("log-{}", c.id))
        .role("button")
        .label(format!("Call {number} again"))
        .hit()
        .w_full()
        .h(37.0)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(7.0)
        .hover(p.ink.alpha(0.04))
        .on_press(move |s: &mut Ui| s.redial(&redial))
        .child(text(when(c.started_at)).font(TextStyle { family: Family::SystemMono, ..digits(11.0, 400.0) }).color(p.ink2).w(42.0))
        .child(direction_icon(p, c, live))
        .child(name)
        .child(text(trailing).font(TextStyle { family: Family::SystemMono, ..digits(11.0, if live { 600.0 } else { 400.0 }) }).color(if live {
            p.sig
        } else {
            p.ink2
        }))
}

/// The registration line: where the phone is registered, or what is wrong.
pub fn problem(s: &Ui) -> Option<String> {
    let st = &s.status;
    s.error.clone().or_else(|| (!st.registered).then(|| if st.last_error.is_empty() { "Not registered".into() } else { st.last_error.clone() }))
}

pub fn host(proxy: &str) -> &str {
    proxy.split(':').next().unwrap_or(proxy)
}

/// The agent's prompt and voice, used when a call is answered or placed as Agent.
pub fn agent(s: &Ui, p: &Pal) -> El {
    let voices = row().key("voices").w_full().gap(6.0).center_y().children(VOICES.iter().map(|&v| {
        let on = s.voice == v;
        let ink = neutral(p, on, false).ink;
        let content = row().h(28.0).px(10.0).gap(5.0).center().child(led(p, on)).child(key_text(v, 9.0, ink));
        key(p, &format!("voice-{v}"), v, |h| neutral(p, on, h), on, 7.0, content).role("radio").checked(on).on_press(move |s: &mut Ui| s.voice = v)
    }));
    col()
        .key("agent")
        .w_full()
        .gap(10.0)
        .child(engraved(p, "Agent prompt"))
        .child(
            text_input(&s.instructions, style(13.0, 420.0))
                .key("instructions")
                .label("Agent prompt")
                .placeholder("Empty uses the default: a receptionist answering a business line.", p.ink3)
                .color(p.ink)
                .caret(p.sig, p.sig.alpha(0.2))
                .w_full()
                .min_h(96.0)
                .padding(12.0)
                .radius(10.0)
                .fill(p.card)
                .border(1.0, p.hair)
                .on_edit(|s: &mut Ui| &mut s.instructions),
        )
        .child(engraved(p, "Voice"))
        .child(voices)
}
