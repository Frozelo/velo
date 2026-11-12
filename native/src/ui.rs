use crate::{Event, Phase};
use gtk::{glib, prelude::*};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
    },
    time::{Duration, Instant},
};

pub fn run(
    receiver: Receiver<Event>,
    commands: Sender<Event>,
    _stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    demo: bool,
) {
    let app = gtk::Application::builder()
        .application_id("org.frozelo.Velo.Native")
        .build();
    let receiver = Rc::new(receiver);
    app.connect_activate(move |app| {
        let window = gtk::ApplicationWindow::builder().application(app).title("Velo Native")
            .decorated(false).resizable(false).build();
        window.add_css_class("velo-native");
        let css = gtk::CssProvider::new();
        css.load_from_data("window.velo-native { background: transparent; } .capsule { background: alpha(#17181b, .72); border-radius: 18px; padding: 3px 5px; } button { background: transparent; border: none; box-shadow: none; color: #8b8c93; min-width: 24px; min-height: 24px; padding: 0; border-radius: 15px; } button:hover { background: #35363b; color: white; }");
        gtk::style_context_add_provider_for_display(&gtk::gdk::Display::default().expect("GTK display"), &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        container.add_css_class("capsule");
        let activity = gtk::DrawingArea::builder().content_width(34).content_height(24).build();
        let phase = Rc::new(Cell::new(Phase::Idle));
        let draw_phase = phase.clone();
        let started = Instant::now();
        activity.set_draw_func(move |_, cr, width, height| {
            let phase = draw_phase.get();
            if phase == Phase::Error { cr.set_source_rgb(0.90, 0.43, 0.39); }
            else { cr.set_source_rgb(0.68, 0.69, 0.72); }
            cr.set_line_width(2.0);
            cr.set_line_cap(gtk::cairo::LineCap::Round);
            for i in 0..5 {
                let amplitude = match phase {
                    Phase::Recording => 9.0,
                    Phase::Processing => 4.0,
                    _ => 0.0,
                };
                let length = 3.0 + amplitude * (0.5 + 0.5 * (started.elapsed().as_secs_f64() * 5.0 + f64::from(i)).sin());
                let x = f64::from(width) / 2.0 + f64::from(i - 2) * 5.0;
                cr.move_to(x, (f64::from(height) - length) / 2.0);
                cr.line_to(x, (f64::from(height) + length) / 2.0);
                let _ = cr.stroke();
            }
        });
        container.append(&activity);
        let button = gtk::Button::from_icon_name("process-stop-symbolic");
        button.set_tooltip_text(Some("Отменить диктовку"));
        button.set_focusable(false);
        let stop_flag = _stop.clone();
        let cancel_flag = cancel.clone();
        button.connect_clicked(move |_| {
            cancel_flag.store(true, Ordering::Relaxed);
            stop_flag.store(true, Ordering::Relaxed);
        });
        container.append(&button);
        window.set_child(Some(&container));
        // The application stays alive while the capsule is hidden.
        let guard = app.hold();
        window.connect_close_request(move |_| { let _ = &guard; glib::Propagation::Stop });
        let timer_window = window.clone();
        let events = receiver.clone();
        let send = commands.clone();
        let cancel = cancel.clone();
        let mut busy = false;
        let mut hide_at = None;
        let demo_started = Instant::now();
        glib::timeout_add_local(Duration::from_millis(80), move || {
            while let Ok(event) = events.try_recv() {
                match event {
                    Event::Down if !busy && !demo => {
                        busy = true;
                        cancel.store(false, Ordering::Relaxed);
                        phase.set(Phase::Recording);
                        timer_window.set_tooltip_text(None);
                        timer_window.set_visible(true);
                        hide_at = None;
                        let _ = send.send(Event::Start);
                    }
                    Event::Phase(next) => {
                        phase.set(next);
                        if next == Phase::Ready || next == Phase::Error {
                            busy = false;
                            hide_at = Some(Instant::now() + Duration::from_secs(if next == Phase::Error { 8 } else { 2 }));
                        }
                    }
                    Event::Error(message) => {
                        eprintln!("Velo native: {message}");
                        timer_window.set_visible(true);
                        phase.set(Phase::Error);
                        timer_window.set_tooltip_text(Some(&message));
                        busy = false;
                        hide_at = Some(Instant::now() + Duration::from_secs(8));
                    }
                    _ => {}
                }
            }
            if demo {
                phase.set(if demo_started.elapsed().as_secs() % 5 < 3 { Phase::Recording } else { Phase::Processing });
                timer_window.set_visible(true);
            }
            if hide_at.is_some_and(|at| Instant::now() >= at) {
                timer_window.set_visible(false);
                hide_at = None;
            }
            button.set_sensitive(busy && !cancel.load(Ordering::Relaxed));
            if timer_window.is_visible() { activity.queue_draw(); }
            glib::ControlFlow::Continue
        });
    });
    // Do not pass CLI spike commands to GTK's argument parser.
    app.run_with_args::<&str>(&[]);
}
