//! The debug layout: the calls in a sidebar, the selected call drawn as a SIP ladder with its
//! audio lanes, and an inspector with the facts, per-second levels and transcript.

use chrono::Local;
use rune::shape::LineCap;
use rune::{Color, Cx, El, ScrollSpec, Shape, canvas, col, el, row, text, text_input};

use super::Ui;
use super::glow;
use super::icons;
use super::view::{
    Lanes, Pal, Tint, agent, clock, digits, direction_icon, duration, frost, hairline, host, icon_button, media, missed, mode_keys, mono, peer, problem,
    small_key, small_tinted, state_line, style, when,
};
use crate::call::{CallSnapshot, CallState, Level};
use crate::trace::SipMessage;

const SIDEBAR: f32 = 250.0;
const INSPECTOR: f32 = 320.0;

pub fn view(s: &Ui, cx: &mut Cx<Ui>, p: &Pal) -> El {
    let main = col()
        .key("main")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .child(toolbar(s, p))
        .child(hairline(p))
        .child(row().key("panes").w_full().grow(1.0).min_h(0.0).child(ladder_pane(s, cx, p)).child(inspector(s, cx, p)));
    row().key("debug").w_full().h_full().fill(frost(p.bg)).child(sidebar(s, p)).child(el().w(1.0).h_full().fill(p.hair)).child(main)
}

fn sidebar(s: &Ui, p: &Pal) -> El {
    let st = &s.status;
    let (dot, line) = match problem(s) {
        Some(e) => (p.end, e),
        None => (p.ok, format!("{} · {}", st.proxy, st.transport.to_uppercase())),
    };
    let reg = row().key("registration").w_full().px(10.0).pt(6.0).pb(14.0).gap(9.0).child(el().size(7.0, 7.0).radius(3.5).fill(dot).mt(5.0)).child(
        col()
            .grow(1.0)
            .min_w(0.0)
            .gap(1.0)
            .child(text(if st.device.is_empty() { "Not configured".to_string() } else { st.device.clone() }).font(style(13.0, 600.0)).color(p.ink).truncate())
            .child(text(line).font(style(11.0, 450.0)).color(if dot == p.end { p.end } else { p.ink2 }).wrap()),
    );
    let rows = s.calls.iter().map(|c| {
        let id = c.id.clone();
        let selected = s.selected.as_deref() == Some(c.id.as_str());
        let live = c.state != CallState::Ended;
        let sub = format!(
            "{} · {}",
            if missed(c) {
                "Missed"
            } else if c.outbound {
                "Outgoing"
            } else {
                "Incoming"
            },
            when(c.started_at)
        );
        let trailing = match c.state {
            CallState::Ringing => "ringing".to_string(),
            _ => duration(c).unwrap_or_default(),
        };
        row()
            .key(format!("call-{}", c.id))
            .role("button")
            .label(format!("Show call with {}", peer(c)))
            .checked(selected)
            .hit()
            .w_full()
            .px(10.0)
            .py(8.0)
            .gap(8.0)
            .center_y()
            .radius(8.0)
            .fill(if selected { p.sel } else { Color::TRANSPARENT })
            .hover(if selected { p.sel } else { p.ink.alpha(0.04) })
            .on_press(move |s: &mut Ui| {
                s.selected = Some(id.clone());
                s.open_sip.clear();
                s.refresh();
            })
            .child(direction_icon(p, c, live))
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(1.0)
                    .child(text(peer(c).to_string()).font(style(13.0, 600.0)).color(if missed(c) { p.end } else { p.ink }).truncate())
                    .child(text(sub).font(style(11.0, 450.0)).color(p.ink2).truncate()),
            )
            .child(text(trailing).font(digits(11.0, if live { 600.0 } else { 450.0 })).color(if live { p.sig } else { p.ink2 }))
    });
    col()
        .key("sidebar")
        .w(SIDEBAR)
        .h_full()
        .fill(p.side.alpha(0.55))
        .child(el().w_full().h(52.0).window_drag())
        .child(reg)
        .child(text("Calls").font(style(11.0, 600.0)).color(p.ink2).px(20.0).pb(6.0))
        .child(col().key("call-list").w_full().px(10.0).gap(2.0).children(rows).scroll(ScrollSpec { stick_bottom: false }).grow(1.0).min_h(0.0))
}

