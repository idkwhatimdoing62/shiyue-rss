use eframe::egui;

use crate::gui_theme::ReaderTheme;

pub(super) const WINDOW_TITLE: &str = "拾阅 · RSS 阅读器";
// A wheel notch should move roughly two lines in the reader. egui's native
// default (40 px) feels too short against our 17 px body typography.
const READER_LINE_SCROLL_SPEED: f32 = 64.0;

const JB_MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
const JB_MONO_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf");
const LXGW_WENKAI_REGULAR: &[u8] = include_bytes!("../../assets/fonts/LXGWWenKaiLite-Regular.ttf");
const LXGW_WENKAI_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/LXGWWenKaiLite-Medium.ttf");

pub(super) fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_app_id("rrss-reading-optimized")
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([1120.0, 680.0]),
        ..Default::default()
    }
}

pub(super) fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        ("jb-mono", JB_MONO_REGULAR),
        ("jb-mono-bold", JB_MONO_BOLD),
        ("lxgw-wenkai", LXGW_WENKAI_REGULAR),
        ("lxgw-wenkai-medium", LXGW_WENKAI_MEDIUM),
    ] {
        fonts.font_data.insert(
            name.to_owned(),
            egui::FontData::from_owned(bytes.to_vec()).into(),
        );
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let names = fonts.families.entry(family).or_default();
        names.insert(0, "lxgw-wenkai-medium".to_owned());
        names.insert(0, "jb-mono".to_owned());
    }
    fonts.families.insert(
        egui::FontFamily::Name("cjk-bold".into()),
        vec!["jb-mono-bold".to_owned(), "lxgw-wenkai-medium".to_owned()],
    );
    ctx.set_fonts(fonts);

    ctx.options_mut(|options| {
        options.input_options.line_scroll_speed = READER_LINE_SCROLL_SPEED;
    });

    let theme = ReaderTheme::sspai();
    ctx.all_styles_mut(|style| {
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(17.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(15.5, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(15.0, egui::FontFamily::Monospace),
        );
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.visuals.window_fill = theme.canvas;
        style.visuals.panel_fill = theme.panel;
        style.visuals.extreme_bg_color = theme.code_bg;
        style.visuals.faint_bg_color = theme.code_bg;
        style.visuals.hyperlink_color = theme.link;
        style.visuals.override_text_color = Some(theme.text);
        style.visuals.widgets.noninteractive.bg_stroke.color = theme.border;
        style.visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        style.visuals.widgets.hovered.weak_bg_fill = theme.accent.gamma_multiply(0.08);
        style.visuals.widgets.active.weak_bg_fill = theme.accent.gamma_multiply(0.16);
        // Hover feedback must not expand the widget or add a border: both cause
        // text to move by a pixel and read as a distracting "shake" in dense lists.
        style.visuals.widgets.hovered.expansion = 0.0;
        style.visuals.widgets.active.expansion = 0.0;
        style.visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        style.visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
        style.visuals.selection.bg_fill = theme.accent.gamma_multiply(0.22);
        style.visuals.selection.stroke.color = theme.text;
        for widget in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            widget.corner_radius = egui::CornerRadius::same(5);
        }
        style.interaction.selectable_labels = false;
        style.interaction.multi_widget_text_select = true;
    });
}

#[cfg(test)]
mod tests {
    use super::{READER_LINE_SCROLL_SPEED, install};

    #[test]
    fn reader_scroll_speed_is_large_enough_for_body_typography() {
        let context = eframe::egui::Context::default();
        install(&context);

        let speed = context.options(|options| options.input_options.line_scroll_speed);
        assert_eq!(speed, READER_LINE_SCROLL_SPEED);
        assert!(
            speed >= 56.0,
            "滚轮单次移动仅 {speed}px，正文浏览会显得过短"
        );
    }
}
