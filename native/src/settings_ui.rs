use crate::settings::{self, Config, Provider};
use anyhow::Result;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Clone)]
struct Form {
    provider: gtk::DropDown,
    description: gtk::Label,
    message: gtk::Label,
    save: gtk::Button,
    saved: Rc<Cell<Option<Config>>>,
    notice: String,
    path: PathBuf,
}

impl Form {
    fn selected(&self) -> Provider {
        Provider::ALL
            .get(self.provider.selected() as usize)
            .copied()
            .unwrap_or_default()
    }

    fn proposed(&self) -> Config {
        let saved = self.saved.get().unwrap_or_default();
        Config {
            provider: if self.provider.is_sensitive() {
                self.selected()
            } else {
                saved.provider
            },
            ..saved
        }
    }

    fn changed(&self) {
        let selected = self.selected();
        self.description.set_text(selected.description());
        self.message.set_text(&self.notice);
        self.save.set_sensitive(
            self.provider.is_sensitive() && self.saved.get() != Some(self.proposed()),
        );
    }

    fn persist(&self) {
        if !self.provider.is_sensitive() {
            return;
        }
        let config = self.proposed();
        match config.save(&self.path) {
            Ok(()) => {
                self.saved.set(Some(config));
                self.save.set_sensitive(false);
                self.message.set_text(&format!(
                    "Сохранено. Настройки действуют для следующих диктовок и восстановлений.\n{}",
                    self.notice
                ));
            }
            Err(_) => self.message.set_text(
                "Не удалось сохранить настройки. Проверь доступ к папке, указанной ниже.",
            ),
        }
    }
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_max_width_chars(55);
    label
}

fn build(
    app: &gtk::Application,
    parent: Option<&gtk::ApplicationWindow>,
    path: &Path,
    override_value: Result<Option<String>>,
) -> (gtk::ApplicationWindow, Form) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Velo — настройки")
        .default_width(500)
        .resizable(false)
        .build();
    window.set_widget_name("velo-settings");
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
        window.set_modal(true);
    }
    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("Настройки Velo"))));
    window.set_titlebar(Some(&header));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    let title = label("Провайдер распознавания");
    title.add_css_class("heading");
    let provider =
        gtk::DropDown::from_strings(&["Автоматически", "Только OpenRouter", "Только PlusVibe"]);
    provider.set_tooltip_text(Some("Провайдер распознавания речи"));
    let description = label("");
    let message = label("");
    message.set_selectable(true);
    let note = label(
        "Настройки действуют для Velo Native. Текущая обработка продолжится с прежним режимом. Настройки Python-версии не меняются.",
    );
    note.add_css_class("dim-label");
    let save = gtk::Button::with_label("Сохранить");
    save.add_css_class("suggested-action");
    save.set_halign(gtk::Align::End);
    let location = gtk::Label::new(Some(&path.to_string_lossy()));
    location.set_xalign(0.0);
    location.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    location.set_selectable(true);
    location.add_css_class("dim-label");
    let loaded = Config::load(path);
    let stored = loaded.as_ref().copied().unwrap_or_default();
    let mut selected = stored.provider;
    let mut blocked = loaded.is_err();
    let mut messages = Vec::new();
    if let Err(error) = loaded {
        messages.push(format!("Не удалось прочитать настройки: {error}. Файл оставлен без изменений. Исправь его и открой окно снова."));
    }
    match override_value {
        Ok(Some(value)) => {
            blocked = true;
            match Provider::parse(&value) {
                Ok(provider) => {
                    selected = provider;
                    messages.push("Режим задан при запуске через VELO_PROVIDER. Убери эту переменную, чтобы менять провайдера здесь.".into());
                }
                Err(_) => messages.push("При запуске задан некорректный VELO_PROVIDER. Допустимые значения: auto, openrouter, plusvibe.".into()),
            }
        }
        Ok(None) => {}
        Err(_) => {
            blocked = true;
            messages.push(
                "Не удалось прочитать VELO_PROVIDER. Убери эту переменную и открой окно снова."
                    .into(),
            );
        }
    }
    provider.set_selected(Provider::ALL.iter().position(|p| *p == selected).unwrap() as u32);
    provider.set_sensitive(!blocked);
    let form = Form {
        provider: provider.clone(),
        description: description.clone(),
        message: message.clone(),
        save: save.clone(),
        saved: Rc::new(Cell::new(path.exists().then_some(stored))),
        notice: messages.join("\n"),
        path: path.to_owned(),
    };
    form.changed();
    message.set_text(&messages.join("\n"));
    content.append(&title);
    content.append(&provider);
    content.append(&description);
    content.append(&note);
    content.append(&message);
    content.append(&save);
    content.append(&location);
    window.set_child(Some(&content));
    let changed = form.clone();
    let changed_handler = provider.connect_selected_notify(move |_| changed.changed());
    let saving = form.clone();
    let save_handler = save.connect_clicked(move |_| saving.persist());
    let handlers = RefCell::new(Some((changed_handler, save_handler)));
    window.connect_destroy(move |_| {
        if let Some((changed, saved)) = handlers.take() {
            provider.disconnect(changed);
            save.disconnect(saved);
        }
    });
    window.present();
    (window, form)
}

pub fn present(app: &gtk::Application, parent: Option<&gtk::ApplicationWindow>) -> Result<()> {
    if let Some(window) = app
        .windows()
        .into_iter()
        .find(|w| w.widget_name() == "velo-settings")
    {
        window.present();
    } else {
        build(
            app,
            parent,
            &settings::path()?,
            settings::environment_override(),
        );
    }
    Ok(())
}

pub fn run() -> Result<()> {
    // Resolve the config location before GTK activation. Opening settings
    // does not start a microphone or read API keys.
    let path = settings::path()?;
    let app = gtk::Application::builder()
        .application_id("org.frozelo.Velo.Settings")
        .build();
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else {
            build(app, None, &path, settings::environment_override());
        }
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}
