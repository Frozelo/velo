use crate::{linux, sessions};
use anyhow::Result;
use gtk::{glib, prelude::*};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

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

#[derive(Clone)]
struct View {
    list: gtk::ListBox,
    title: gtk::Label,
    details: gtk::Label,
    text: gtk::TextBuffer,
    path: gtk::Label,
    message: gtk::Label,
    copy: gtk::Button,
    entries: Rc<RefCell<Vec<sessions::Entry>>>,
    selected_text: Rc<RefCell<Option<String>>>,
}

impl View {
    fn select(&self, index: Option<usize>) {
        self.copy.set_sensitive(false);
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
        let mut details = vec![status(entry).to_owned()];
        for (key, label) in [("provider", "Сервис"), ("model", "Модель")] {
            if let Some(value) = entry.report.get(key).and_then(|v| v.as_str()) {
                details.push(format!("{label}: {value}"));
            }
        }
        for (key, label) in [("audio_seconds", "Запись"), ("api_seconds", "API")] {
            if let Some(value) = entry.report.get(key).and_then(|v| v.as_f64()) {
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
                "Текста пока нет. Аудио находится в папке сессии; повторное распознавание ещё не добавлено."
                    .into()
            } else {
                "В этой сессии нет сохранённого текста или аудио.".into()
            });
        }
        if let Some(error) = entry.report["api_error"].as_str() {
            notes.push(error.to_owned());
        }
        self.message.set_text(&notes.join("\n"));
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
    content.append(&copy);
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
    view.refresh(root);
    // Release callback captures when closing the standalone history window.
    // In particular, do not retain ListBox -> handler -> View -> ListBox.
    let handlers = RefCell::new(Some((selected_handler, copy_handler, refresh_handler)));
    window.connect_destroy(move |_| {
        if let Some((selected, copied, refreshed)) = handlers.take() {
            list.disconnect(selected);
            copy.disconnect(copied);
            refresh.disconnect(refreshed);
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
