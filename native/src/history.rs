use crate::{linux, recovery, sessions, settings_ui};
use anyhow::Result;
use gtk::{glib, prelude::*};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

fn date(id: &str) -> String {
    id.parse::<u128>()
        .ok()
        .and_then(|ns| i64::try_from(ns / 1_000_000_000).ok())
        .and_then(|seconds| glib::DateTime::from_unix_local(seconds).ok())
        .and_then(|date| date.format("%d.%m.%Y  %H:%M:%S").ok())
        .map(|date| date.to_string())
        .unwrap_or_else(|| id.to_owned())
}

fn status(entry: &sessions::Entry) -> &'static str {
    match entry.outcome() {
        "recovered" => "Текст восстановлен",
        "recovery_failed" => "Повторная обработка не удалась",
        "recovery_cancelled" => "Повторная обработка отменена",
        "recovery_started" => "Распознавание не завершено",
        "paste_requested" => "Вставка запрошена",
        "clipboard_only" => "В буфере обмена",
        outcome if outcome.starts_with("cancelled") => {
            if entry.has_result {
                "Отменено · текст сохранён"
            } else {
                "Отменено"
            }
        }
        _ if entry.has_result => "Текст сохранён",
        "failed" => "Ошибка · текст не получен",
        _ if entry.has_audio => "Аудио сохранено",
        _ => "Нет сохранённого текста",
    }
}

struct Job {
    id: String,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
struct View {
    list: gtk::ListBox,
    title: gtk::Label,
    details: gtk::Label,
    text: gtk::TextBuffer,
    path: gtk::Label,
    message: gtk::Label,
    copy: gtk::Button,
    retry: gtk::Button,
    cancel: gtk::Button,
    spinner: gtk::Spinner,
    activity: gtk::Label,
    active: Rc<RefCell<Option<Job>>>,
    entries: Rc<RefCell<Vec<sessions::Entry>>>,
    selected_text: Rc<RefCell<Option<String>>>,
}

impl View {
    fn select(&self, index: Option<usize>) {
        self.copy.set_sensitive(false);
        self.retry.set_sensitive(false);
        self.selected_text.replace(None);
        self.message.set_text("");
        let entries = self.entries.borrow();
        let Some(entry) = index.and_then(|i| entries.get(i)) else {
            self.title.set_text("История диктовок");
            self.details
                .set_text("Здесь появятся ваши диктовки из Velo Native.");
            self.text.set_text("");
            self.path.set_text("");
            return;
        };
        self.title.set_text(&date(&entry.id));
        self.retry
            .set_sensitive(entry.has_audio && !entry.has_result && self.active.borrow().is_none());
        let mut details = vec![status(entry).to_owned()];
        for (key, label) in [("provider", "Сервис"), ("model", "Модель")] {
            if let Some(value) = entry
                .recovery
                .get(key)
                .or_else(|| entry.report.get(key))
                .and_then(|v| v.as_str())
            {
                details.push(format!("{label}: {value}"));
            }
        }
        for (key, label) in [("audio_seconds", "Запись"), ("api_seconds", "API")] {
            if let Some(value) = entry
                .recovery
                .get(key)
                .or_else(|| entry.report.get(key))
                .and_then(|v| v.as_f64())
            {
                details.push(format!("{label}: {value:.2} с"));
            }
        }
        self.details.set_text(&details.join(" · "));
        self.path.set_text(&entry.path.to_string_lossy());
        let mut notes = Vec::new();
        if let Some(issue) = &entry.issue {
            notes.push(issue.clone());
        }
        if entry.has_result {
            match entry.text() {
                Ok(text) => {
                    self.text.set_text(&text);
                    self.copy.set_sensitive(true);
                    self.selected_text.replace(Some(text));
                }
                Err(_) => {
                    self.text.set_text("");
                    notes.push("Не удалось прочитать текст. Файлы сессии сохранены.".into());
                }
            }
        } else {
            self.text.set_text("");
            notes.push(if entry.has_audio {
                "Аудио сохранено. Кнопка «Распознать снова» отправит его в API; запрос платный."
                    .into()
            } else {
                "В этой сессии нет сохранённого текста или аудио.".into()
            });
        }
        let error = entry.recovery["error"].as_str().or_else(|| {
            entry
                .recovery
                .is_null()
                .then(|| entry.report["api_error"].as_str())
                .flatten()
        });
        if let Some(error) = error {
            notes.push(error.to_owned());
        }
        self.message.set_text(&notes.join("\n"));
    }