fn toolbar(s: &Ui, p: &Pal) -> El {
    let shown = s.selected_call();
    let title = shown.map_or("No call selected".to_string(), |c| format!("{} → {}", c.from, c.to));
    let active = s.active();
    let actions: Vec<El> = match active {
        None => vec![
            row().w(140.0).h(28.0).px(10.0).center_y().radius(7.0).fill(p.card).border(1.0, p.hair).child(
                text_input(&s.dial, digits(13.0, 500.0))
                    .key("dial")
                    .label("Extension or number")
                    .single_line()
                    .placeholder("Number", p.ink3)
                    .color(p.ink)
                    .caret(p.sig, p.sig.alpha(0.2))
                    .grow(1.0)
                    .min_w(0.0)
                    .on_edit(|s: &mut Ui| &mut s.dial)
                    .on_submit(|s: &mut Ui, _| s.dial_now()),
            ),
            small_tinted(p, "call", "Call", Tint::Call).on_press(|s: &mut Ui| s.dial_now()),
        ],
        Some(c) if c.state == CallState::Ringing && !c.outbound => {
            let (a, b) = (c.id.clone(), c.id.clone());
            vec![
                small_key(p, "reject", "Reject", false).on_press(move |s: &mut Ui| s.hangup(&b)),
                small_tinted(p, "answer", "Answer", Tint::Call).on_press(move |s: &mut Ui| {
                    let m = s.mode;
                    s.answer(&a, m)
                }),
            ]
        }
        Some(c) => {
            let id = c.id.clone();
            vec![small_tinted(p, "hangup", "End call", Tint::End).on_press(move |s: &mut Ui| s.hangup(&id))]
        }
    };
    row()
        .key("toolbar")
        .w_full()
        .h(52.0)
        .pl(22.0)
        .pr(14.0)
        .gap(10.0)
        .center_y()
        .window_drag()
        .child(text(title).font(style(14.0, 600.0)).color(p.ink).truncate().min_w(0.0))
        .child(el().grow(1.0))
        .child(mode_keys(p, s.current_mode(), true))
        .child(el().w(1.0).h(20.0).fill(p.hair).mx(2.0))
        .children(active.is_some().then(|| small_key(p, "mute", if s.muted { "Muted" } else { "Mute" }, s.muted).on_press(|s: &mut Ui| s.toggle_mute())))
        .child(small_key(p, "prompt", "Prompt", s.agent_open).on_press(|s: &mut Ui| s.agent_open = !s.agent_open))
        .children(actions)
        .child(icon_button(p, "debug-toggle", "Simple view", &icons::MINIMIZE, "⌘D").on_press_tx(|s: &mut Ui, tx| s.toggle_debug(tx)))
}

// The ladder: lifelines for us, the PBX and the far end; SIP arrows down the page in time.

struct Lines {
    you: f32,
    pbx: f32,
    far: f32,
    w: f32,
}

const ROW: f32 = 28.0;

fn arrow(shape: Shape, from: f32, to: f32, y: f32, width: f32, color: Color, dashed: bool) -> Shape {
    let d = (to - from).signum();
    let (a, b) = (from + d * 3.0, to - d * 3.0);
    let tip = b;
    let base = b - d * 8.0;
    let mut shape = shape;
    if dashed {
        let mut x = a;
        while (base - x) * d > 0.0 {
            let x2 = if ((base - (x + d * 2.0)) * d) > 0.0 { x + d * 2.0 } else { base };
            shape = shape.line([x, y], [x2, y], width, LineCap::Butt, color);
            x += d * 5.0;
        }
    } else {
        shape = shape.line([a, y], [base, y], width, LineCap::Butt, color);
    }
    shape.fill(vec![[tip, y], [base, y - 4.0], [base, y + 4.0]], color)
}

