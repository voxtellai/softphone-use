//! The simple layout: the call, its audio both ways, the recorder keys and the call log.

use chrono::Utc;
use rune::{Cx, El, ScrollSpec, TextStyle, col, el, row, text, text_input};

use super::Ui;
use super::icons;
use super::view::{
    Lanes, Pal, Tint, agent, digits, engraved, frost, hairline, host, icon_button, log_row, main_key, media, mmss, mode_keys, peer, problem, state_line, style,
    wide_key,
};
use crate::call::CallState;

pub fn view(s: &Ui, cx: &mut Cx<Ui>, p: &Pal) -> El {
    let active = s.active();
    let ringing_in = active.is_some_and(|c| c.state == CallState::Ringing && !c.outbound);
    let answered = active.filter(|c| c.state == CallState::Answered);

    let top = match active {
        Some(c) => header(c, p),
        None => dial(s, p),
    };
    // The audio card shows from the first moment of a call; its lanes stay quiet until it connects.
    let lanes = active.map(|c| {
        let (inn, out) = (s.lanes.0.iter().copied().collect::<Vec<_>>(), s.lanes.1.iter().copied().collect::<Vec<_>>());
        media(
            p,
            cx,
            "lanes",
            Lanes {
                out_label: c.mode.unwrap_or(s.mode).label().to_lowercase(),
                in_label: peer(c).to_string(),
                out: &out,
                inn: &inn,
                db: if answered.is_some() { s.live_db } else { (None, None) },
                live: true,
                rtp: None,
                height: 40.0,
            },
        )
    });

    let modes = col().key("modes").w_full().gap(8.0).child(engraved(p, if active.is_some() { "Answer as" } else { "Call as" }).pl(2.0)).child(mode_keys(
        p,
        s.current_mode(),
        false,
    ));

    let left = if ringing_in {
        let id = active.map(|c| c.id.clone()).unwrap_or_default();
        wide_key(p, "reject", "Reject", &icons::CLOSE, false).on_press(move |s: &mut Ui| s.hangup(&id))
    } else {
        wide_key(p, "mute", if s.muted { "Muted" } else { "Mute" }, if s.muted { &icons::MIC_OFF } else { &icons::MIC }, s.muted)
            .on_press(|s: &mut Ui| s.toggle_mute())
    };
    let big = match active {
        None => main_key(p, "call", "Call", &icons::PHONE, Tint::Call).on_press(|s: &mut Ui| s.dial_now()),
        Some(c) if ringing_in => {
            let id = c.id.clone();
            main_key(p, "answer", "Answer", &icons::PHONE, Tint::Call).on_press(move |s: &mut Ui| {
                let m = s.mode;
                s.answer(&id, m)
            })
        }
        Some(c) => {
            let id = c.id.clone();
            main_key(p, "hangup", "Hang up", &icons::PHONE_OFF, Tint::End).on_press(move |s: &mut Ui| s.hangup(&id))
        }
    };
    let prompt = wide_key(p, "prompt", "Prompt", &icons::MESSAGE, s.agent_open).on_press(|s: &mut Ui| s.agent_open = !s.agent_open);
    let keys = row().key("keys").w_full().gap(10.0).center_y().child(left).child(big).child(prompt);

    let body = col().key("body").w_full().px(16.0).pt(16.0).gap(18.0).child(top).children(lanes).child(modes).child(keys);

    let bottom = if s.agent_open {
        col()
            .key("bottom")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .px(16.0)
            .pt(18.0)
            .pb(16.0)
            .child(agent(s, p).scroll(ScrollSpec { stick_bottom: false }).grow(1.0).min_h(0.0))
    } else {
        log(s, p)
    };

    col().key("simple").w_full().h_full().fill(frost(p.bg)).child(titlebar(s, cx, p)).child(hairline(p)).child(body).child(bottom)
}

