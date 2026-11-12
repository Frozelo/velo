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
    polish: gtk::CheckButton,
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
            polish: if self.polish.is_sensitive() {
                self.polish.is_active()
            } else {
                saved.polish
            },
        }
    }

    fn changed(&self) {
        let selected = self.selected();
        self.description.set_text(selected.description());
        self.message.set_text(&self.notice);
        self.save.set_sensitive(
            (self.provider.is_sensitive() || self.polish.is_sensitive())
                && self.saved.get() != Some(self.proposed()),
        );
    }

    fn persist(&self) {
        if !self.provider.is_sensitive() && !self.polish.is_sensitive() {
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
    polish_value: Result<Option<String>>,
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
    let polish = gtk::CheckButton::with_label("Расставлять пунктуацию");
    let polish_note = label(
        "Лёгкая редактура через текстовую модель: дополнительный платный запрос, до 8 секунд ожидания. При ошибке используется исходный текст.",
    );
    polish_note.add_css_class("dim-label");
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
    let mut polish_blocked = blocked;
    let mut polish_selected = stored.polish;
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
    match polish_value {
        Ok(Some(value)) => {
            polish_blocked = true;
            match settings::parse_polish(&value) {
                Ok(value) => polish_selected = value,
                Err(_) => messages.push("Некорректный VELO_POLISH: допустимы 0 или 1.".into()),
            }
            messages.push("Редактура задана при запуске через VELO_POLISH. Убери переменную, чтобы менять её здесь.".into());
        }
        Ok(None) => {}
        Err(_) => {
            polish_blocked = true;
            messages.push("Не удалось прочитать VELO_POLISH.".into());
        }
    }
    provider.set_selected(Provider::ALL.iter().position(|p| *p == selected).unwrap() as u32);
    provider.set_sensitive(!blocked);
    polish.set_active(polish_selected);
    polish.set_sensitive(!polish_blocked);
    let form = Form {
        provider: provider.clone(),
        polish: polish.clone(),
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
    content.append(&polish);
    content.append(&polish_note);
    content.append(&note);
    content.append(&message);
    content.append(&save);
    content.append(&location);
    window.set_child(Some(&content));
    let changed = form.clone();
    let changed_handler = provider.connect_selected_notify(move |_| changed.changed());
    let changed = form.clone();
    let polish_handler = polish.connect_toggled(move |_| changed.changed());
    let saving = form.clone();
    let save_handler = save.connect_clicked(move |_| saving.persist());
    let handlers = RefCell::new(Some((changed_handler, save_handler, polish_handler)));
    window.connect_destroy(move |_| {
        if let Some((changed, saved, edited)) = handlers.take() {
            provider.disconnect(changed);
            save.disconnect(saved);
            polish.disconnect(edited);
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
            settings::environment_value("VELO_POLISH"),
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
            build(
                app,
                None,
                &path,
                settings::environment_override(),
                settings::environment_value("VELO_POLISH"),
            );
        }
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// Runs on the existing GTK test thread; no API, keys or user settings involved.
#[cfg(test)]
pub(crate) fn exercise_form(app: &gtk::Application) {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = Fixture(std::env::temp_dir().join(format!(
            "velo-settings-form-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    let path = root.0.join("settings.json");
    let (window, form) = build(app, None, &path, Ok(None), Ok(None));
    assert!(!path.exists(), "Opening settings must not create a config");
    form.provider.set_selected(2);
    assert!(form.save.is_sensitive());
    form.save.emit_clicked();
    assert_eq!(Config::load(&path).unwrap().provider, Provider::PlusVibe);
    assert!(!form.save.is_sensitive());
    window.close();

    let (window, form) = build(app, None, &path, Ok(None), Ok(None));
    assert_eq!(form.selected(), Provider::PlusVibe);
    assert!(!form.save.is_sensitive());
    window.close();
    let (window, form) = build(app, None, &path, Ok(Some("openrouter".into())), Ok(None));
    assert_eq!(form.selected(), Provider::OpenRouter);
    assert!(!form.provider.is_sensitive());
    assert!(!form.save.is_sensitive());
    form.polish.set_active(true);
    form.save.emit_clicked();
    assert!(Config::load(&path).unwrap().polish);
    assert_eq!(Config::load(&path).unwrap().provider, Provider::PlusVibe);
    window.close();

    let (window, form) = build(app, None, &path, Ok(None), Ok(Some("0".into())));
    assert!(!form.polish.is_active());
    assert!(!form.polish.is_sensitive());
    form.provider.set_selected(0);
    form.save.emit_clicked();
    assert!(
        Config::load(&path).unwrap().polish,
        "An environment override must not overwrite the saved choice"
    );
    window.close();

    fs::write(&path, b"{partial").unwrap();
    let (window, form) = build(app, None, &path, Ok(None), Ok(None));
    assert!(!form.save.is_sensitive());
    assert!(form.message.text().contains("JSON"));
    form.save.emit_clicked();
    assert_eq!(fs::read(&path).unwrap(), b"{partial");
    window.close();

    let blocked = root.0.join("blocked");
    let (window, form) = build(
        app,
        None,
        &blocked.join("settings.json"),
        Ok(None),
        Ok(None),
    );
    fs::write(blocked, b"not a directory").unwrap();
    form.provider.set_selected(1);
    form.save.emit_clicked();
    assert!(form.message.text().contains("Не удалось сохранить"));
    assert!(form.save.is_sensitive());
    window.close();
}