/// "200 OK", "INVITE", plus "sdp" when the message carries one.
fn message_label(m: &SipMessage) -> (String, bool) {
    let first = &m.first_line;
    let name = if let Some(rest) = first.strip_prefix("SIP/2.0 ") { rest.to_string() } else { first.split(' ').next().unwrap_or(first).to_string() };
    (name, !m.sdp.is_empty())
}

fn ladder_pane(s: &Ui, cx: &mut Cx<Ui>, p: &Pal) -> El {
    let Some(c) = s.selected_call() else {
        return col().key("ladder-pane").grow(1.0).h_full().center().child(text("Calls will show up here.").font(style(13.0, 450.0)).color(p.ink3));
    };
    let [w, h] = cx.layout_size("ladder").unwrap_or([520.0, 600.0]);
    let you = 100.0;
    let far = (w - 60.0).max(you + 220.0);
    let l = Lines { you, pbx: you + (far - you) * 0.58, far, w };

    let mut rows: Vec<El> = Vec::new();
    rows.push(heads(c, s, p, &l));
    if !c.outbound {
        let shape = arrow(Shape::new(), l.far, l.pbx, ROW - 6.5, 1.25, p.ink3, true);
        rows.push(
            el().key("implied")
                .w_full()
                .h(ROW)
                .child(canvas(shape).w(l.w).h(ROW).absolute(0.0, 0.0))
                .child(text("INVITE").font(mono(10.5)).color(p.ink3).absolute(l.pbx + 14.0, 3.0)),
        );
    }
    let media_row = |cx: &mut Cx<Ui>| media_row(s, c, cx, p, &l);
    let answered = c.answered_at.is_some();
    let band_after = s.sip.iter().position(|m| m.cseq.ends_with("ACK"));
    let mut band_done = !answered;
    for (i, m) in s.sip.iter().enumerate() {
        rows.push(message_row(i, m, s.open_sip.contains(&i), p, &l));
        if s.open_sip.contains(&i) {
            rows.push(
                row().key(format!("raw-{i}")).w_full().pl(l.you + 22.0).pr(16.0).py(6.0).child(
                    text(m.raw.trim_end().to_string())
                        .font(mono(10.5))
                        .color(p.ink2)
                        .selectable(true)
                        .wrap()
                        .max_w(l.far - l.you + 20.0)
                        .padding(12.0)
                        .radius(8.0)
                        .fill(p.card)
                        .border(1.0, p.hair)
                        .shadow(6.0, 18.0, Color::BLACK.alpha(0.07)),
                ),
            );
        }
        if !band_done && band_after == Some(i) {
            rows.push(media_row(cx));
            band_done = true;
        }
    }
    if !band_done {
        rows.push(media_row(cx));
    }
    let end_line = match c.state {
        CallState::Ended => {
            format!("ended · {} · {}", if c.end_reason.is_empty() { "hung up" } else { &c.end_reason }, c.ended_at.map(clock).unwrap_or_default())
        }
        _ => format!("now · {}", Local::now().format("%H:%M:%S")),
    };
    rows.push(
        el().key("now")
            .w_full()
            .h(40.0)
            .child(canvas(Shape::new().line([8.0, 12.0], [l.far + 30.0, 12.0], 1.0, LineCap::Butt, p.ink)).w(l.w).h(14.0).absolute(0.0, 0.0))
            .child(text(end_line).font(mono(10.5)).color(p.ink2).absolute(8.0, 18.0)),
    );

    let mut lifelines = Shape::new();
    let bottom = (h - 34.0).max(60.0);
    for (x, a) in [(l.you, 1.0), (l.pbx, 1.0), (l.far, 0.5)] {
        let mut y = 48.0;
        while y < bottom {
            lifelines = lifelines.line([x, y], [x, (y + 3.0).min(bottom)], 1.0, LineCap::Butt, p.life.alpha(a));
            y += 7.0;
        }
        lifelines = lifelines.fill(circle([x, 46.0], 3.0), p.ink.alpha(a.max(0.35)));
    }
    let ladder = col().key("ladder").w_full().pb(20.0).child(canvas(lifelines).w(l.w).h(h).absolute(0.0, 0.0)).children(rows);
    col().key("ladder-pane").grow(1.0).min_w(0.0).h_full().child(ladder.scroll(ScrollSpec { stick_bottom: c.state != CallState::Ended }).grow(1.0).min_h(0.0))
}

