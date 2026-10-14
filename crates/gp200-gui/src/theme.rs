use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, TextStyle};
use gp200_domain::ThemePreference;

#[derive(Clone, Copy)]
pub struct Palette {
    pub canvas: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub ink: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub selected: Color32,
    pub rule: Color32,
    pub warning: Color32,
}
impl Palette {
    pub fn for_context(ctx: &egui::Context) -> Self {
        if ctx.global_style().visuals.dark_mode {
            Self {
                canvas: Color32::from_rgb(28, 41, 51),
                surface: Color32::from_rgb(37, 53, 66),
                raised: Color32::from_rgb(46, 65, 80),
                ink: Color32::from_rgb(237, 243, 245),
                muted: Color32::from_rgb(163, 182, 194),
                accent: Color32::from_rgb(139, 198, 222),
                selected: Color32::from_rgb(48, 83, 106),
                rule: Color32::from_rgb(67, 88, 102),
                warning: Color32::from_rgb(239, 187, 120),
            }
        } else {
            Self {
                canvas: Color32::from_rgb(231, 237, 241),
                surface: Color32::from_rgb(245, 247, 248),
                raised: Color32::WHITE,
                ink: Color32::from_rgb(36, 51, 64),
                muted: Color32::from_rgb(98, 118, 131),
                accent: Color32::from_rgb(55, 105, 131),
                selected: Color32::from_rgb(214, 232, 242),
                rule: Color32::from_rgb(202, 213, 220),
                warning: Color32::from_rgb(148, 99, 25),
            }
        }
    }
}
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "Cantarell".into(),
        FontData::from_static(include_bytes!(
            "../../../assets/ui/fonts/Cantarell-Regular.otf"
        ))
        .into(),
    );
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.insert(0, "Cantarell".into());
    }
    ctx.set_fonts(fonts);
}
pub fn apply(ctx: &egui::Context, preference: ThemePreference, scale: f32) {
    let dark = match preference {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::System => ctx
            .system_theme()
            .is_none_or(|theme| theme == egui::Theme::Dark),
    };
    ctx.set_theme(if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.set_zoom_factor(scale);
    let mut style = (*ctx.global_style()).clone();
    let palette = Palette::for_context(ctx);
    style.visuals.panel_fill = palette.canvas;
    style.visuals.window_fill = palette.surface;
    style.visuals.extreme_bg_color = palette.surface;
    style.visuals.faint_bg_color = palette.raised;
    style.visuals.override_text_color = Some(palette.ink);
    style.visuals.selection.bg_fill = palette.selected;
    style.visuals.selection.stroke = egui::Stroke::new(1.0, palette.accent);
    style.visuals.widgets.inactive.bg_fill = palette.surface;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette.rule);
    style.visuals.widgets.hovered.bg_fill = palette.raised;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette.accent);
    style.visuals.widgets.active.bg_fill = palette.selected;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette.accent);
    style.spacing.item_spacing = egui::vec2(9.0, 9.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.interact_size.y = 29.0;
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(12.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(22.0));
    ctx.set_global_style(style);
}