    fn selected_entry(&self) -> Option<sessions::Entry> {
        let index = self.list.selected_row()?.index() as usize;
        self.entries.borrow().get(index).cloned()
    }

    fn recover(&self, window: &gtk::ApplicationWindow, root: &Path) {
        self.recover_with(window, root, recovery::run);
    }

    fn recover_with(
        &self,
        window: &gtk::ApplicationWindow,
        root: &Path,
        run: impl FnOnce(&Path, &AtomicBool) -> Result<recovery::Outcome> + Send + 'static,
    ) {
        if self.active.borrow().is_some() {
            return;
        }
        let Some(entry) = self
            .selected_entry()
            .filter(|e| e.has_audio && !e.has_result)
        else {
            return;
        };
        let Some(app) = window.application() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.active.replace(Some(Job {
            id: entry.id.clone(),
            cancel: cancel.clone(),
        }));
        self.retry.set_sensitive(false);
        self.cancel.set_visible(true);
        self.cancel.set_sensitive(true);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.activity.set_text("Распознаю сохранённое аудио…");
        self.message.set_text("");
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let result = run(&entry.path, &cancel).map_err(|e| format!("{e:#}"));
            let _ = send.send(result);
        });
        // Keep the process alive long enough to save an in-flight result if the
        // window closes. Closing requests cancellation and prevents new retries.
        let hold = app.hold();
        let view = self.clone();
        let root = root.to_owned();
        let window = window.downgrade();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let _ = &hold;
            let result = match receive.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err("Обработка прервалась. Проверь сохранённые файлы сессии.".into())
                }
            };
            let job = view.active.take();
            view.spinner.stop();
            if window.upgrade().is_some_and(|w| w.is_visible()) {
                view.spinner.set_visible(false);
                view.cancel.set_visible(false);
                view.activity.set_text("");
                view.refresh(&root);
                let same_selection = view
                    .selected_entry()
                    .zip(job.as_ref())
                    .is_some_and(|(entry, job)| entry.id == job.id);
                if same_selection {
                    view.message.set_text(match &result {
                        Ok(recovery::Outcome::Cached) => {
                            "Использован сохранённый текст, без нового запроса."
                        }
                        Ok(recovery::Outcome::Recovered) => {
                            "Текст восстановлен. Его можно скопировать."
                        }
                        Ok(recovery::Outcome::Cancelled) => {
                            "Обработка отменена. Аудио сохранено для следующей попытки."
                        }
                        Err(error) => error,
                    });
                } else {
                    view.activity.set_text(match &result {
                        Ok(recovery::Outcome::Recovered | recovery::Outcome::Cached) => {
                            "Текст выбранной для обработки записи сохранён."
                        }
                        Ok(recovery::Outcome::Cancelled) => "Повторная обработка отменена.",
                        Err(error) => error,
                    });
                }
            }
            glib::ControlFlow::Break
        });
    }

    fn refresh(&self, root: &std::path::Path) {
        let entries = match sessions::list(root, sessions::HISTORY_LIMIT) {
            Ok(entries) => entries,
            Err(_) => {
                self.message
                    .set_text("Не удалось открыть историю. Проверь доступ к папке сессий.");
                return;
            }
        };
        let selected_id = self.list.selected_row().and_then(|row| {
            self.entries
                .borrow()
                .get(row.index() as usize)
                .map(|e| e.id.clone())
        });
        // Unselect before replacing the backing entries so GTK callbacks cannot
        // interpret an old row index against a new set of sessions.
        self.list.unselect_all();
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        for entry in &entries {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 5);
            row.set_margin_top(12);
            row.set_margin_bottom(12);
            row.set_margin_start(14);
            row.set_margin_end(14);
            let title = gtk::Label::new(Some(&date(&entry.id)));
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let summary = gtk::Label::new(Some(status(entry)));
            summary.set_xalign(0.0);
            summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
            summary.set_tooltip_text(Some(status(entry)));
            summary.add_css_class("dim-label");
            row.append(&title);
            row.append(&summary);
            self.list.append(&row);
        }
        let index = selected_id
            .and_then(|id| entries.iter().position(|e| e.id == id))
            .unwrap_or(0);
        self.entries.replace(entries);
        if let Some(row) = self.list.row_at_index(index as i32) {
            self.list.select_row(Some(&row));
        } else {
            self.select(None);
        }
    }
}

