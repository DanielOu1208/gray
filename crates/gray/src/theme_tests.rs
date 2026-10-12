use super::*;

#[test]
fn every_role_name_maps_to_a_distinct_field() {
    // All fields are `Color`, so the struct size counts them: a new role
    // without a ROLE_NAMES / role_mut entry fails here.
    let fields = std::mem::size_of::<UiTheme>() / std::mem::size_of::<Color>();
    assert_eq!(ROLE_NAMES.len(), fields);
    for (i, name) in ROLE_NAMES.iter().enumerate() {
        let mut t = GRAY_UI_THEME;
        *t.role_mut(name).unwrap_or_else(|| panic!("{name}")) = Color::Indexed(i as u8);
        let changed = ROLE_NAMES
            .iter()
            .filter(|n| t.role(n) != GRAY_UI_THEME.role(n))
            .count();
        assert_eq!(changed, 1, "{name} must touch exactly one field");
    }
}

#[test]
fn partial_theme_overrides_only_named_roles() {
    let (t, warnings) =
        parse_theme(r##"{"colors":{"accent":"#ff0000","surface_bg":"default"}}"##).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(t.accent, Color::Rgb(255, 0, 0));
    assert_eq!(t.surface_bg, Color::Reset);
    assert_eq!(t.text_body, GRAY_UI_THEME.text_body);
    // A transparent surface has no color to derive a window from, so the
    // window stays transparent as well.
    assert_eq!(t.window_bg, Color::Reset);
}

#[test]
fn window_bg_tracks_surface_unless_overridden() {
    let (t, warnings) = parse_theme(r##"{"colors":{"surface_bg":"#fafafa"}}"##).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(t.window_bg, derive_window(Color::Rgb(0xfa, 0xfa, 0xfa)));

    let (t, _) =
        parse_theme(r##"{"colors":{"window_bg":"reset","surface_bg":"#fafafa"}}"##).unwrap();
    assert_eq!(t.window_bg, Color::Reset);
    assert_eq!(t.window(), Color::Reset);

    let (t, _) = parse_theme(r##"{"colors":{"window_bg":"#112233"}}"##).unwrap();
    assert_eq!(t.window_bg, Color::Rgb(0x11, 0x22, 0x33));

    assert_eq!(GRAY_UI_THEME.window_bg, Color::Reset);
}

#[test]
fn derived_window_steps_away_from_the_surface() {
    // Dark surface: the window sinks below the cards.
    let (dark, _) = parse_theme(r##"{"colors":{"surface_bg":"#202020"}}"##).unwrap();
    assert_ne!(dark.window_bg, dark.surface_bg);
    assert_eq!(dark.window_bg, Color::Rgb(18, 18, 18));

    // Light surface: the window rises above the cards.
    let (light, _) = parse_theme(r##"{"colors":{"surface_bg":"#dfe4c4"}}"##).unwrap();
    assert_ne!(light.window_bg, light.surface_bg);
    assert_eq!(light.window_bg, Color::Rgb(241, 243, 228));
}

#[test]
fn derive_window_only_moves_rgb_surfaces() {
    // Transparent, indexed and named surfaces have no channels to move.
    assert_eq!(derive_window(Color::Reset), Color::Reset);
    assert_eq!(derive_window(Color::Indexed(236)), Color::Indexed(236));
    assert_eq!(derive_window(Color::Black), Color::Black);
}

#[test]
fn colors_accept_vars_names_indexes_and_short_hex() {
    let (t, warnings) = parse_theme(
        r##"{
          "vars": {"peach": "#f6ad7e", "alias": "peach"},
          "colors": {
            "accent": "alias",
            "info": "lightblue",
            "error": 196,
            "success": "34",
            "rose": "#fab"
          }
        }"##,
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(t.accent, Color::Rgb(246, 173, 126));
    assert_eq!(t.info, Color::LightBlue);
    assert_eq!(t.error, Color::Indexed(196));
    assert_eq!(t.success, Color::Indexed(34));
    assert_eq!(t.rose, Color::Rgb(0xff, 0xaa, 0xbb));
}

#[test]
fn bad_entries_warn_but_the_rest_applies() {
    let (t, warnings) =
        parse_theme(r#"{"colors":{"accnet":"red","info":"blurple","accent":"red"}}"#).unwrap();
    assert_eq!(t.accent, Color::Red);
    assert_eq!(t.info, GRAY_UI_THEME.info);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
}

#[test]
fn var_cycles_are_reported_not_hung() {
    let (_, warnings) =
        parse_theme(r#"{"vars":{"a":"b","b":"a"},"colors":{"accent":"a"}}"#).unwrap();
    assert_eq!(warnings.len(), 1);
}

#[test]
fn structural_errors_fail() {
    assert!(parse_theme("[]").is_err());
    assert!(parse_theme("{}").is_err());
    assert!(parse_theme(r#"{"colors":[]}"#).is_err());
    assert!(parse_theme("{").is_err());
}

#[test]
fn exported_palette_roundtrips() {
    let json = theme_to_json(&GRAY_UI_THEME);
    let (t, warnings) = parse_theme(&json).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(t, GRAY_UI_THEME);
}

#[test]
fn theme_files_are_listed_and_loaded_by_stem() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.json"), r#"{"colors":{}}"#).unwrap();
    std::fs::write(dir.path().join("a.json"), r#"{"colors":{"accent":"red"}}"#).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
    assert_eq!(list_themes_in(dir.path()), vec!["a", "b"]);
    assert_eq!(load_theme_in(dir.path(), "a").unwrap().0.accent, Color::Red);
    assert!(load_theme_in(dir.path(), "missing").is_err());
    assert!(load_theme_in(dir.path(), "../a").is_err());
}

#[test]
fn builtin_names() {
    assert!(is_builtin_name("gray"));
    assert!(is_builtin_name("Default"));
    assert!(!is_builtin_name("grayish"));
}

#[test]
fn derived_window_keeps_its_side_of_near_extreme_surfaces() {
    // Blackgold's #0d0d0d used to come out #191919, lighter than its cards.
    // Near-black surfaces sink further toward black, never above the cards.
    let (blackgold, _) = parse_theme(r##"{"colors":{"surface_bg":"#0d0d0d"}}"##).unwrap();
    assert_eq!(blackgold.window_bg, Color::Rgb(1, 1, 1));
    assert_eq!(derive_window(Color::Rgb(4, 4, 4)), Color::Rgb(0, 0, 0));
    // Near-white surfaces rise toward white, never below the cards.
    assert_eq!(
        derive_window(Color::Rgb(0xfa, 0xfa, 0xfa)),
        Color::Rgb(255, 255, 255)
    );
    // Pure black and white have no room on their far side.
    assert_eq!(derive_window(Color::Rgb(0, 0, 0)), Color::Rgb(0, 0, 0));
    assert_eq!(
        derive_window(Color::Rgb(255, 255, 255)),
        Color::Rgb(255, 255, 255)
    );
}

#[test]
fn derived_window_sits_on_the_far_side_of_every_surface() {
    // Sweep a coarse RGB grid: by luma, a dark surface's window is darker and
    // a light surface's is lighter. Pure black and white stay equal.
    let luma =
        |r: u8, g: u8, b: u8| 0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b);
    let steps: Vec<u8> = (0..=255).step_by(17).collect();
    for &r in &steps {
        for &g in &steps {
            for &b in &steps {
                let Color::Rgb(wr, wg, wb) = derive_window(Color::Rgb(r, g, b)) else {
                    panic!("rgb surface ({r},{g},{b}) must derive an rgb window");
                };
                if (r, g, b) == (0, 0, 0) || (r, g, b) == (255, 255, 255) {
                    assert_eq!((wr, wg, wb), (r, g, b));
                    continue;
                }
                let (s, w) = (luma(r, g, b), luma(wr, wg, wb));
                if s < 128.0 {
                    assert!(
                        w < s,
                        "dark ({r},{g},{b}): window luma {w:.1} not below {s:.1}"
                    );
                } else {
                    assert!(
                        w > s,
                        "light ({r},{g},{b}): window luma {w:.1} not above {s:.1}"
                    );
                }
            }
        }
    }
}
