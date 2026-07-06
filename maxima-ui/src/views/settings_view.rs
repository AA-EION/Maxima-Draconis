use egui::{vec2, Ui};

use crate::{widgets::enum_dropdown::enum_dropdown, MaximaEguiApp};

pub fn settings_view(app: &mut MaximaEguiApp, ui: &mut Ui) {
    let localization = &app.locale.localization.settings_view;
    ui.style_mut().spacing.interact_size.y = 30.0;
    ui.style_mut().spacing.icon_width = 30.0;
    ui.heading(&app.locale.localization.settings_view.interface.header);
    ui.separator();
    ui.horizontal(|ui| {
        enum_dropdown(
            ui,
            "Settings_LanguageComboBox".to_owned(),
            &mut app.settings.language,
            150.0,
            &localization.interface.language,
            &app.locale,
        );
    });

    ui.heading("");
    ui.heading(&localization.game_installation.header);
    ui.separator();
    ui.label(&localization.game_installation.default_folder);
    ui.horizontal(|ui| {
        ui.add_sized(
            vec2(
                ui.available_width() - (100.0 + ui.spacing().item_spacing.x),
                30.0,
            ),
            egui::TextEdit::singleline(&mut app.settings.default_install_folder)
                .vertical_align(egui::Align::Center),
        );
        if ui.add_sized(vec2(100.0, 30.0), egui::Button::new("BROWSE")).clicked() {}
    });
    ui.checkbox(
        &mut app.settings.ignore_ood_games,
        &app.locale.localization.settings_view.game_installation.ignore_ood_warning,
    );

    ui.heading("");
    ui.heading(&localization.performance.header);
    ui.separator();
    ui.checkbox(
        &mut app.settings.performance_settings.disable_blur,
        &localization.performance.disable_blur,
    );

    // Wine engine (unix targets only): which wine runs the games. Empty =
    // auto-detect (CrossOver's loader on macOS, umu on Linux). Applied live
    // via MAXIMA_WINE_COMMAND — the same knob the CLI honors.
    #[cfg(unix)]
    {
        ui.heading("");
        ui.heading("Wine engine");
        ui.separator();
        ui.label("Custom wine command — leave empty for auto-detection");
        let response = ui.add_sized(
            vec2(ui.available_width(), 30.0),
            egui::TextEdit::singleline(&mut app.settings.wine_command)
                .hint_text("auto")
                .vertical_align(egui::Align::Center),
        );
        if response.changed() {
            if app.settings.wine_command.is_empty() {
                std::env::remove_var("MAXIMA_WINE_COMMAND");
            } else {
                std::env::set_var("MAXIMA_WINE_COMMAND", &app.settings.wine_command);
            }
        }

        #[cfg(target_os = "macos")]
        {
            let auto = std::path::Path::new(maxima::unix::wine::CROSSOVER_WINE).exists();
            ui.label(format!(
                "Auto-detected engine: {}",
                if auto {
                    "CrossOver"
                } else {
                    "none — install CrossOver or set a custom command above"
                }
            ));
            if let Ok(dir) = maxima::unix::crossover::bottles_dir() {
                ui.label(format!("CrossOver bottles folder: {}", dir.display()));
            }
        }
        #[cfg(target_os = "linux")]
        ui.label("Auto-detected engine: umu (managed by Maxima)");
    }
}