fn build(app: &gtk::Application, root: &std::path::Path) -> (gtk::ApplicationWindow, View) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Velo — история")
        .default_width(920)
        .default_height(600)
        .build();
    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Velo · История"));
    title.add_css_class("heading");
    header.set_title_widget(Some(&title));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Обновить историю"));
    header.pack_end(&refresh);
    let preferences = gtk::Button::from_icon_name("preferences-system-symbolic");
    preferences.set_tooltip_text(Some("Настройки Velo"));
    header.pack_start(&preferences);
    let settings_window = window.downgrade();
    preferences.connect_clicked(move |_| {
        if let Some(window) = settings_window.upgrade()
            && let Some(app) = window.application()
            && let Err(error) = settings_ui::present(&app, Some(&window))
        {
            eprintln!("Velo settings: {error}");
        }
    });
    window.set_titlebar(Some(&header));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_show_separators(true);
    list.add_css_class("navigation-sidebar");
    let placeholder = gtk::Label::new(Some("Пока нет диктовок"));
    placeholder.set_margin_top(24);
    placeholder.add_css_class("dim-label");
    list.set_placeholder(Some(&placeholder));
    let sidebar = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(250)
        .child(&list)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(24);
    content.set_margin_bottom(18);
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.add_css_class("title-2");
    let details = gtk::Label::new(None);
    details.set_xalign(0.0);
    details.set_wrap(true);
    details.add_css_class("dim-label");
    let text_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(14)
        .right_margin(14)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let text_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .has_frame(true)
        .child(&text_view)
        .build();
    let message = gtk::Label::new(None);
    message.set_xalign(0.0);
    message.set_wrap(true);
    message.set_selectable(true);
    let path = gtk::Label::new(None);
    path.set_xalign(0.0);
    path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path.set_selectable(true);
    path.add_css_class("dim-label");
    let copy = gtk::Button::with_label("Копировать текст");
    copy.set_halign(gtk::Align::Start);
    copy.add_css_class("suggested-action");
    copy.set_sensitive(false);
    let retry = gtk::Button::with_label("Распознать снова");
    retry.set_tooltip_text(Some("Отправить сохранённое аудио в API. Запрос платный."));
    retry.set_sensitive(false);
    let cancel = gtk::Button::with_label("Отменить");
    cancel.set_visible(false);
    cancel.set_tooltip_text(Some("Остановить следующие попытки. Уже отправленный запрос может завершиться и тарифицироваться."));
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    actions.append(&copy);
    actions.append(&retry);
    actions.append(&cancel);
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    let activity = gtk::Label::new(None);
    activity.set_xalign(0.0);
    activity.set_wrap(true);
    let progress = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    progress.append(&spinner);
    progress.append(&activity);
    let footer = gtk::Label::new(Some(
        "Velo Native · последние 100 диктовок · обновление вручную",
    ));
    footer.set_xalign(0.0);
    footer.set_wrap(true);
    footer.add_css_class("dim-label");
    content.append(&title);
    content.append(&details);
    content.append(&text_scroll);
    content.append(&message);
    content.append(&actions);
    content.append(&progress);
    content.append(&path);
    content.append(&footer);
    let panes = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&sidebar)
        .end_child(&content)
        .position(270)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .resize_start_child(false)
        .build();
    window.set_child(Some(&panes));

    let view = View {
        list: list.clone(),
        title,
        details,
        text: text_view.buffer(),
        path,
        message,
        copy: copy.clone(),
        retry: retry.clone(),
        cancel: cancel.clone(),
        spinner,
        activity,
        active: Rc::new(RefCell::new(None)),
        entries: Rc::new(RefCell::new(Vec::new())),
        selected_text: Rc::new(RefCell::new(None)),
    };
    let selected = view.clone();
    let selected_handler = list.connect_row_selected(move |_, row| {
        selected.select(row.map(|row| row.index() as usize));
    });
    let copied = view.clone();
    let copy_handler = copy.connect_clicked(move |_| {
        if let Some(text) = copied.selected_text.borrow().as_ref() {
            copied.message.set_text(if linux::copy_text(text).is_ok() {
                "Текст скопирован"
            } else {
                "Не удалось скопировать текст. Проверь wl-copy и Wayland-сессию."
            });
        }
    });
    let refreshed = view.clone();
    let refresh_root = root.to_owned();
    let refresh_handler = refresh.connect_clicked(move |_| refreshed.refresh(&refresh_root));
    let recovering = view.clone();
    let recovery_root = root.to_owned();
    let recovery_window = window.downgrade();
    let retry_handler = retry.connect_clicked(move |_| {
        if let Some(window) = recovery_window.upgrade() {
            recovering.recover(&window, &recovery_root);
        }
    });
    let cancelling = view.clone();
    let cancel_handler = cancel.connect_clicked(move |_| {
        if let Some(job) = cancelling.active.borrow().as_ref() {
            job.cancel.store(true, Ordering::Relaxed);
            cancelling.cancel.set_sensitive(false);
            cancelling
                .activity
                .set_text("Отмена запрошена. Ожидаю завершения текущего запроса…");
        }
    });
    let closing = view.active.clone();
    window.connect_close_request(move |_| {
        if let Some(job) = closing.borrow().as_ref() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        glib::Propagation::Proceed
    });
    view.refresh(root);
    // Release callback captures when closing the standalone history window.
    // In particular, do not retain ListBox -> handler -> View -> ListBox.
    let handlers = RefCell::new(Some((
        selected_handler,
        copy_handler,
        refresh_handler,
        retry_handler,
        cancel_handler,
    )));
    window.connect_destroy(move |_| {
        if let Some((selected, copied, refreshed, retried, cancelled)) = handlers.take() {
            list.disconnect(selected);
            copy.disconnect(copied);
            refresh.disconnect(refreshed);
            retry.disconnect(retried);
            cancel.disconnect(cancelled);
        }
    });
    window.present();
    (window, view)
}

