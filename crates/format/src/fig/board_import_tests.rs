//! FigJam generated layers (#1341). All document content and outlines are synthetic.

use super::*;

fn board_schema() -> Schema {
    Schema::parse(&SCHEMA.replace(
        "\nmessage Blob ",
        ", GeneratedData nodeGenerationData, GeneratedData derivedImmutableFrameData, string textAlignHorizontal, string textAlignVertical\nmessage GeneratedData NodeChange[] overrides, uint version\nmessage Blob ",
    ))
    .unwrap()
}

fn record(id: u64, mut fields: Vec<(&'static str, Kv<'static>)>) -> Kv<'static> {
    fields.push(("guidPath", o(vec![("guids", arr(vec![o(vec![("sessionID", u(40_000_000)), ("localID", u(id))])]))])));
    o(fields)
}

fn text(chars: &str, size: f64) -> Vec<(&'static str, Kv<'static>)> {
    vec![
        (
            "textData",
            o(vec![
                ("characters", s(chars)),
                ("characterStyleIDs", arr(vec![u(0), u(1)])),
                ("styleOverrideTable", arr(vec![o(vec![("styleID", u(1)), ("fontSize", f(18.0))])])),
            ]),
        ),
        ("fontSize", f(size)),
        ("fontName", o(vec![("family", s("Synthetic Sans")), ("style", s("Bold"))])),
        ("fillPaints", arr(vec![solid(0.0, 0.0, 0.0)])),
        ("textAlignHorizontal", s("LEFT")),
        ("textAlignVertical", s("TOP")),
    ]
}