fn titlebar(s: &Ui, cx: &mut Cx<Ui>, p: &Pal) -> El {
    let st = &s.status;
    let device = st.device.split('@').next().filter(|d| !d.is_empty()).unwrap_or("Softphone").to_string();
    let line = match problem(s) {
        Some(e) => row()
            .gap(5.0)
            .center_y()
            .child(el().size(6.0, 6.0).radius(3.0).fill(p.end))
            .child(text(e).key("problem").font(style(11.0, 500.0)).color(p.end).truncate().min_w(0.0)),
        None => row()
            .gap(5.0)
            .center_y()
            .child(el().size(6.0, 6.0).radius(3.0).fill(p.ok).shadow(0.0, 5.0, p.ok.alpha(0.3)))
            .child(text(format!("Registered on {}", host(&st.proxy))).font(style(11.0, 450.0)).color(p.ink2).truncate().min_w(0.0)),
    };
    let toggle = icon_button(p, "debug-toggle", "Debug view", &icons::ACTIVITY, "⌘D").on_press_tx(|s: &mut Ui, tx| s.toggle_debug(tx));
    row()
        .key("titlebar")
        .w_full()
        .h(56.0)
        .pl(cx.titlebar_inset() + 12.0)
        .pr(14.0)
        .center_y()
        .window_drag()
        .child(el().w(28.0))
        .child(col().grow(1.0).min_w(0.0).gap(1.0).items_center().child(text(device).font(style(13.0, 600.0)).color(p.ink).truncate()).child(line))
        .child(toggle)
}

/// Who is on the line and for how long.
fn header(c: &crate::call::CallSnapshot, p: &Pal) -> El {
    let since = c.answered_at.unwrap_or(c.started_at);
    let secs = (Utc::now() - since).num_seconds();
    let mut sub = state_line(c);
    if !c.codec.is_empty() {
        sub.push_str(&format!(" · {}", c.codec));
    } else if c.state == CallState::Ringing && !c.offer.codecs.is_empty() {
        sub.push_str(&format!(" · offered {}", c.offer.codecs.first().map(|s| s.split('/').next().unwrap_or(s)).unwrap_or_default()));
    }
    let timer_color = if c.state == CallState::Answered { p.ink } else { p.ink3 };
    row()
        .key(format!("header-{}", c.id))
        .w_full()
        .px(4.0)
        .gap(12.0)
        .items_end()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(text(peer(c).to_string()).font(TextStyle { tracking: -0.5, ..style(26.0, 650.0) }).color(p.ink).truncate())
                .child(text(sub).font(style(12.0, 450.0)).color(p.ink2).truncate()),
        )
        .child(text(mmss(secs)).key("timer").font(TextStyle { tracking: -0.6, ..digits(34.0, 600.0) }).color(timer_color))
}

/// The number to call, typed or picked from the log.
fn dial(s: &Ui, p: &Pal) -> El {
    let ready = !s.dial.text.trim().is_empty();
    row()
        .key("dial-block")
        .w_full()
        .h(52.0)
        .px(4.0)
        .gap(10.0)
        .center_y()
        .child(
            text_input(&s.dial, digits(28.0, 600.0))
                .key("dial")
                .label("Extension or number")
                .single_line()
                .placeholder("Extension or number", p.ink3)
                .color(p.ink)
                .caret(p.sig, p.sig.alpha(0.2))
                .grow(1.0)
                .min_w(0.0)
                .on_edit(|s: &mut Ui| &mut s.dial)
                .on_submit(|s: &mut Ui, _| s.dial_now()),
        )
        .children(ready.then(|| text("Press Return to call").font(style(12.0, 450.0)).color(p.ink3)))
}

fn log(s: &Ui, p: &Pal) -> El {
    let empty = s.calls.is_empty().then(|| text("No calls yet.").font(style(13.0, 450.0)).color(p.ink3).px(8.0).py(10.0));
    let list = col().key("log-list").w_full().children(s.calls.iter().enumerate().map(|(i, c)| {
        let r = log_row(p, c);
        if i + 1 < s.calls.len() { col().w_full().child(r).child(el().w_full().h(1.0).fill(p.hair)) } else { r }
    }));
    col()
        .key("log")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .pt(24.0)
        .pb(8.0)
        .gap(2.0)
        .child(engraved(p, "Log").pl(8.0))
        .children(empty)
        .child(list.scroll(ScrollSpec { stick_bottom: false }).grow(1.0).min_h(0.0))
}