fn circle(c: [f32; 2], r: f32) -> Vec<[f32; 2]> {
    (0..16)
        .map(|i| {
            let a = i as f32 / 16.0 * std::f32::consts::TAU;
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        })
        .collect()
}

fn heads(c: &CallSnapshot, s: &Ui, p: &Pal, l: &Lines) -> El {
    let head = |key: &str, x: f32, name: String, sub: String, faint: bool| {
        col()
            .key(key.to_string())
            .w(170.0)
            .items_center()
            .gap(1.0)
            .absolute(x - 85.0, 10.0)
            .child(text(name).font(style(13.0, 600.0)).color(if faint { p.ink3 } else { p.ink }).truncate())
            .child(text(sub).font(style(11.0, 450.0)).color(p.ink2).truncate())
    };
    let me = s.status.device.split('@').next().unwrap_or("").to_string();
    let local_ip = c.local_rtp.split(':').next().unwrap_or("").to_string();
    let pbx = s.status.proxy.clone();
    el().key("heads")
        .w_full()
        .h(52.0)
        .child(head("head-you", l.you, "You".into(), if local_ip.is_empty() { me } else { format!("{me} · {local_ip}") }, false))
        .child(head("head-pbx", l.pbx, host(&pbx).to_string(), pbx.clone(), false))
        .child(head("head-far", l.far, peer(c).to_string(), "beyond PBX".into(), true))
}

fn message_row(i: usize, m: &SipMessage, open: bool, p: &Pal, l: &Lines) -> El {
    let inbound = m.direction == "in";
    let (from, to) = if inbound { (l.pbx, l.you) } else { (l.you, l.pbx) };
    let color = if open { p.sig } else { p.ink };
    let shape = arrow(Shape::new(), from, to, ROW - 6.5, if open { 2.0 } else { 1.25 }, color, false);
    let (name, sdp) = message_label(m);
    let t = m.time.with_timezone(&Local).format("%S%.3f").to_string();
    let label = row()
        .gap(6.0)
        .absolute(l.you + 22.0, 3.0)
        .child(text(format!("F{}", i + 1)).font(mono(10.5)).color(if open { p.sig.alpha(0.6) } else { p.ink3 }))
        .child(text(name).font(rune::TextStyle { weight: if open { 650.0 } else { 400.0 }, ..mono(10.5) }).color(color))
        .children(sdp.then(|| text("sdp").font(mono(10.5)).color(if open { p.sig.alpha(0.6) } else { p.ink3 })));
    el().key(format!("sip-{i}"))
        .role("button")
        .label(format!("{} {}", if open { "Hide" } else { "Show" }, m.first_line))
        .checked(open)
        .hit()
        .w_full()
        .h(ROW)
        .radius(6.0)
        .hover(p.ink.alpha(0.03))
        .on_press(move |s: &mut Ui| {
            if !s.open_sip.remove(&i) {
                s.open_sip.insert(i);
            }
        })
        .child(canvas(shape).w(l.w).h(ROW).absolute(0.0, 0.0))
        .child(text(t).font(mono(10.5)).color(if open { p.sig } else { p.ink3 }).absolute(8.0, ROW - 13.5))
        .child(label)
}

