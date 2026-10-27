use super::*;

// ---- Font picker ----

const RECENT_KEY: &str = "figcraft_recent_fonts";

const ROW: f32 = 26.0;

/// The font list's tallest (it scrolls past that).
const LIST_HEIGHT: f32 = 380.0;

/// Return the persisted list of recently used font families.
fn recent_families(ctx: &egui::Context) -> Vec<String> {
    ctx.data_mut(|d| d.get_persisted::<Vec<String>>(Id::unique(RECENT_KEY))).unwrap_or_default()
}

/// Remember a font family at the front of the recent list, removing an earlier
/// case-insensitive match and keeping at most eight entries.
pub(super) fn remember_family(ctx: &egui::Context, family: &str) {
    ctx.data_mut(|d| {
        let v = d.get_persisted_mut_or_default::<Vec<String>>(Id::unique(RECENT_KEY));
        v.retain(|f| !f.eq_ignore_ascii_case(family));
        v.insert(0, family.to_string());
        v.truncate(8);
    });
}

enum PickRow {
    Header(&'static str),
    Family(String),
}

/// What the family list did this frame.
#[derive(Default)]
pub(crate) struct FontPick {
    /// The family clicked, or the one Enter picked.
    pub picked: Option<String>,
    /// The family Up and Down are on (Enter picks it).
    pub highlighted: Option<String>,
    /// The family the pointer is over (the caller previews it on the canvas).
    pub hovered: Option<String>,
}

/// The searchable family list: click a family, or step through it with Up and Down and press Enter.
/// Hovering a family previews it on the canvas (closing without picking restores the original).
pub(crate) fn font_picker(ui: &mut Ui, current: Option<&str>, just_opened: bool) -> FontPick {
    let t = Tokens::get(ui.ctx());
    let q_id = Id::unique("font_query");
    // The row Up and Down are on, and the search it is a row of.
    let hi_id = Id::unique("font_highlight");
    let mut q: String = ui.data(|d| d.get_temp(q_id)).unwrap_or_default();
    if just_opened {
        q.clear();
        ui.data_mut(|d| d.remove::<(String, usize)>(hi_id));
    }
    // Taken before the search field sees them (it would move its cursor).
    let step = ui.input_mut(|i| {
        i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) as i64
            - i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) as i64
    });
    let search = ui.add(egui::TextEdit::singleline(&mut q).hint_text(tr("Search fonts")).desired_width(ui.available_width()).font(regular(11.5)));
    crate::widgets::select_all_on_focus(ui, &search, &q);
    if just_opened {
        search.request_focus();
    }
    ui.data_mut(|d| d.insert_temp(q_id, q.clone()));
    let ql = q.trim().to_lowercase();
    let all = figcraft_text::family_names();
    let matches: Vec<String> = all.iter().filter(|f| ql.is_empty() || f.to_lowercase().contains(&ql)).cloned().collect();
    let mut rows: Vec<PickRow> = Vec::new();
    if ql.is_empty() {
        let recent: Vec<String> = recent_families(ui.ctx()).into_iter().filter(|f| all.iter().any(|a| a.eq_ignore_ascii_case(f))).collect();
        if !recent.is_empty() {
            rows.push(PickRow::Header(tr("Recent")));
            rows.extend(recent.into_iter().map(PickRow::Family));
            rows.push(PickRow::Header(tr("All fonts")));
        }
    }
    if matches.is_empty() {
        widgets::label(ui, tr("No fonts match"), t.text_tertiary);
        return FontPick::default();
    }
    rows.extend(matches.into_iter().map(PickRow::Family));
    let family = |i: usize| match rows.get(i) {
        Some(PickRow::Family(f)) => Some(f.clone()),
        _ => None,
    };
    // A new search starts again; the first step starts from the current family.
    let mut hi = ui.data(|d| d.get_temp::<(String, usize)>(hi_id)).filter(|(hq, _)| *hq == q).map(|(_, i)| i);
    let stride = ROW + ui.spacing().item_spacing.y;
    let mut offset = None;
    if step != 0 {
        let next = |i: usize| if step > 0 { i.checked_add(1) } else { i.checked_sub(1) };
        let mut from =
            hi.or_else(|| rows.iter().rposition(|r| matches!(r, PickRow::Family(f) if current.is_some_and(|c| c.eq_ignore_ascii_case(f)))));
        for _ in 0..step.unsigned_abs() {
            let mut at = match from {
                Some(i) => next(i),
                None if step > 0 => Some(0),
                None => rows.len().checked_sub(1),
            };
            // Past the headers; at either end it stays where it was.
            while let Some(i) = at.filter(|i| *i < rows.len() && family(*i).is_none()) {
                at = next(i);
            }
            if let Some(i) = at.filter(|i| family(*i).is_some()) {
                from = Some(i);
                hi = Some(i);
            }
        }
        if let Some(i) = hi {
            // Scrolled just enough to show it (the list is shorter than its tallest where the
            // window has less room).
            let (seen, height): (f32, f32) = ui.data(|d| d.get_temp(hi_id.with("view"))).unwrap_or((0.0, LIST_HEIGHT));
            let top = i as f32 * stride;
            offset = Some(seen.max(top + stride - height).min(top));
        }
    }
    match hi {
        Some(i) => ui.data_mut(|d| {
            d.insert_temp(hi_id, (q.clone(), i));
        }),
        None => ui.data_mut(|d| d.remove::<(String, usize)>(hi_id)),
    }
    if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        let first = rows.iter().find_map(|r| match r {
            PickRow::Family(f) => Some(f.clone()),
            PickRow::Header(_) => None,
        });
        return FontPick { picked: hi.and_then(family).or(first), highlighted: None, hovered: None };
    }
    let mut budget = 4;
    let mut picked = None;
    let mut hovered_family = None;
    let mut area = egui::ScrollArea::vertical().max_height(LIST_HEIGHT).auto_shrink([false, true]);
    if let Some(y) = offset {
        area = area.vertical_scroll_offset(y);
    }
    let shown = area.show_rows(ui, ROW, rows.len(), |ui, range| {
        for (i, r) in rows.iter().enumerate().skip(range.start).take(range.len()) {
            let w = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(vec2(w, ROW), if matches!(r, PickRow::Family(_)) { Sense::click() } else { Sense::hover() });
            match r {
                PickRow::Header(h) => {
                    ui.painter().text(pos2(rect.min.x + 8.0, rect.center().y), Align2::LEFT_CENTER, *h, semibold(10.5), t.text_secondary);
                }
                PickRow::Family(f) => {
                    let hovered = resp.hovered() || hi == Some(i);
                    if hovered {
                        ui.painter().rect_filled(rect, CornerRadius::same(5), t.accent);
                    }
                    let col = if hovered { t.accent_on } else { t.text };
                    if current.is_some_and(|c| c.eq_ignore_ascii_case(f)) {
                        icons::paint(
                            ui.painter(),
                            Rect::from_center_size(pos2(rect.min.x + 10.0, rect.center().y), vec2(12.0, 12.0)),
                            Icon::Check,
                            col,
                        );
                    }
                    let text_min = pos2(rect.min.x + 22.0, rect.min.y);
                    match crate::font_preview::family(ui.ctx(), f, 13.0, &mut budget) {
                        Some(tex) => {
                            crate::font_preview::paint(
                                ui.painter(),
                                &tex,
                                pos2(text_min.x, rect.center().y),
                                rect.max.x - text_min.x - 6.0,
                                ROW,
                                col,
                            );
                        }
                        None => {
                            ui.painter().text(pos2(text_min.x, rect.center().y), Align2::LEFT_CENTER, f, regular(12.0), col);
                        }
                    }
                    if resp.clicked() {
                        picked = Some(f.clone());
                    }
                    if resp.hovered() {
                        hovered_family = Some(f.clone());
                    }
                }
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(hi_id.with("view"), (shown.state.clamped_offset().y, shown.inner_rect.height())));
    FontPick { highlighted: hi.and_then(family), picked, hovered: hovered_family }
}