pub fn run() -> Result<()> {
    let root: PathBuf = sessions::root()?;
    let app = gtk::Application::builder()
        .application_id("org.frozelo.Velo.History")
        .build();
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else {
            build(app, &root);
        }
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api, audio};
    use std::{
        fs,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pump_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let context = glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
            if ready() {
                return;
            }
            assert!(Instant::now() < deadline, "GUI operation timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "needs a graphical session; uses only simulated API responses"]
    fn history_recovery_stays_responsive_and_preserves_data_on_cancel_and_close() {
        gtk::init().unwrap();
        let app = gtk::Application::builder()
            .application_id("org.frozelo.Velo.HistoryTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let root = Fixture(std::env::temp_dir().join(format!(
                "velo-history-worker-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        for id in ["1", "2", "3"] {
            let path = root.0.join(id);
            fs::create_dir_all(&path).unwrap();
            sessions::private_write(&path.join("audio.wav"), &audio::wav(&vec![0; 32000])).unwrap();
        }
        let (window, view) = build(&app, &root.0);
        assert!(view.retry.is_sensitive());
        view.recover_with(&window, &root.0, |path, cancel| {
            recovery::run_with(path, cancel, |_, _, _| {
                thread::sleep(Duration::from_millis(400));
                Ok(api::Transcript {
                    text: "Текст восстановлен.".into(),
                    usage: serde_json::json!({}),
                    provider: "plusvibe",
                    model: "test".into(),
                })
            })
        });
        assert!(!view.retry.is_sensitive());
        assert!(view.spinner.is_spinning());
        view.list.select_row(view.list.row_at_index(1).as_ref());
        assert_eq!(view.selected_entry().unwrap().id, "2");
        assert!(!view.retry.is_sensitive());
        // A second click cannot start a concurrent paid job in this window.
        view.recover_with(&window, &root.0, |_, _| panic!("concurrent job started"));
        pump_until(|| view.active.borrow().is_none());
        assert_eq!(view.selected_entry().unwrap().id, "2");
        assert!(view.retry.is_sensitive());
        assert_eq!(
            sessions::saved_text(&root.0.join("3")).unwrap(),
            "Текст восстановлен."
        );
        view.list.select_row(view.list.row_at_index(0).as_ref());
        assert!(view.copy.is_sensitive());
        assert!(!view.retry.is_sensitive());
        assert!(view.details.text().contains("Текст восстановлен"));
        assert_eq!(
            view.selected_text.borrow().as_deref(),
            Some("Текст восстановлен.")
        );

        view.list.select_row(view.list.row_at_index(1).as_ref());
        view.recover_with(&window, &root.0, |path, cancel| {
            recovery::run_with(path, cancel, |_, _, _| anyhow::bail!("test failure"))
        });
        pump_until(|| view.active.borrow().is_none());
        assert!(view.retry.is_sensitive());
        assert!(!view.copy.is_sensitive());
        assert!(view.message.text().contains("test failure"));
        assert!(root.0.join("2/audio.wav").exists());

        // Cancel only after the simulated request started; the response is an
        // error here, so the recording must remain available for another try.
        let started = Arc::new(AtomicBool::new(false));
        let working = started.clone();
        view.recover_with(&window, &root.0, move |path, cancel| {
            recovery::run_with(path, cancel, |_, cancel, _| {
                working.store(true, Ordering::Relaxed);
                while !cancel.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
                anyhow::bail!("cancelled in flight")
            })
        });
        pump_until(|| started.load(Ordering::Relaxed));
        view.cancel.emit_clicked();
        pump_until(|| view.active.borrow().is_none());
        assert_eq!(
            view.selected_entry().unwrap().outcome(),
            "recovery_cancelled"
        );
        assert!(view.details.text().contains("отменена"));
        assert!(root.0.join("2/audio.wav").exists());

        // Closing while a successful response is in flight still saves its text.
        started.store(false, Ordering::Relaxed);
        let working = started.clone();
        view.list.select_row(view.list.row_at_index(2).as_ref());
        view.recover_with(&window, &root.0, move |path, cancel| {
            recovery::run_with(path, cancel, |_, cancel, _| {
                working.store(true, Ordering::Relaxed);
                while !cancel.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(api::Transcript {
                    text: "Saved after close".into(),
                    usage: serde_json::json!({}),
                    provider: "plusvibe",
                    model: "test".into(),
                })
            })
        });
        pump_until(|| started.load(Ordering::Relaxed));
        window.close();
        pump_until(|| view.active.borrow().is_none());
        assert_eq!(
            sessions::saved_text(&root.0.join("1")).unwrap(),
            "Saved after close"
        );
        assert!(!root.0.join("1/audio.wav").exists());
        assert!(!view.spinner.is_spinning());
    }
}