fn media_row(s: &Ui, c: &CallSnapshot, cx: &mut Cx<Ui>, p: &Pal, l: &Lines) -> El {
    let live = s.active().is_some_and(|a| a.id == c.id && a.state == CallState::Answered);
    let per_second = |v: &[Level]| v.iter().map(|x| glow(x.dbfs)).collect::<Vec<f32>>();
    let (inn, out) = if live {
        (s.lanes.0.iter().copied().collect::<Vec<_>>(), s.lanes.1.iter().copied().collect::<Vec<_>>())
    } else {
        (per_second(&s.levels.0), per_second(&s.levels.1))
    };
    let lost = c.audio.packets_out.saturating_sub(c.audio.packets_in);
    let rtp = format!(
        "RTP · {} · {} ⇄ {}{}",
        if c.codec.is_empty() { "–" } else { &c.codec },
        if c.local_rtp.is_empty() { "–" } else { &c.local_rtp },
        if c.remote_rtp.is_empty() { "–" } else { &c.remote_rtp },
        if c.audio.packets_in > 0 && lost == 0 { " · 0 lost".to_string() } else { String::new() }
    );
    let panel = media(
        p,
        cx,
        "ladder-lanes",
        Lanes {
            out_label: c.mode.map_or("us", |m| m.label()).to_lowercase(),
            in_label: peer(c).to_string(),
            out: &out,
            inn: &inn,
            db: s.live_db,
            live,
            rtp: Some(rtp),
            height: 44.0,
        },
    );
    row().key("media-row").w_full().pl(l.you - 10.0).py(10.0).child(col().w(l.far - l.you + 20.0).child(panel))
}

// The inspector.

fn section(p: &Pal, t: &str) -> El {
    text(t.to_string()).font(style(11.0, 600.0)).color(p.ink2)
}

fn inspector(s: &Ui, cx: &mut Cx<Ui>, p: &Pal) -> El {
    let mut body = col().key("inspector-body").w_full().px(20.0).py(20.0).gap(12.0);
    if s.agent_open {
        body = body.child(agent(s, p)).child(hairline(p).my(4.0));
    }
    if let Some(c) = s.selected_call() {
        let a = &c.audio;
        let dash = |v: &str| if v.is_empty() { "–".to_string() } else { v.to_string() };
        let offered = if c.offer.codecs.is_empty() { "–".to_string() } else { c.offer.codecs.join(", ") };
        let heard = if a.seconds_in > 0 { format!("{} of {} s", a.seconds_with_sound_in, a.seconds_in) } else { "–".into() };
        let mut state = state_line(c);
        if let Some(d) = duration(c) {
            state.push_str(&format!(" · {d}"));
        }
        let facts = [
            ("State", state),
            ("Call-ID", c.id.clone()),
            ("Offered", offered),
            ("Negotiated", dash(&c.codec)),
            ("Packets", format!("{} in · {} out", a.packets_in, a.packets_out)),
            ("Heard", heard),
        ];
        let mut flags = Vec::new();
        if c.state != CallState::Ringing && a.seconds_in > 3 {
            if a.packets_in == 0 {
                flags.push("No RTP received");
            } else if !a.inbound_audio_heard && a.seconds_digital_silent > 0 {
                flags.push("Inbound is digital silence");
            } else if !a.inbound_audio_heard {
                flags.push("No inbound sound yet");
            }
        }
        body = body
            .child(section(p, "Facts"))
            .children(flags.into_iter().map(|f| text(f).font(style(12.0, 600.0)).color(p.end)))
            .child(col().w_full().gap(7.0).children(facts.into_iter().map(|(k, v)| {
                row()
                    .w_full()
                    .gap(8.0)
                    .child(text(k).font(style(12.0, 450.0)).color(p.ink2).w(92.0))
                    .child(text(v).font(mono(11.0)).color(p.ink).wrap().grow(1.0).min_w(0.0).selectable(true))
            })))
            .child(hairline(p).my(4.0))
            .child(
                row()
                    .gap(12.0)
                    .center_y()
                    .child(section(p, "Levels"))
                    .child(text("— in").font(style(11.0, 500.0)).color(p.ink))
                    .child(text("— out").font(style(11.0, 500.0)).color(p.sig)),
            )
            .child(chart(&s.levels, cx.layout_size("levels").map_or(INSPECTOR - 40.0, |[w, _]| w), p))
            .child(hairline(p).my(4.0))
            .child(section(p, "Transcript"))
            .child(transcript(c, p))
            .child(hairline(p).my(4.0))
            .child(section(p, "Events"))
            .child(events(c, p));
    }
    col()
        .key("inspector")
        .w(INSPECTOR)
        .h_full()
        .fill(p.insp.alpha(0.6))
        .child(el().w(1.0).h_full().fill(p.hair).absolute(0.0, 0.0))
        .child(body.scroll(ScrollSpec { stick_bottom: false }).grow(1.0).min_h(0.0))
}

