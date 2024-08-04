use std::sync::Arc;

use egui::{vec2, Color32, Ui};
use log::info;

use crate::{widgets::enum_dropdown::enum_dropdown, FrontendLanguage, MaximaEguiApp};

#[derive(Debug, PartialEq)]
enum SettingsViewDemoTheme {
    System,
    Dark,
    Light,
}

pub fn settings_view(app: &mut MaximaEguiApp, ui: &mut Ui) {
    ui.style_mut().spacing.interact_size = vec2(100.0, 30.0);
    ui.style_mut().spacing.icon_width = ui.style().spacing.interact_size.y;
    ui.style_mut().visuals.widgets.hovered.fg_stroke.color = Color32::WHITE;
    ui.heading(&app.locale.localization.settings_view.interface.header);
    ui.separator();
    ui.horizontal(|ui| {
        enum_dropdown(ui, "Settings_LanguageComboBox".to_owned(), &mut app.settings.language, 150.0, &app.locale.localization.settings_view.interface.language, &app.locale);
    });
    if ui.checkbox(&mut app.settings.videos, &app.locale.localization.settings_view.interface.videos).clicked() {
        if app.settings.videos {
            if app.app_bg_media_player.is_none() {
                app.app_bg_media_player = Some(crate::renderers::media_player::Player::new(ui.ctx()));
            }
        } else {
            if app.app_bg_media_player.is_some() {
                app.app_bg_media_player = None;
            }
        }
    }
        
    ui.heading("");
    ui.heading(&app.locale.localization.settings_view.game_installation.header);
    ui.separator();
    ui.label(&app.locale.localization.settings_view.game_installation.default_folder);
    ui.horizontal(|ui| {
        ui.add_sized(vec2(ui.available_width() - (100.0 + ui.spacing().item_spacing.x), 30.0), egui::TextEdit::singleline(&mut app.settings.default_install_folder).vertical_align(egui::Align::Center));
        if ui.add_sized(vec2(100.0, 30.0), egui::Button::new("BROWSE")).clicked() {

        }
    });
}
