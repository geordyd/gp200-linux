use super::*;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-language-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct NoDevice(Arc<AtomicUsize>);
impl DeviceService for NoDevice {
    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SubmitError::Disconnected(command))
    }
    fn try_next_event(&self) -> Option<DeviceEvent> {
        None
    }
    fn shutdown(&self) {}
}
fn app(preferences: AppPreferences, directory: &Directory) -> (StudioApp, Arc<AtomicUsize>) {
    let controller =
        AppController::with_demo(gp200_sim::captured_demo_snapshot(), preferences.clone()).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    (
        StudioApp::with_services(
            controller,
            RuntimeServices::with_preferences_path(
                Box::new(NoDevice(count.clone())),
                no_wake(),
                preferences,
                directory.0.join("preferences.json"),
            ),
        ),
        count,
    )
}
fn draw(
    app: &mut StudioApp,
    context: &egui::Context,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            events,
            ..Default::default()
        },
        |ui| app.render(ui),
    );
    output.textures_delta.clear();
    output
}
fn text(output: &egui::FullOutput) -> Vec<String> {
    fn collect(shape: &egui::epaint::Shape, values: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(text) => values.push(text.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, values);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    for shape in &output.shapes {
        collect(&shape.shape, &mut values);
    }
    values
}
#[test]
fn legacy_chinese_preferences_render_english_settings_without_changing_documents_or_device_authority()
 {
    let directory = Directory::new();
    let preferences: AppPreferences = serde_json::from_value(serde_json::json!({
        "language": "zh-CN", "theme": "Dark", "scale": 1.25, "compact": true,
        "last_input": "hw:GP200,0,0", "last_output": "hw:GP200,0,0"
    }))
    .unwrap();
    assert_eq!(preferences.language, LanguagePreference::English);
    let (mut app, count) = app(preferences.clone(), &directory);
    app.controller
        .dispatch(AppInput::Ui(UiIntent::Navigate(ViewId::AppSettings)));
    let before = app.controller.recovery_session();
    let context = egui::Context::default();
    theme::install_fonts(&context);
    draw(&mut app, &context, vec![]);
    let labels = text(&draw(&mut app, &context, vec![]));
    for expected in [
        "Application settings",
        "System",
        "Light",
        "Dark",
        "UI scale",
        "File dialog folders",
    ] {
        assert!(
            labels.iter().any(|value| value == expected),
            "missing {expected}: {labels:?}"
        );
    }
    assert!(
        !labels
            .iter()
            .any(|value| value == "Language" || value == "English")
    );
    assert_eq!(app.controller.state().preferences, preferences);
    assert_eq!(app.controller.recovery_session(), before);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn migrated_preferences_recover_exact_user_text_undo_and_comparison_into_offline_documents() {
    let directory = Directory::new();
    let mut previous = AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    let document = previous.active_document_id().unwrap().clone();
    previous
        .capture_comparison(&document, ComparisonSlot::A)
        .unwrap();
    previous.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: document.clone(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            name: Some("我的音色".into()),
            notes: Some("Exact user text / {raw}".into()),
            ..Default::default()
        }),
    }));
    previous
        .capture_comparison(&document, ComparisonSlot::B)
        .unwrap();
    let session = previous.recovery_session();
    assert_eq!(session.documents[0].undo.past.len(), 1);
    let store = gp200_library::DraftRecoveryStore::open(directory.0.join("recovery")).unwrap();
    let saved = store.save(&session).unwrap();
    let preferences: AppPreferences =
        serde_json::from_str(r#"{"language":"zh-CN","theme":"Light","scale":1.5}"#).unwrap();
    let (mut app, count) = app(preferences, &directory);
    let retained = store.discover().unwrap().unwrap();
    let ids = app.controller.recover_offline(&retained.session).unwrap();
    assert_eq!(ids.len(), 1);
    let reopened = app.controller.recovery_session();
    let restored = reopened
        .documents
        .iter()
        .find(|d| d.original_id == ids[0])
        .unwrap();
    assert_eq!(restored.draft, session.documents[0].draft);
    assert_eq!(restored.undo, session.documents[0].undo);
    assert_eq!(restored.comparison, session.documents[0].comparison);
    assert!(matches!(restored.original_target, EditTarget::Offline(_)));
    assert_eq!(store.discover().unwrap().unwrap().sha256, saved.sha256);
    assert_eq!(
        app.controller.state().preferences.language,
        LanguagePreference::English
    );
    assert_eq!(
        app.controller.state().preferences.theme,
        ThemePreference::Light
    );
    assert_eq!(app.controller.state().preferences.scale, 1.5);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn english_workspace_and_help_rendering_preserves_documents_and_uses_no_device_commands() {
    let directory = Directory::new();
    let (mut app, count) = app(AppPreferences::default(), &directory);
    let before = app.controller.recovery_session();
    let context = egui::Context::default();
    theme::install_fonts(&context);
    for (view, heading) in [
        (ViewId::Editor, "Capture to library"),
        (ViewId::Library, "Preset library"),
        (ViewId::Assets, "Sound assets"),
        (ViewId::Assignments, "Patch assignments"),
        (ViewId::GlobalSettings, "Device settings"),
        (ViewId::Performance, "Choose a song"),
        (ViewId::Practice, "Audio device"),
        (ViewId::Diagnostics, "Operation diagnostics"),
        (ViewId::AppSettings, "Application settings"),
        (ViewId::Help, "Connect your GP-200"),
    ] {
        app.controller
            .dispatch(AppInput::Ui(UiIntent::Navigate(view)));
        draw(&mut app, &context, vec![]);
        let labels = text(&draw(&mut app, &context, vec![]));
        assert!(
            labels.iter().any(|value| value == "Editor"),
            "missing navigation: {labels:?}"
        );
        assert!(
            labels.iter().any(|value| value == heading),
            "missing {heading}: {labels:?}"
        );
    }
    for (topic, heading) in [
        (HelpTopic::Start, "Connect your GP-200"),
        (HelpTopic::Editing, "Edit, capture and save"),
        (HelpTopic::Backups, "Two kinds of archive"),
        (HelpTopic::Assets, "Impulse responses"),
        (HelpTopic::Troubleshooting, "No pedal or no identity reply"),
        (HelpTopic::About, "Current session"),
    ] {
        app.open_help_topic(topic);
        draw(&mut app, &context, vec![]);
        assert!(
            text(&draw(&mut app, &context, vec![]))
                .iter()
                .any(|value| value == heading)
        );
    }
    assert_eq!(app.controller.recovery_session(), before);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn visible_numeric_and_music_symbols_have_glyphs_in_both_retained_font_families() {
    let context = egui::Context::default();
    theme::install_fonts(&context);
    // Actual catalog notes use A, A#, B, etc. Keep their ASCII sharp notation
    // and the numeric/navigation symbols used by the interface.
    let symbols = "0123456789ABCDEFG.+−-–—/%#°→←·×±∞Ωé“”…";
    let mut missing = vec![];
    let mut output = context.run_ui(egui::RawInput::default(), |ui| {
        ui.ctx().fonts_mut(|fonts| {
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                let font = egui::FontId::new(14.0, family.clone());
                let replacement =
                    fonts.layout_no_wrap("\u{fffd}".into(), font.clone(), egui::Color32::WHITE);
                let replacement_uv = replacement.rows[0].glyphs[0].uv_rect;
                for symbol in symbols.chars() {
                    // epaint 0.36 has_glyph compares the selected face to its
                    // replacement face, returning false even for Hack digits.
                    // Compare the actual rendered glyph against replacement.
                    let rendered = fonts.layout_no_wrap(
                        symbol.to_string(),
                        font.clone(),
                        egui::Color32::WHITE,
                    );
                    if rendered.rows[0].glyphs[0].uv_rect == replacement_uv {
                        missing.push((family.clone(), symbol));
                    }
                }
            }
        });
    });
    output.textures_delta.clear();
    assert!(missing.is_empty(), "missing interface symbols: {missing:?}");
}

#[test]
fn presentation_helpers_keep_names_diagnostics_paths_and_numeric_bits_exact() {
    for data in [
        "Bass",
        "Editor",
        "我的音色",
        "/tmp/My IR {raw}",
        "Unknown backend 0xcafe",
        "Raw bits 0x7fa12345",
    ] {
        assert_eq!(localization::text(data), data);
        assert_eq!(
            localization::text_for(LanguagePreference::English, data),
            data
        );
        assert_eq!(localization::choice(data), data);
        assert_eq!(localization::message(data), data);
        assert_eq!(localization::value(data), data);
        assert_eq!(localization::description("AMP", 0x07000015, data), data);
    }
    for name in ["Rock", "Dark", "Gain", "自定义风格"] {
        let mut body = vec![0; 20];
        body[..2].copy_from_slice(&12_u16.to_le_bytes());
        body[4..4 + name.len()].copy_from_slice(name.as_bytes());
        let raw = RawTransfer::from_records(
            gp200_protocol::WRITE_KIND,
            &[gp200_protocol::RawRecord { command: 10, body }],
        )
        .unwrap();
        assert_eq!(
            localization::style(NativeStyleId::new(12).unwrap(), Some(&raw)),
            name
        );
    }
    assert_eq!(
        localization::style(NativeStyleId::new(12).unwrap(), None),
        "Custom style 1 (name not read)"
    );
    assert_eq!(
        localization::style(NativeStyleId::from_raw(65_535), None),
        "Unknown native style ID 65535"
    );
}

#[test]
fn conversion_notes_and_unknown_external_diagnostics_render_without_rewriting() {
    let directory = Directory::new();
    let (mut app, count) = app(AppPreferences::default(), &directory);
    let report = ConversionReport {
        level: CompatibilityLevel::Partial,
        notes: vec![
            "IR audio content is unavailable".into(),
            "Unknown IR carrier 0x7fed / /tmp/My IR {raw}".into(),
        ],
        unresolved_assets: vec![],
    };
    app.controller
        .dispatch(AppInput::Library(LibraryEvent::Imported {
            intent: IntentId(90),
            versions: vec![],
            report: report.clone(),
        }));
    app.controller
        .dispatch(AppInput::Ui(UiIntent::Navigate(ViewId::Library)));
    let context = egui::Context::default();
    theme::install_fonts(&context);
    draw(&mut app, &context, vec![]);
    let labels = text(&draw(&mut app, &context, vec![]));
    for note in &report.notes {
        assert!(
            labels.iter().any(|value| value == note),
            "missing exact diagnostic: {labels:?}"
        );
    }
    assert_eq!(
        app.controller.state().library.last_conversion.as_ref(),
        Some(&report)
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn retired_language_option_is_rejected_and_scale_remains_bounded_without_hardware() {
    for tag in ["en", "zh-CN", "zh-Hans"] {
        let result = LaunchOptions::parse(["--language".into(), tag.into()]);
        assert!(matches!(result, Err(error) if error == "Unknown argument --language"));
    }
    let options = LaunchOptions::parse(["--scale".into(), "1.5".into()]).unwrap();
    assert_eq!(options.scale, Some(1.5));
    assert!(!options.connect && !options.demo);
    for invalid in ["NaN", "inf", "0.5", "2.1", "bad"] {
        assert!(LaunchOptions::parse(["--scale".into(), invalid.into()]).is_err());
    }
}

#[test]
fn english_appearance_preferences_keep_scale_validation_and_recovery_documents() {
    let mut controller = AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    let before = controller.recovery_session();
    for invalid in [f32::NAN, f32::INFINITY, 0.25, 3.5] {
        let unchanged = controller.state().preferences.clone();
        let proposed = AppPreferences {
            scale: invalid,
            ..unchanged.clone()
        };
        assert!(
            controller
                .dispatch(AppInput::Ui(UiIntent::SetPreferences(proposed)))
                .is_empty()
        );
        assert_eq!(controller.state().preferences, unchanged);
    }
    for scale in [0.5, 1.5, 3.0] {
        let preferences = AppPreferences {
            scale,
            ..Default::default()
        };
        let effects =
            controller.dispatch(AppInput::Ui(UiIntent::SetPreferences(preferences.clone())));
        assert!(matches!(effects.as_slice(), [Effect::PersistSettings(p)] if p == &preferences));
        assert_eq!(controller.state().preferences, preferences);
    }
    assert_eq!(controller.recovery_session(), before);
}