fn board_node(ty: &'static str) -> Kv<'static> {
    let background = record(0, vec![("fillPaints", arr(vec![solid(1.0, 0.75, 0.25)])), ("opacity", f(1.0))]);
    let note = record(1, text("Olá\nIdea", 24.0));
    let author = record(2, text("Test Author", 12.0));
    let bounds = |id, x, y, w, h| record(id, vec![("size", xy(w, h)), ("transform", at(x, y))]);
    node(
        2,
        Some((1, "!")),
        ty,
        "Board object",
        vec![
            ("size", xy(240.0, 240.0)),
            ("transform", at(100.0, 150.0)),
            ("opacity", f(0.5)),
            ("shapeWithTextType", e("ELLIPSE")),
            // Deliberately shuffled: records are addressed by their full template GUID.
            ("nodeGenerationData", o(vec![("overrides", arr(vec![author, background, note]))])),
            (
                "derivedImmutableFrameData",
                o(vec![
                    ("version", u(1)),
                    (
                        "overrides",
                        arr(vec![
                            bounds(2, 20.0, 204.0, 200.0, 16.0),
                            bounds(1, 20.0, 20.0, 200.0, 160.0),
                            record(
                                0,
                                vec![
                                    ("size", xy(240.0, 240.0)),
                                    ("transform", at(0.0, 0.0)),
                                    ("fillGeometry", arr(vec![o(vec![("commandsBlob", u(0)), ("windingRule", e("NONZERO"))])])),
                                ],
                            ),
                        ]),
                    ),
                ]),
            ),
        ],
    )
}

fn message(layer: Kv<'static>) -> Kv<'static> {
    o(vec![
        ("nodeChanges", arr(vec![node(0, None, "DOCUMENT", "Synthetic board", vec![]), node(1, Some((0, "!")), "CANVAS", "Page", vec![]), layer])),
        ("blobs", arr(vec![o(vec![("bytes", Kv::Bytes(triangle_blob()))])])),
    ])
}

fn imported(layer: Kv<'static>) -> (Document, Vec<String>) {
    import(&canvas(&board_schema(), &message(layer)), "Synthetic board").unwrap()
}

#[test]
fn generated_stickies_and_shapes_keep_their_background_text_and_author() {
    for ty in ["STICKY", "SHAPE_WITH_TEXT"] {
        let (doc, warnings) = imported(board_node(ty));
        let frame = named(&doc, "Board object");
        assert!(matches!(frame.kind, NodeKind::Frame));
        assert_eq!(frame.opacity, 0.5);
        assert_eq!(frame.children.len(), if ty == "STICKY" { 3 } else { 2 });
        let background = doc.node(frame.children[0]).unwrap();
        assert!(matches!(background.kind, NodeKind::Vector { .. }));
        assert_eq!(background.fills[0].solid_color().unwrap().to_rgba8(), [255, 191, 64, 255]);
        assert_eq!(background.opacity, 1.0);
        let note = doc.node(frame.children[1]).unwrap();
        let t = note.kind.text().unwrap();
        assert_eq!(t.characters, "Olá\nIdea");
        assert_eq!(t.style.font_size, 24.0);
        assert_eq!(t.style.font_family, "Synthetic Sans");
        assert_eq!(note.fills[0].solid_color().unwrap().to_rgba8(), [0, 0, 0, 255]);
        assert_eq!(note.transform.translation(), figcraft_doc::geom::Vec2::new(20.0, 20.0));
        assert_eq!((note.width, note.height), (200.0, 160.0));
        assert!(t.runs.iter().any(|r| r.style.font_size == 18.0), "inline styles survive");
        if ty == "STICKY" {
            let author = doc.node(frame.children[2]).unwrap();
            assert_eq!(author.kind.text().unwrap().characters, "Test Author");
            assert_eq!(author.kind.text().unwrap().style.font_size, 12.0);
            assert_eq!(author.transform.translation(), figcraft_doc::geom::Vec2::new(20.0, 204.0));
        }
        assert!(!warnings.iter().any(|w| w.contains("skipped")), "{warnings:?}");
    }
}

fn set(value: &mut Kv<'static>, field: &'static str, replacement: Kv<'static>) {
    let Kv::Object(fields) = value else { panic!("object") };
    if let Some((_, old)) = fields.iter_mut().find(|(name, _)| *name == field) {
        *old = replacement;
    } else {
        fields.push((field, replacement));
    }
}

#[test]
fn missing_generated_bounds_warn_without_losing_the_background() {
    let mut layer = board_node("STICKY");
    set(
        &mut layer,
        "derivedImmutableFrameData",
        o(vec![("version", u(1)), ("overrides", arr(vec![record(0, vec![("size", xy(240.0, 240.0)), ("transform", at(0.0, 0.0))])]))]),
    );
    let (doc, warnings) = imported(layer);
    let frame = named(&doc, "Board object");
    assert_eq!(frame.children.len(), 1);
    let background = doc.node(frame.children[0]).unwrap();
    assert!(matches!(background.kind, NodeKind::Rectangle));
    assert_eq!(background.fills.len(), 1);
    assert!(warnings.iter().any(|w| w.contains("missing its style or bounds")), "{warnings:?}");
}

#[test]
fn invalid_generated_sizes_and_transforms_are_skipped() {
    for (width, x) in [(-1.0, 20.0), (f64::INFINITY, 20.0), (200.0, f64::NAN)] {
        let mut layer = board_node("STICKY");
        set(
            &mut layer,
            "derivedImmutableFrameData",
            o(vec![("version", u(1)), ("overrides", arr(vec![record(1, vec![("size", xy(width, 160.0)), ("transform", at(x, 20.0))])]))]),
        );
        // The fixture encoder refuses non-finite floats; exercise the mapper's guard directly.
        let msg = message(layer);
        let (json, warnings, _) = map::to_rest(&msg, "Invalid bounds", 100);
        let (doc, _) = crate::rest::import(&json, &|_| None).unwrap();
        assert!(named(&doc, "Board object").children.is_empty());
        assert!(warnings.iter().any(|w| w.contains("invalid bounds")), "{warnings:?}");
    }
}

#[test]
fn unsupported_generated_versions_keep_the_legacy_fallback() {
    let mut layer = board_node("SHAPE_WITH_TEXT");
    set(&mut layer, "derivedImmutableFrameData", o(vec![("version", u(2))]));
    set(&mut layer, "textData", o(vec![("characters", s("Legacy text"))]));
    set(&mut layer, "fillPaints", arr(vec![solid(0.0, 1.0, 0.0)]));
    let (doc, warnings) = imported(layer);
    let frame = named(&doc, "Board object");
    assert_eq!(frame.fills.len(), 1);
    assert_eq!(frame.children.len(), 1);
    assert_eq!(doc.node(frame.children[0]).unwrap().kind.text().unwrap().characters, "Legacy text");
    assert!(warnings.iter().any(|w| w.contains("unsupported")), "{warnings:?}");
}

#[test]
fn generated_children_stay_within_the_layer_budget() {
    for cap in 1..6 {
        let msg = message(board_node("STICKY"));
        let (json, warnings, _) = map::to_rest(&msg, "Budget", cap);
        fn count(n: &Value) -> usize {
            1 + n.get("children").and_then(Value::as_array).map_or(0, |kids| kids.iter().map(count).sum::<usize>())
        }
        assert_eq!(count(&json["document"]) - 2, cap.min(4), "{json}");
        if cap < 4 {
            assert!(warnings.iter().any(|w| w.contains("limit") || w.contains("skipped")), "{warnings:?}");
        }
    }
}

#[test]
fn malformed_generated_records_never_panic() {
    for junk in [Kv::Bool(false), f(f64::NAN), f(f64::INFINITY), s("junk"), arr(vec![]), o(vec![])] {
        for field in ["nodeGenerationData", "derivedImmutableFrameData"] {
            let mut layer = board_node("STICKY");
            set(&mut layer, field, junk.clone());
            let msg = message(layer);
            let (json, _, _) = map::to_rest(&msg, "Junk", 100);
            let _ = crate::rest::import(&json, &|_| None);
        }
    }
}
