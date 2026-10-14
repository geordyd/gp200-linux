use gp200_domain::*;

#[test]
fn old_preferences_and_partial_preferences_keep_local_defaults() {
    let old = r#"{"theme":"Dark","scale":1.25,"language":"en","compact":true,"last_input":"GP-200","last_output":null}"#;
    let preferences: AppPreferences = serde_json::from_str(old).unwrap();
    assert_eq!(preferences.theme, ThemePreference::Dark);
    assert_eq!(preferences.scale, 1.25);
    assert!(preferences.dialog_directories.is_empty());
    assert_eq!(
        serde_json::from_str::<AppPreferences>("{}").unwrap(),
        AppPreferences::default()
    );
}

#[test]
fn separate_directory_purposes_roundtrip_without_affecting_device_choices() {
    let mut preferences = AppPreferences {
        last_input: Some("exact-input".into()),
        ..Default::default()
    };
    preferences.dialog_directories.insert(
        DialogDirectoryPurpose::PresetImport,
        "/music/presets".into(),
    );
    preferences.dialog_directories.insert(
        DialogDirectoryPurpose::CalibrationStimulus,
        "/music/calibration".into(),
    );
    let bytes = serde_json::to_vec(&preferences).unwrap();
    let decoded: AppPreferences = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, preferences);
    assert!(
        !decoded
            .dialog_directories
            .contains_key(&DialogDirectoryPurpose::PresetExport)
    );
    assert_eq!(decoded.last_input.as_deref(), Some("exact-input"));
}

#[test]
fn each_theme_preference_round_trips() {
    for theme in [
        ThemePreference::Dark,
        ThemePreference::Light,
        ThemePreference::System,
    ] {
        let prefs = AppPreferences {
            theme,
            ..AppPreferences::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let restored: AppPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, prefs);
    }
}

#[test]
fn legacy_language_tags_migrate_to_english_without_changing_other_preferences() {
    for tag in [
        "en", "en-US", "en-GB", "English", "zh", "zh_cn", "zh-Hans", "zh-CN", "Chinese",
    ] {
        let old = serde_json::json!({
            "language": tag, "theme": "Light", "scale": 1.5, "compact": true,
            "last_input": "hw:GP200,0,0", "last_output": "用户设备",
            "dialog_directories": {"preset_import": "/music/我的音色", "ir_import": "/music/IR"}
        });
        let preferences: AppPreferences = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(preferences.language, LanguagePreference::English);
        let saved = serde_json::to_value(&preferences).unwrap();
        let mut expected = old;
        expected["language"] = "en".into();
        assert_eq!(saved, expected, "other preferences changed for {tag}");
        assert_eq!(
            serde_json::from_value::<AppPreferences>(saved).unwrap(),
            preferences
        );
    }
    assert_eq!(
        serde_json::from_str::<AppPreferences>("{}")
            .unwrap()
            .language,
        LanguagePreference::English
    );
    for tag in ["", "zh-TW", "unsupported", "../zh"] {
        assert!(
            serde_json::from_value::<AppPreferences>(serde_json::json!({"language": tag})).is_err()
        );
    }
}