/// Per-second levels, in and out: -120 dBFS at the bottom, 0 at the top.
fn chart(levels: &(Vec<Level>, Vec<Level>), width: f32, p: &Pal) -> El {
    const H: f32 = 84.0;
    let n = levels.0.len().max(levels.1.len()).max(2);
    let x = |i: usize| i as f32 / (n - 1) as f32 * (width - 2.0) + 1.0;
    let y = |l: &Level| 2.0 + (-(l.dbfs.unwrap_or(-120.0) as f32) / 120.0).clamp(0.0, 1.0) * (H - 4.0);
    let mut shape = Shape::new();
    for f in [0.0f32, 0.5, 1.0] {
        let yy = (2.0 + f * (H - 4.0)).clamp(0.5, H - 0.5);
        shape = shape.stroke(vec![[0.0, yy], [width, yy]], 1.0, p.hair);
    }
    for (series, color) in [(&levels.0, p.ink), (&levels.1, p.sig)] {
        if series.len() >= 2 {
            shape = shape.stroke(series.iter().enumerate().map(|(i, l)| [x(i), y(l)]).collect::<Vec<_>>(), 1.5, color);
        }
    }
    canvas(shape).key("levels").w_full().h(H)
}

/// What the agent said and heard. Only Agent calls have speech; for the rest it says so.
fn transcript(c: &CallSnapshot, p: &Pal) -> El {
    let who = peer(c).to_string();
    let lines: Vec<El> = c
        .events
        .iter()
        .filter_map(|e| {
            let (name, color, t) = if let Some(t) = e.message.strip_prefix("ai said: ") {
                ("Agent".to_string(), p.sig, t)
            } else {
                let t = e.message.strip_prefix("heard: ")?;
                (who.clone(), p.ink2, t)
            };
            Some(
                col()
                    .w_full()
                    .gap(1.0)
                    .child(text(name).font(style(11.0, 600.0)).color(color))
                    .child(text(t.to_string()).font(style(12.5, 450.0)).color(p.ink).wrap().selectable(true)),
            )
        })
        .collect();
    let agent = c.mode == Some(crate::audio::Mode::Ai);
    let note = if !lines.is_empty() {
        (c.state == CallState::Answered && agent).then_some("Listening…")
    } else if agent {
        Some(if c.state == CallState::Answered { "Listening…" } else { "Nothing was said." })
    } else {
        Some("Only calls answered as Agent are transcribed.")
    };
    col().key("transcript").w_full().gap(9.0).children(lines).children(note.map(|n| text(n).font(style(11.5, 450.0)).color(p.ink3)))
}

/// What happened to the call besides speech and SIP: answered, mode switches, agent errors.
fn events(c: &CallSnapshot, p: &Pal) -> El {
    let sip_status = |m: &str| m.split(' ').next().is_some_and(|code| code.len() == 3 && code.chars().all(|ch| ch.is_ascii_digit()));
    let rows = c.events.iter().filter(|e| !e.message.starts_with("ai said: ") && !e.message.starts_with("heard: ") && !sip_status(&e.message)).map(|e| {
        let bad = ["error", "failed", "ai session ended"].iter().any(|w| e.message.contains(w));
        row()
            .w_full()
            .gap(8.0)
            .child(text(clock(e.time)).font(mono(10.5)).color(p.ink3))
            .child(text(e.message.clone()).font(style(11.5, 450.0)).color(if bad { p.end } else { p.ink2 }).wrap().grow(1.0).min_w(0.0).selectable(true))
    });
    col().key("events").w_full().gap(6.0).children(rows)
}
