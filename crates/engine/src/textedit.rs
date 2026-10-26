//! On-canvas text editing: caret, selection, typing. A whole editing session is one undo step.

use std::sync::{LazyLock, Mutex};

use figcraft_doc::{Document, ListStyle, NodeId, NodeKind, TextData};

use crate::{EngineError, Preedit, Result, Session, TextEdit};

/// Most committed UTF-8 bytes an outgoing IME snapshot may describe.
///
/// This mirrors the fork's `egui::output::MAX_IME_SURROUNDING_TEXT_BYTES`; the engine keeps its
/// own constant so this layer has no egui dependency. The query below returns only this bounded
/// window around the selection, never a copy of the whole document.
pub const MAX_IME_SURROUNDING_BYTES: usize = 4000;

/// Stable identity of the on-canvas text edit an IME context belongs to.
///
/// `document` and `node` do not change through ordinary text or caret edits. `editing_session`
/// changes when [`begin`] starts a new session for an equal `(document, node)` pair (leaving and
/// re-entering the same layer), so a replaced target never inherits an old composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImeEditOwner {
    /// The document tab's uid.
    pub document: u64,
    /// The edited text node.
    pub node: NodeId,
    /// [`TextEdit::editing_session`] of the current edit.
    pub editing_session: u64,
}

/// Why the committed text around the IME caret cannot be described.
///
/// These are the only reasons this custom editor reports
/// `egui::output::IMESurroundingText::Unavailable`; an empty text layer is available with empty
/// text and a collapsed selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImeSurroundingUnavailable {
    /// No text layer is being edited.
    NotEditing,
    /// The editing target is not a text node any more.
    NotText,
    /// The selection alone is wider than [`MAX_IME_SURROUNDING_BYTES`], so no bounded snapshot
    /// can contain both selection ends.
    SelectionTooWide,
    /// Defensive: the computed window was not a character-boundary slice. The rounding above
    /// makes this unreachable for a well-formed text model.
    InvalidWindow,
}

/// One bounded, committed logical-text observation for the outgoing IME snapshot.
///
/// `text` is never display/bidi order and never contains the pre-edit (which is UI-only in this
/// editor). `cursor` and `anchor` are UTF-8 byte offsets into `text`; `anchor > cursor` is a
/// valid reversed selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImeSurrounding {
    /// Identity of the edit this observation belongs to.
    pub owner: ImeEditOwner,
    /// At most [`MAX_IME_SURROUNDING_BYTES`] bytes of committed text around the selection.
    pub text: String,
    /// UTF-8 byte offset of the primary caret in [`Self::text`].
    pub cursor: usize,
    /// UTF-8 byte offset of the selection anchor in [`Self::text`].
    pub anchor: usize,
}

/// Every editing session gets its own token; zero is valid and never reused deliberately.
///
/// A mutex (rather than an atomic) also works on wasm32 targets without 64-bit atomics.
static EDITING_SESSION: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(0));

/// Return the next editing-session token from a mutex-protected counter (`wrapping_add`).
/// Zero is a valid token; the mutex also works on wasm32 without 64-bit atomics.
fn next_editing_session() -> u64 {
    let mut session = match EDITING_SESSION.lock() {
        Ok(session) => session,
        Err(poisoned) => poisoned.into_inner(),
    };
    *session = session.wrapping_add(1);
    *session
}

fn text_of(s: &Session, id: NodeId) -> Result<TextData> {
    match &s.doc()?.get(id)?.kind {
        NodeKind::Text(t) => Ok(t.clone()),
        _ => Err(EngineError::Other(format!("{id} is not a text layer"))),
    }
}

/// Start editing text node `id` with the caret at `caret` (None = select all).
pub fn begin(s: &mut Session, id: NodeId, caret: Option<usize>, created: bool) -> Result<()> {
    let t = text_of(s, id)?;
    let n = t.char_count();
    s.begin_interaction("Edit text")?;
    let ds = s.ds_mut()?;
    // Editing the same layer again (a double-click into a text just made) keeps it new.
    let created = created || ds.text_edit.as_ref().is_some_and(|te| te.id == id && te.created);
    let (caret, anchor) = match caret {
        Some(c) => (c.min(n), c.min(n)),
        None => (n, 0),
    };
    ds.text_edit = Some(TextEdit { id, caret, anchor, created, editing_session: next_editing_session(), preedit: None });
    ds.selection = vec![id];
    ds.revision += 1;
    s.live_text_notice(id);
    Ok(())
}

/// Finish editing: an empty new text layer is removed; the session becomes one undo step.
pub fn end(s: &mut Session) -> Result<()> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(()) };
    s.ds_mut()?.text_edit = None;
    // A widget's editable field: the typing is put back and the new value goes to the widget's
    // plugin through `widget.setText`, one undo step (or none when nothing changed).
    if let Some((w, field)) = s.doc().ok().and_then(|d| crate::widgets::field_of(d, te.id)) {
        let value = text_of(s, te.id).map(|t| t.characters).unwrap_or_default();
        s.cancel_interaction()?;
        let ds = s.ds_mut()?;
        ds.selection = vec![w];
        ds.revision += 1;
        if text_of(s, te.id).is_ok_and(|t| t.characters != value)
            && let Err(e) = s.execute("widget.setText", &serde_json::json!({"id": w, "field": field, "value": value}))
        {
            s.message = Some(e.to_string());
        }
        return Ok(());
    }
    // A board object's text (a sticky's note) stays when emptied; the object is selected again.
    let owner = s.doc().ok().and_then(|d| crate::board::owner_of_text(d, te.id));
    let empty = owner.is_none() && text_of(s, te.id).map(|t| t.characters.trim().is_empty()).unwrap_or(false);
    if empty {
        let doc = s.doc_mut()?;
        let _ = doc.remove(te.id);
        s.ds_mut()?.selection.clear();
    }
    if empty && te.created {
        // Nothing to undo for a text box that was never filled. The gesture snapshot was taken
        // after the layer was added, so it still holds it: re-base it on the document without the
        // layer, and committing then records no step (and leaves the redo stack alone).
        // A snapshot taken before the layer was added (the Text on a path tool, which also took
        // the path layer away) is put back whole instead.
        let ds = s.ds_mut()?;
        let doc = ds.doc.clone();
        if ds.interaction.as_ref().is_some_and(|it| it.doc.node(te.id).is_none()) {
            s.cancel_interaction()?;
        } else if let Some(it) = ds.interaction.as_mut() {
            it.doc = doc;
        }
    }
    s.commit_interaction()?;
    let ds = s.ds_mut()?;
    if let Some(o) = owner {
        ds.selection = vec![o];
    }
    ds.revision += 1;
    Ok(())
}

fn range(te: &TextEdit) -> (usize, usize) {
    (te.caret.min(te.anchor), te.caret.max(te.anchor))
}

/// Apply an unconditional text-model mutation through `edit_if`, whose success clears the
/// composition, clamps caret and anchor to the character count, marks a `characters` override
/// when the node is source-backed, normalizes the document and bumps the revision.
fn edit(s: &mut Session, f: impl FnOnce(&mut TextData, &mut TextEdit)) -> Result<()> {
    edit_if(s, |t, te| {
        f(t, te);
        true
    })
    .map(|_| ())
}

/// [`edit`] for a mutation that can still turn out to be a no-op.
///
/// The closure returns whether it changed the model; return `false` only before mutating (the
/// text is handed over directly). A no-op leaves the composition (`preedit`), the revision and the
/// document untouched, so an IME deletion that finds nothing to delete does not cancel an
/// in-progress composition.
fn edit_if(s: &mut Session, f: impl FnOnce(&mut TextData, &mut TextEdit) -> bool) -> Result<bool> {
    let ds = s.ds_mut()?;
    let Some(mut te) = ds.text_edit.clone() else { return Err(EngineError::Other("not editing text".into())) };
    let before = ds.doc.clone();
    let doc = ds.doc_mut();
    let n = doc.get_mut(te.id)?;
    let NodeKind::Text(t) = &mut n.kind else { return Err(EngineError::Other("not a text layer".into())) };
    if !f(t, &mut te) {
        return Ok(false);
    }
    // Any edit ends a composition (a commit replaces it with the final text).
    te.preedit = None;
    let len = t.char_count();
    te.caret = te.caret.min(len);
    te.anchor = te.anchor.min(len);
    if n.source.is_some() {
        n.overrides.insert("characters".into());
    }
    crate::normalize(doc, &before);
    ds.text_edit = Some(te);
    ds.revision += 1;
    Ok(true)
}

/// What an accepted IME `Commit` did to the committed model.
///
/// An IME receipt may label the snapshot `InputMethod` only for [`Self::Plain`]. AutoFormat
/// (typing "- " at a line start turns the paragraph into a list) is an application
/// transformation in the same command, so the frame's change must be reported as `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImeInsertion {
    /// Nothing was inserted (an empty, control-only, or bare-newline commit).
    Nothing,
    /// The committed text replaced the selection exactly.
    Plain,
    /// The editor transformed the commit before applying it: paragraph AutoFormat, or line-ending
    /// normalisation/control-character removal. The frame's change is application-origin.
    Transformed,
}

/// Type (or paste) `text` replacing the selection. Line endings are normalised and other control
/// characters dropped, so pasted plain text from any platform lands cleanly.
pub fn insert(s: &mut Session, text: &str) -> Result<()> {
    insert_model(s, text, false).map(|_| ())
}

/// End the composition without changing the document, for an empty IME commit.
///
/// Returns without touching the session when no composition is active (or nothing is being
/// edited): an empty commit is otherwise a no-op, never an error.
fn clear_composition(s: &mut Session) -> Result<()> {
    let composing = s.ds().ok().and_then(|d| d.text_edit.as_ref()).is_some_and(|te| te.preedit.is_some());
    if !composing {
        return Ok(());
    }
    let ds = s.ds_mut()?;
    if let Some(te) = ds.text_edit.as_mut() {
        te.preedit = None;
    }
    ds.revision += 1;
    Ok(())
}

/// Apply an IME `Commit`, reporting whether it was a plain insertion.
///
/// The UI calls this instead of [`insert`] so only a commit that the editor applied as a plain
/// insertion can earn an `InputMethod` receipt. AutoFormat, line-ending normalisation and
/// control-character removal are application transformations and are reported as
/// [`ImeInsertion::Transformed`]. Like stock `TextEdit`, a bare `"\n"`/`"\r"` commit is ignored.
pub fn insert_ime_commit(s: &mut Session, text: &str) -> Result<ImeInsertion> {
    insert_model(s, text, true)
}

/// Insert committed text, optionally as an IME commit: normalize line endings, drop control
/// characters except `\n`/`\t`, ignore a bare newline/CR IME commit, clear an active
/// composition for an empty or control-only IME commit, and turn a committed space after a
/// line-start `-`, `*` or `1.` into a bulleted/numbered list. Reports whether the insertion was
/// plain, transformed, or a no-op.
fn insert_model(s: &mut Session, text: &str, ime_commit: bool) -> Result<ImeInsertion> {
    // Stock `TextEdit` deliberately ignores these two commits (they are not a mutation); only a
    // real commit may do so, since typing Enter goes through `insert`.
    if ime_commit && matches!(text, "\n" | "\r") {
        return Ok(ImeInsertion::Nothing);
    }
    let normalized: String =
        crate::cmd::props::normalize_line_endings(text).chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).collect();
    if normalized.is_empty() {
        // An empty commit is how some integrations (Safari's egui web integration when the last
        // composition character is deleted) end the composition. It inserts nothing, but it must
        // still clear the pre-edit; stock `TextEdit` does the same in its non-ignored `Commit` arm.
        if ime_commit {
            clear_composition(s)?;
        }
        return Ok(ImeInsertion::Nothing);
    }
    // A commit the editor had to transform is an application edit, not a plain insertion.
    let mut outcome = if normalized == text { ImeInsertion::Plain } else { ImeInsertion::Transformed };
    let text = normalized;
    edit(s, |t, te| {
        let (a, b) = range(te);
        // "- ", "* " or "1. " typed at the start of a plain paragraph starts a list.
        if text == " " && a == b && t.para_list(t.para_of(a)).list == ListStyle::None {
            let chars: Vec<char> = t.characters.chars().collect();
            let start = line_start(&chars, a);
            let typed: String = chars.get(start..a).unwrap_or(&[]).iter().collect();
            let list = match typed.as_str() {
                "-" | "*" => ListStyle::Bulleted,
                "1." => ListStyle::Numbered,
                _ => ListStyle::None,
            };
            if list != ListStyle::None {
                t.replace_range(start, a, "");
                t.set_para_lists(start, start, |pl| pl.list = list);
                (te.caret, te.anchor) = (start, start);
                outcome = ImeInsertion::Transformed;
                return;
            }
        }
        t.replace_range(a, b, &text);
        te.caret = a + text.chars().count();
        te.anchor = te.caret;
    })?;
    Ok(outcome)
}

/// List keys: Tab nests the selected list items a level deeper; Shift+Tab, Backspace at an item's
/// start and Enter on an empty item move it a level out (at the top: out of the list). Returns
/// whether the key was taken.
fn list_key(s: &mut Session, key: &str, shift: bool) -> Result<bool> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(false) };
    let t = text_of(s, te.id)?;
    let (a, b) = range(&te);
    let chars: Vec<char> = t.characters.chars().collect();
    let at_start = a == b && line_start(&chars, a) == a;
    let item = t.para_list(t.para_of(a)).list != ListStyle::None;
    let (pa, pb) = t.para_span(a, b);
    let any_item = (pa..=pb).any(|p| t.para_list(p).list != ListStyle::None);
    let deeper = match key {
        "Tab" if any_item => !shift,
        "Backspace" if item && at_start => false,
        "Enter" if item && at_start && chars.get(a).is_none_or(|c| *c == '\n') => false,
        _ => return Ok(false),
    };
    edit(s, |t, _| {
        t.set_para_lists(a, b, |pl| match pl.list {
            ListStyle::None => {}
            _ if deeper => pl.indent = pl.indent.saturating_add(1),
            _ if pl.indent > 0 => pl.indent -= 1,
            _ => pl.list = ListStyle::None,
        })
    })?;
    Ok(true)
}

/// Cmd/Ctrl+] and Cmd/Ctrl+[ while editing: the selected list items a level deeper or out, like
/// Tab and Shift+Tab. Returns whether anything changed (text that isn't a list and items already
/// at the deepest level stay as they are).
pub fn list_indent(s: &mut Session, out: bool) -> Result<bool> {
    let Some(id) = s.ds()?.text_edit.as_ref().map(|te| te.id) else { return Ok(false) };
    let before = text_of(s, id)?;
    if !list_key(s, "Tab", out)? {
        return Ok(false);
    }
    Ok(text_of(s, id)? != before)
}

/// The kind of word char `i` belongs to: 0 for spaces and punctuation, else its script, so a word
/// ends where the script changes (Han, hiragana, katakana and Hangul runs are words of their own in
/// text without spaces). Combining marks and voicing marks belong to the char before them.
fn word_class(chars: &[char], mut i: usize) -> u8 {
    while i > 0
        && chars.get(i).is_some_and(
            |c| matches!(*c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0x3099..=0x309A | 0xFE20..=0xFE2F),
        )
    {
        i -= 1;
    }
    match chars.get(i).copied() {
        Some(
            '\u{3005}'
            | '\u{3006}'
            | '\u{3007}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{3FFFF}',
        ) => 2,
        Some('\u{3041}'..='\u{309F}') => 3,
        Some('\u{30A0}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9F}') => 4,
        Some('\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}' | '\u{AC00}'..='\u{D7AF}') => 5,
        Some(c) if c.is_alphanumeric() || c == '_' => 1,
        _ => 0,
    }
}

fn is_word(chars: &[char], i: usize) -> bool {
    word_class(chars, i) != 0
}

/// The start of the word before `i` (skipping spaces and punctuation first).
fn word_left(chars: &[char], mut i: usize) -> usize {
    while i > 0 && !is_word(chars, i - 1) {
        i -= 1;
    }
    let class = word_class(chars, i.saturating_sub(1));
    while i > 0 && word_class(chars, i - 1) == class && class != 0 {
        i -= 1;
    }
    i
}

/// The end of the word after `i` (skipping spaces and punctuation first).
fn word_right(chars: &[char], mut i: usize) -> usize {
    let n = chars.len();
    while i < n && !is_word(chars, i) {
        i += 1;
    }
    let class = word_class(chars, i);
    while i < n && word_class(chars, i) == class && class != 0 {
        i += 1;
    }
    i
}

/// A named key: Backspace, Delete, Left, Right, Up, Down, Home, End, Enter, Escape, SelectAll.
/// `word` moves or deletes by word, `line` to the line's ends (Up/Down, Home/End: the text's ends);
/// [`crate::keymap::text_motion`] gives them for the platform's keys. `ArrowLeft` etc. also work.
pub fn key(s: &mut Session, key: &str, shift: bool, word: bool, line: bool) -> Result<()> {
    let key = key.strip_prefix("Arrow").unwrap_or(key);
    let Some(te) = s.ds()?.text_edit.clone() else { return Err(EngineError::Other("not editing text".into())) };
    let t = text_of(s, te.id)?;
    let chars: Vec<char> = t.characters.chars().collect();
    let n = chars.len();
    if matches!(key, "Tab" | "Backspace" | "Enter") && !word && !line && list_key(s, key, shift)? {
        return Ok(());
    }
    match key {
        "Escape" => return end(s),
        "Enter" => return insert(s, "\n"),
        "Tab" => return insert(s, "\t"),
        "Backspace" | "Delete" => {
            return edit(s, |t, te| {
                let (a, b) = range(te);
                let (a, b) = if a != b {
                    (a, b)
                } else if key == "Backspace" {
                    let from = if line {
                        line_start(&chars, a)
                    } else if word {
                        word_left(&chars, a)
                    } else {
                        figcraft_text::backspace_step(&t.characters, a)
                    };
                    (from, a)
                } else {
                    let to = if line {
                        line_end(&chars, a)
                    } else if word {
                        word_right(&chars, a)
                    } else {
                        figcraft_text::grapheme_step(&t.characters, a, true).min(n)
                    };
                    (a, to)
                };
                t.replace_range(a, b, "");
                te.caret = a;
                te.anchor = a;
            });
        }
        _ => {}
    }
    // Caret movement.
    let layout = {
        let doc = s.doc()?;
        let node = doc.get(te.id)?;
        figcraft_text::layout_node(node).map(|(l, _)| l)
    };
    let (a, b) = range(&te);
    // Without Shift, a selection collapses first: plain arrows stop at its edge, word/line moves
    // continue from that edge.
    let cur = match key {
        "Left" | "Up" | "Home" if !shift && a != b => a,
        "Right" | "Down" | "End" if !shift && a != b => b,
        _ => te.caret,
    };
    let next = match key {
        "Left" if !shift && a != b && !word && !line => a,
        "Right" if !shift && a != b && !word && !line => b,
        "Left" if line => line_start(&chars, cur),
        "Right" if line => line_end(&chars, cur),
        "Left" if word => word_left(&chars, cur),
        "Right" if word => word_right(&chars, cur),
        // Plain arrows move on screen through right-to-left runs, by char elsewhere.
        "Left" | "Right" => match layout.as_ref().and_then(|l| l.visual_step(cur, key == "Right")) {
            Some(i) => i.min(n),
            None => figcraft_text::grapheme_step(&t.characters, cur, key == "Right").min(n),
        },
        "Home" if line => 0,
        "End" if line => n,
        "Home" => line_start(&chars, cur),
        "End" => line_end(&chars, cur),
        "Up" | "Down" => match &layout {
            _ if line => {
                if key == "Up" {
                    0
                } else {
                    n
                }
            }
            Some(l) => {
                let (x, li) = l.caret(cur);
                let target = if key == "Up" { li.checked_sub(1) } else { Some(li + 1).filter(|k| *k < l.lines.len()) };
                match target.and_then(|k| l.lines.get(k)) {
                    Some(line) => l.hit(x, line.top + line.height / 2.0),
                    None => {
                        if key == "Up" {
                            0
                        } else {
                            n
                        }
                    }
                }
            }
            None => cur,
        },
        "SelectAll" => return select(s, 0, n),
        _ => return Ok(()),
    };
    let anchor = if shift { te.anchor } else { next };
    select(s, anchor, next)
}

/// Set the selection (`anchor`..`caret`, clamped to the text). Moving the caret abandons any
/// composition in progress.
fn select(s: &mut Session, anchor: usize, caret: usize) -> Result<()> {
    let Some(id) = s.ds()?.text_edit.as_ref().map(|te| te.id) else { return Ok(()) };
    let len = s.doc()?.node(id).and_then(|n| n.kind.text()).map_or(0, TextData::char_count);
    let ds = s.ds_mut()?;
    if let Some(te) = ds.text_edit.as_mut() {
        te.anchor = anchor.min(len);
        te.caret = caret.min(len);
        te.preedit = None;
    }
    ds.revision += 1;
    Ok(())
}

/// Delete the text surrounding the caret/selection that an IME reports with
/// [`egui::ImeEvent::DeleteSurroundingBytes`]: `before_bytes` UTF-8 bytes end where the selection
/// starts and `after_bytes` start where it ends. As in stock `TextEdit`, the selected text itself
/// is kept for the paired `Commit`; only whole characters covered by the byte counts beyond the
/// selection are removed (a character the range only partly covers stays). Returns whether
/// anything was deleted.
pub fn delete_surrounding_bytes(s: &mut Session, before_bytes: usize, after_bytes: usize) -> Result<bool> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(false) };
    let t = text_of(s, te.id)?;
    let (a, b) = range(&te);
    let (before, after) = surrounding_chars(&t.characters, a, b, before_bytes, after_bytes);
    if before == 0 && after == 0 {
        return Ok(false);
    }
    edit_if(s, |t, te| {
        let (a, b) = range(te);
        let (before, after) = surrounding_chars(&t.characters, a, b, before_bytes, after_bytes);
        if before == 0 && after == 0 {
            return false;
        }
        // Delete after the selection first so the before range's indices stay valid.
        if after > 0 {
            t.replace_range(b, b + after, "");
        }
        if before > 0 {
            t.replace_range(a - before, a, "");
        }
        // Keep the selection: both ends move left by the deleted prefix, reversed or not.
        te.caret = te.caret.saturating_sub(before);
        te.anchor = te.anchor.saturating_sub(before);
        true
    })
}

/// Delete surrounding text counted in characters (the older `DeleteSurrounding` event). The
/// selection is kept, as in stock `TextEdit`; only text before and after it is removed.
pub fn delete_surrounding_chars(s: &mut Session, before_chars: usize, after_chars: usize) -> Result<bool> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(false) };
    let t = text_of(s, te.id)?;
    let (a, b) = range(&te);
    let (_, b_byte) = char_byte_offsets(&t.characters, a, b);
    let before = before_chars.min(a);
    let after = t.characters.get(b_byte..).map_or(0, |rest| rest.chars().take(after_chars).count());
    if before == 0 && after == 0 {
        return Ok(false);
    }
    edit_if(s, |t, te| {
        let (a, b) = range(te);
        let (_, b_byte) = char_byte_offsets(&t.characters, a, b);
        let before = before_chars.min(a);
        let after = t.characters.get(b_byte..).map_or(0, |rest| rest.chars().take(after_chars).count());
        if before == 0 && after == 0 {
            return false;
        }
        if after > 0 {
            t.replace_range(b, b + after, "");
        }
        if before > 0 {
            t.replace_range(a - before, a, "");
        }
        te.caret = te.caret.saturating_sub(before);
        te.anchor = te.anchor.saturating_sub(before);
        true
    })
}

/// The UTF-8 byte offsets of character indices `a` and `b` in one forward pass.
///
/// An index at or past the end maps to `text.len()`. Walking once (instead of twice) matters on
/// every IME event over a large text layer; the walk is still O(max(a, b)) characters, and callers
/// that need the end still pay O(n). See [`ime_surrounding`].
fn char_byte_offsets(text: &str, a: usize, b: usize) -> (usize, usize) {
    let (mut a_byte, mut b_byte) = (None, None);
    for (i, (byte, _)) in text.char_indices().enumerate() {
        if a_byte.is_none() && i == a {
            a_byte = Some(byte);
        }
        if b_byte.is_none() && i == b {
            b_byte = Some(byte);
        }
        if a_byte.is_some() && b_byte.is_some() {
            break;
        }
    }
    (a_byte.unwrap_or(text.len()), b_byte.unwrap_or(text.len()))
}

/// `(before, after)` characters covered by the IME's byte counts around the selection `[a, b)`
/// (character indices): mirror of egui's own `surrounding_bytes_to_chars`.
fn surrounding_chars(text: &str, a: usize, b: usize, before_bytes: usize, after_bytes: usize) -> (usize, usize) {
    let (a_byte, b_byte) = char_byte_offsets(text, a, b);
    let from = a_byte.saturating_sub(before_bytes);
    let before = text[..a_byte].char_indices().rev().take_while(|(i, _)| *i >= from).count();
    let after = text[b_byte..].char_indices().take_while(|(i, c)| i + c.len_utf8() <= after_bytes).count();
    (before, after)
}

/// The committed text around the IME caret for the outgoing surrounding-text snapshot.
///
/// This is the edited node's logical model text (never the bidi display order), with the
/// pre-edit excluded because a composition is UI-only in this editor until it is committed.
/// Character caret/anchor indices become independent UTF-8 byte offsets, so a reversed
/// selection stays reversed. At most [`MAX_IME_SURROUNDING_BYTES`] bytes are copied: the window
/// is centred on the selection and rounded inward to character boundaries. An empty text layer
/// is `Ok` with `text == ""` and both offsets zero; the result is [`ImeSurroundingUnavailable`]
/// only when there is no text edit, the target is not a text node, or the selection alone is
/// wider than the budget.
///
/// Cost: one character walk up to the later of the two cursor indices (to turn char indices into
/// UTF-8 bytes) plus the bounded copy. The caret is a character index, so reaching a caret near
/// the end of a very large layer is still O(n); the UI calls this once per pass and twice around
/// each text-affecting event (`ime_stamp`), so such a layer pays O(caret/anchor) per event. The
/// document is never cloned here.
pub fn ime_surrounding(s: &Session) -> std::result::Result<ImeSurrounding, ImeSurroundingUnavailable> {
    let ds = s.ds().map_err(|_| ImeSurroundingUnavailable::NotEditing)?;
    let te = ds.text_edit.as_ref().ok_or(ImeSurroundingUnavailable::NotEditing)?;
    let node = ds.doc.node(te.id).ok_or(ImeSurroundingUnavailable::NotText)?;
    let NodeKind::Text(t) = &node.kind else { return Err(ImeSurroundingUnavailable::NotText) };
    let text = t.characters.as_str();
    // One pass for both offsets (the caret and anchor are already clamped by `edit`/`select`).
    let (cursor, anchor) = char_byte_offsets(text, te.caret, te.anchor);
    let (selection_start, selection_end) = (cursor.min(anchor), cursor.max(anchor));
    let selection_len = selection_end - selection_start;
    if selection_len > MAX_IME_SURROUNDING_BYTES {
        return Err(ImeSurroundingUnavailable::SelectionTooWide);
    }
    // Spend the budget on both sides, giving room unused on one side to the other.
    let budget = MAX_IME_SURROUNDING_BYTES - selection_len;
    let before_available = selection_start;
    let after_available = text.len() - selection_end;
    let mut before = (budget / 2).min(before_available);
    let mut after = (budget - before).min(after_available);
    before = (budget - after).min(before_available);
    after = (budget - before).min(after_available);
    let mut window_start = selection_start - before;
    let mut window_end = selection_end + after;
    // Round inward so the window stays within the byte budget and on character boundaries.
    while window_start < window_end && !text.is_char_boundary(window_start) {
        window_start += 1;
    }
    while window_end > window_start && !text.is_char_boundary(window_end) {
        window_end -= 1;
    }
    let Some(window) = text.get(window_start..window_end) else {
        return Err(ImeSurroundingUnavailable::InvalidWindow);
    };
    Ok(ImeSurrounding {
        owner: ImeEditOwner { document: ds.uid, node: te.id, editing_session: te.editing_session },
        text: window.to_owned(),
        cursor: cursor.saturating_sub(window_start),
        anchor: anchor.saturating_sub(window_start),
    })
}

/// Show an IME composition inline at the caret (`cursor` = the IME's cursor inside it, in chars;
/// default its end). The document doesn't change until the composition is committed with
/// [`insert`]; an empty `text` cancels it. Starting to compose over a selection deletes it, as
/// typing would.
pub fn preedit(s: &mut Session, text: &str, cursor: Option<usize>) -> Result<()> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Err(EngineError::Other("not editing text".into())) };
    let text: String = text.chars().filter(|c| !c.is_control()).take(MAX_PREEDIT).collect();
    if text.is_empty() && te.preedit.is_none() {
        return Ok(()); // IMEs send empty compositions when idle
    }
    if !text.is_empty() && te.preedit.is_none() && te.caret != te.anchor {
        edit(s, |t, te| {
            let (a, b) = range(te);
            t.replace_range(a, b, "");
            te.caret = a;
            te.anchor = a;
        })?;
    }
    let n = text.chars().count();
    let ds = s.ds_mut()?;
    if let Some(te) = ds.text_edit.as_mut() {
        te.preedit = (n > 0).then(|| Preedit { text, cursor: cursor.unwrap_or(n).min(n) });
    }
    ds.revision += 1;
    Ok(())
}

/// Longest composition shown (IMEs compose a phrase at a time).
const MAX_PREEDIT: usize = 1000;

/// The document as it looks while composing: the composition inserted at the caret, with text
/// auto-size and layout applied. Also returns the composition's char range and the IME cursor's
/// index in that document. `None` when nothing is being composed.
pub fn preedit_doc(s: &Session) -> Option<(Document, (usize, usize), usize)> {
    let ds = s.ds().ok()?;
    let te = ds.text_edit.as_ref()?;
    let pe = te.preedit.as_ref()?;
    let mut doc = (*ds.doc).clone();
    let NodeKind::Text(t) = &mut doc.get_mut(te.id).ok()?.kind else { return None };
    let at = te.caret.min(t.char_count());
    t.replace_range(at, at, &pe.text);
    crate::normalize(&mut doc, &ds.doc);
    let n = pe.text.chars().count();
    Some((doc, (at, at + n), at + pe.cursor.min(n)))
}

fn line_start(chars: &[char], mut i: usize) -> usize {
    while i > 0 && chars.get(i - 1).is_some_and(|c| *c != '\n') {
        i -= 1;
    }
    i
}

fn line_end(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars.get(i).is_some_and(|c| *c != '\n') {
        i += 1;
    }
    i
}

/// Place the caret (and optionally extend the selection) at char `index`.
pub fn set_caret(s: &mut Session, index: usize, extend: bool) -> Result<()> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(()) };
    select(s, if extend { te.anchor } else { index }, index)
}

/// Select the word at `index` (double-click).
pub fn select_word(s: &mut Session, index: usize) -> Result<()> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(()) };
    let chars: Vec<char> = text_of(s, te.id)?.characters.chars().collect();
    let mut a = index.min(chars.len());
    // Double-clicking just after a word's last char (or at the end) selects that word.
    if a > 0 && !is_word(&chars, a) && is_word(&chars, a - 1) {
        a -= 1;
    }
    let mut b = a;
    // Words by script (see `word_class`), then runs of spaces (10) and of punctuation (11).
    let class = |i: usize| match word_class(&chars, i) {
        0 if chars.get(i).is_some_and(|c| c.is_whitespace()) => 10,
        0 => 11,
        k => k,
    };
    // A word, a run of spaces or a run of punctuation, like the incumbent; newlines stay unselected.
    if let Some(k) = chars.get(a).filter(|c| **c != '\n').map(|_| class(a)) {
        while a > 0 && chars.get(a - 1).is_some_and(|c| *c != '\n') && class(a - 1) == k {
            a -= 1;
        }
        while b < chars.len() && chars.get(b).is_some_and(|c| *c != '\n') && class(b) == k {
            b += 1;
        }
    }
    select(s, a, b)
}

/// Select the line (paragraph) containing `index` (triple-click).
pub fn select_line(s: &mut Session, index: usize) -> Result<()> {
    let Some(te) = s.ds()?.text_edit.clone() else { return Ok(()) };
    let chars: Vec<char> = text_of(s, te.id)?.characters.chars().collect();
    let i = index.min(chars.len());
    select(s, line_start(&chars, i), line_end(&chars, i))
}

/// The selected text while editing (for copy).
pub fn selected_text(s: &Session) -> Option<String> {
    let (id, a, b) = selected_range(s)?;
    let doc = s.doc().ok()?;
    let t = doc.node(id)?.kind.text()?;
    Some(t.characters.chars().skip(a).take(b.saturating_sub(a)).collect())
}

/// The selected range while editing (for styling commands), or None.
pub fn selected_range(s: &Session) -> Option<(NodeId, usize, usize)> {
    let te = s.ds().ok()?.text_edit.clone()?;
    let (a, b) = range(&te);
    Some((te.id, a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{PointerEvent, PointerKind};
    use figcraft_doc::{ListStyle, TextAutoResize, TextCase, TextDecoration};
    use serde_json::json;

    fn setup(text: &str) -> (Session, NodeId) {
        let mut s = Session::new();
        let r = s.execute("text.create", &json!({"text": text, "x": 0, "y": 0, "props": {"fontSize": 24}})).unwrap();
        let id = crate::params::id_of(&r["id"]).unwrap();
        (s, id)
    }

    fn data(s: &Session, id: NodeId) -> TextData {
        s.doc().unwrap().node(id).unwrap().kind.text().unwrap().clone()
    }

    fn sel(s: &Session) -> (usize, usize) {
        let te = s.ds().unwrap().text_edit.clone().unwrap();
        (te.anchor, te.caret)
    }

    #[test]
    fn whole_editing_session_is_one_undo_step() {
        let (mut s, id) = setup("Hello");
        let undo_before = s.ds().unwrap().history.undo.len();
        begin(&mut s, id, Some(5), false).unwrap();
        insert(&mut s, " big").unwrap();
        insert(&mut s, " world").unwrap();
        key(&mut s, "Backspace", false, false, false).unwrap();
        // Styling a range mid-session joins the same step.
        set_caret(&mut s, 0, false).unwrap();
        set_caret(&mut s, 5, true).unwrap();
        s.execute("text.bold", &json!({})).unwrap();
        s.execute("text.alignCenter", &json!({})).unwrap();
        end(&mut s).unwrap();
        let t = data(&s, id);
        assert_eq!(t.characters, "Hello big worl");
        assert_eq!(t.style_at(0).font_style, "Bold");
        assert_eq!(t.style_at(6).font_style, "Regular");
        assert_eq!(s.ds().unwrap().history.undo.len(), undo_before + 1);
        s.undo().unwrap();
        let t = data(&s, id);
        assert_eq!(t.characters, "Hello");
        assert!(t.runs.is_empty());
        assert_eq!(t.align_h, figcraft_doc::TextAlignH::Left);
    }

    #[test]
    fn typing_replaces_selection_and_paste_is_plain() {
        let (mut s, id) = setup("Hello world");
        begin(&mut s, id, None, false).unwrap();
        assert_eq!(sel(&s), (0, 11), "begin without caret selects all");
        select_word(&mut s, 7).unwrap();
        assert_eq!(sel(&s), (6, 11));
        insert(&mut s, "there").unwrap();
        assert_eq!(data(&s, id).characters, "Hello there");
        assert_eq!(sel(&s), (11, 11));
        // Pasted text: CRLF / CR / Unicode separators become newlines, other controls are dropped.
        insert(&mut s, "\r\na\rb\u{2029}c\u{0}\u{7}\td").unwrap();
        assert_eq!(data(&s, id).characters, "Hello there\na\nb\nc\td");
        insert(&mut s, "\u{1}").unwrap();
        assert_eq!(data(&s, id).characters, "Hello there\na\nb\nc\td");
        // Select the line (paragraph) then type over it.
        select_line(&mut s, 0).unwrap();
        assert_eq!(sel(&s), (0, 11));
        insert(&mut s, "Hi").unwrap();
        assert_eq!(data(&s, id).characters, "Hi\na\nb\nc\td");
        assert_eq!(selected_text(&s).as_deref(), Some(""));
        end(&mut s).unwrap();
    }

    #[test]
    fn word_selection_classes() {
        let (mut s, id) = setup("foo,  bar_baz!\nnext");
        begin(&mut s, id, Some(0), false).unwrap();
        select_word(&mut s, 1).unwrap();
        assert_eq!(sel(&s), (0, 3));
        // Right after a word's last char selects the word.
        select_word(&mut s, 13).unwrap();
        assert_eq!(sel(&s), (6, 13));
        // A run of spaces.
        select_word(&mut s, 4).unwrap();
        assert_eq!(sel(&s), (4, 6));
        select_line(&mut s, 16).unwrap();
        assert_eq!(sel(&s), (15, 19));
        select_line(&mut s, 999).unwrap();
        assert_eq!(sel(&s), (15, 19));
        select_word(&mut s, 999).unwrap();
        assert_eq!(sel(&s), (15, 19));
        set_caret(&mut s, 999, false).unwrap();
        assert_eq!(sel(&s), (19, 19), "carets clamp to the text");
    }

    #[test]
    fn keyboard_navigation_and_extension() {
        let (mut s, id) = setup("one two\nthree");
        begin(&mut s, id, Some(0), false).unwrap();
        key(&mut s, "Right", false, true, false).unwrap(); // Alt+Right: end of word
        assert_eq!(sel(&s), (3, 3));
        key(&mut s, "Right", true, true, false).unwrap(); // Shift+Alt+Right extends
        assert_eq!(sel(&s), (3, 7));
        key(&mut s, "Left", false, false, false).unwrap(); // collapses to the start
        assert_eq!(sel(&s), (3, 3));
        key(&mut s, "Right", false, false, true).unwrap(); // Cmd+Right: line end
        assert_eq!(sel(&s), (7, 7));
        key(&mut s, "Down", true, false, true).unwrap(); // Shift+Cmd+Down: to the end
        assert_eq!(sel(&s), (7, 13));
        key(&mut s, "Left", false, false, true).unwrap(); // Cmd+Left from the collapsed start
        assert_eq!(sel(&s), (0, 0));
        key(&mut s, "Delete", false, true, false).unwrap(); // Alt+Delete: word forward
        assert_eq!(data(&s, id).characters, " two\nthree");
        key(&mut s, "SelectAll", false, false, false).unwrap();
        assert_eq!(sel(&s), (0, 10));
        end(&mut s).unwrap();
    }

    #[test]
    fn backspace_removes_only_the_final_thai_mark() {
        let (mut s, id) = setup("น้ำ");
        begin(&mut s, id, Some(3), false).unwrap();

        key(&mut s, "Backspace", false, false, false).unwrap();

        assert_eq!(data(&s, id).characters, "น้");
    }

    fn ev(kind: PointerKind, x: f64, y: f64, shift: bool) -> PointerEvent {
        let mut e = PointerEvent::at(kind, x, y, 1.0);
        e.shift = shift;
        e
    }

    /// Asserts double-clicking a text layer selects all text, a second double-click selects the
    /// word without changing the editing session, and drag and Shift+click extend the
    /// selection.
    #[test]
    fn pointer_double_click_drag_and_shift_click() {
        let (mut s, id) = setup("Hello world");
        let Some((l, _)) = figcraft_text::layout_node(s.doc().unwrap().node(id).unwrap()) else { return };
        if l.glyphs.is_empty() {
            return; // no fonts
        }
        let y = l.lines[0].top + l.lines[0].height / 2.0;
        let x_of = |i: usize| l.caret(i).0;
        // Double-click inside "world" edits with all the text selected; another one there
        // selects the word.
        let mid = (x_of(7) + x_of(8)) / 2.0;
        s.pointer(&ev(PointerKind::Down, mid, y, false)).unwrap();
        s.pointer(&ev(PointerKind::Up, mid, y, false)).unwrap();
        s.pointer(&ev(PointerKind::DoubleClick, mid, y, false)).unwrap();
        assert_eq!(sel(&s), (0, 11));
        let epoch = s.ds().unwrap().text_edit.as_ref().unwrap().editing_session;
        s.pointer(&ev(PointerKind::Down, mid, y, false)).unwrap();
        s.pointer(&ev(PointerKind::DoubleClick, mid, y, false)).unwrap();
        s.pointer(&ev(PointerKind::Up, mid, y, false)).unwrap();
        assert_eq!(sel(&s), (6, 11));
        assert_eq!(
            s.ds().unwrap().text_edit.as_ref().unwrap().editing_session,
            epoch,
            "a double-click inside the edit is an ordinary edit: same IME replacement epoch"
        );
        // Drag-select from the start of "Hello" to the middle.
        s.pointer(&ev(PointerKind::Down, x_of(0) + 0.5, y, false)).unwrap();
        assert_eq!(sel(&s), (0, 0));
        s.pointer(&ev(PointerKind::Move, (x_of(3) + x_of(4)) / 2.0 - 1.0, y, false)).unwrap();
        s.pointer(&ev(PointerKind::Up, (x_of(3) + x_of(4)) / 2.0 - 1.0, y, false)).unwrap();
        assert_eq!(sel(&s), (0, 3));
        // Shift+click extends.
        s.pointer(&ev(PointerKind::Down, x_of(9) + 0.5, y, true)).unwrap();
        s.pointer(&ev(PointerKind::Up, x_of(9) + 0.5, y, true)).unwrap();
        assert_eq!(sel(&s), (0, 9));
        insert(&mut s, "J").unwrap();
        assert_eq!(data(&s, id).characters, "Jld");
    }

    #[test]
    fn text_commands_apply_to_edited_range() {
        let (mut s, id) = setup("Hello world");
        begin(&mut s, id, Some(0), false).unwrap();
        set_caret(&mut s, 6, true).unwrap(); // "Hello "
        for (cmd, p) in [
            ("text.underline", json!({})),
            ("text.italic", json!({})),
            ("text.sizeUp", json!({})),
            ("text.trackingUp", json!({})),
            ("text.lineHeightUp", json!({})),
            ("text.caseSmallCaps", json!({})),
            ("text.link", json!({"url": "https://example.com"})),
        ] {
            s.execute(cmd, &p).unwrap_or_else(|e| panic!("{cmd}: {e}"));
        }
        let t = data(&s, id);
        let (a, b) = (t.style_at(0), t.style_at(8));
        assert_eq!(a.decoration, TextDecoration::Underline);
        assert!(a.italic() && !b.italic());
        assert_eq!(a.font_size, 25.0);
        assert_eq!(b.font_size, 24.0);
        assert_eq!(a.case, TextCase::SmallCaps);
        assert_eq!(a.link.as_deref(), Some("https://example.com"));
        assert!(b.link.is_none());
        // text.style with an explicit range (what the panel sends).
        s.execute("text.style", &json!({"id": id, "start": 6, "end": 11, "fontSize": 40, "textCase": "upper"})).unwrap();
        let t = data(&s, id);
        assert_eq!(t.style_at(8).font_size, 40.0);
        assert_eq!(t.style_at(8).case, TextCase::Upper);
        // Paragraph commands hit the edited node.
        s.execute("text.listStyle", &json!({"style": "numbered"})).unwrap();
        assert_eq!(data(&s, id).list, ListStyle::Numbered);
        // Every Text-menu command runs while editing.
        for item in crate::catalog::MENU.iter().filter(|i| i.id.starts_with("text.")) {
            let p = if item.id == "text.link" { json!({"url": "u"}) } else { json!({}) };
            if crate::find_command(item.id).is_some() {
                s.execute(item.id, &p).unwrap_or_else(|e| panic!("{}: {e}", item.id));
            }
        }
        end(&mut s).unwrap();
    }

    #[test]
    fn paragraph_and_feature_commands_on_nodes() {
        let (mut s, id) = setup("Hello");
        let ids = json!([id]);
        s.execute("text.setFeatures", &json!({"ids": ids, "features": {"liga": false, "tnum": true}})).unwrap();
        assert_eq!(data(&s, id).features.get("liga"), Some(&false));
        s.execute("text.setFeatures", &json!({"ids": ids, "features": {"liga": null, "ss01": 1}})).unwrap();
        let f = data(&s, id).features;
        assert_eq!(f.get("liga"), None);
        assert_eq!(f.get("ss01"), Some(&true));
        assert_eq!(f.get("tnum"), Some(&true));
        s.execute("text.setFeatures", &json!({"ids": ids, "features": {"zero": true}, "replace": true})).unwrap();
        assert_eq!(data(&s, id).features.len(), 1);
        assert!(s.execute("text.setFeatures", &json!({"ids": ids, "features": {"ligatures": true}})).is_err());
        assert!(s.execute("text.setFeatures", &json!({"ids": ids, "features": {"liga": "yes"}})).is_err());
        assert!(s.execute("text.setFeatures", &json!({"ids": ids})).is_err());
        s.execute("node.set", &json!({"id": id, "props": {"features": {"calt": false}}})).unwrap();
        assert_eq!(data(&s, id).features.len(), 1);
        s.execute("text.paragraphIndent", &json!({"ids": ids, "indent": 16})).unwrap();
        s.execute("text.paragraphSpacing", &json!({"ids": ids, "spacing": 8})).unwrap();
        s.execute("text.maxLines", &json!({"ids": ids, "lines": 2})).unwrap();
        s.execute("text.decoration", &json!({"ids": ids, "decoration": "strikethrough"})).unwrap();
        s.execute("text.case", &json!({"ids": ids, "case": "smallCaps"})).unwrap();
        s.execute("text.listStyle", &json!({"ids": ids, "style": "bulleted"})).unwrap();
        let t = data(&s, id);
        assert_eq!((t.paragraph_indent, t.paragraph_spacing, t.max_lines), (16.0, 8.0, 2));
        assert_eq!(t.auto_resize, TextAutoResize::Truncate);
        assert_eq!(t.style.decoration, TextDecoration::Strikethrough);
        assert_eq!(t.style.case, TextCase::SmallCaps);
        assert_eq!(t.list, ListStyle::Bulleted);
        assert!(s.execute("text.listStyle", &json!({"ids": ids, "style": "zigzag"})).is_err());
        assert!(s.execute("text.paragraphIndent", &json!({"ids": ids, "indent": "NaN"})).is_err());
        s.execute("node.set", &json!({"id": id, "props": {"textCase": "smallCaps", "paragraphIndent": 4, "maxLines": 0}})).unwrap();
        assert_eq!(data(&s, id).paragraph_indent, 4.0);
    }

    fn preedit_state(s: &Session) -> Option<Preedit> {
        s.ds().unwrap().text_edit.clone().unwrap().preedit
    }

    #[test]
    fn ime_composition_is_shown_but_not_in_the_document_until_committed() {
        let (mut s, id) = setup("Hello world");
        let undo_before = s.ds().unwrap().history.undo.len();
        begin(&mut s, id, Some(5), false).unwrap();
        let doc_before = s.ds().unwrap().doc.clone();
        s.execute("text.edit.preedit", &json!({"text": "にほ", "cursor": 1})).unwrap();
        assert_eq!(preedit_state(&s), Some(Preedit { text: "にほ".into(), cursor: 1 }));
        assert!(std::sync::Arc::ptr_eq(&doc_before, &s.ds().unwrap().doc), "composing leaves the document alone");
        let (shown, range, cursor) = preedit_doc(&s).unwrap();
        assert_eq!(shown.node(id).unwrap().kind.text().unwrap().characters, "Helloにほ world");
        assert_eq!((range, cursor), ((5, 7), 6));
        let info = s.execute("text.edit.selection", &json!({})).unwrap();
        assert_eq!(info["preedit"]["text"], "にほ");
        // Updating the composition replaces it; an empty one cancels it.
        preedit(&mut s, "にほん", None).unwrap();
        assert_eq!(preedit_doc(&s).unwrap().2, 8);
        preedit(&mut s, "", None).unwrap();
        assert!(preedit_state(&s).is_none() && preedit_doc(&s).is_none());
        assert_eq!(data(&s, id).characters, "Hello world");
        // Commit puts the final text at the caret.
        preedit(&mut s, "にほん", None).unwrap();
        s.execute("text.edit.insert", &json!({"text": "日本"})).unwrap();
        assert!(preedit_state(&s).is_none());
        assert_eq!(data(&s, id).characters, "Hello日本 world");
        assert_eq!(sel(&s), (7, 7));
        // Composing over a selection replaces it; moving the caret abandons the composition.
        set_caret(&mut s, 7, false).unwrap();
        set_caret(&mut s, 13, true).unwrap();
        preedit(&mut s, "x", None).unwrap();
        assert_eq!(data(&s, id).characters, "Hello日本");
        set_caret(&mut s, 0, false).unwrap();
        assert!(preedit_state(&s).is_none());
        assert_eq!(data(&s, id).characters, "Hello日本");
        // Ending the edit drops an uncommitted composition; the session stays one undo step.
        preedit(&mut s, "zz", None).unwrap();
        end(&mut s).unwrap();
        assert_eq!(data(&s, id).characters, "Hello日本");
        assert_eq!(s.ds().unwrap().history.undo.len(), undo_before + 1);
        // Not editing: an error, not a crash.
        assert!(preedit(&mut s, "a", None).is_err());
    }

    #[test]
    fn variable_font_axes_as_commands_and_props() {
        let (mut s, id) = setup("Hello world");
        let ids = json!([id]);
        s.execute("text.setVariations", &json!({"ids": ids, "variations": {"wght": 550, "wdth": 85}})).unwrap();
        let st = data(&s, id).style;
        assert_eq!((st.variations.get("wght"), st.variations.get("wdth")), (Some(&550.0), Some(&85.0)));
        assert_eq!(st.weight(), 550);
        // null removes one axis; replace clears the rest.
        s.execute("text.setVariations", &json!({"ids": ids, "variations": {"wdth": null}})).unwrap();
        assert_eq!(data(&s, id).style.variations.len(), 1);
        s.execute("text.setVariations", &json!({"ids": ids, "variations": {"opsz": 14}, "replace": true})).unwrap();
        assert_eq!(data(&s, id).style.variations.keys().collect::<Vec<_>>(), ["opsz"]);
        // Undo restores the previous values.
        s.undo().unwrap();
        assert_eq!(data(&s, id).style.variations.keys().collect::<Vec<_>>(), ["wght"]);
        // Junk is rejected.
        assert!(s.execute("text.setVariations", &json!({"ids": ids, "variations": {"weight": 500}})).is_err());
        assert!(s.execute("text.setVariations", &json!({"ids": ids, "variations": {"wght": "bold"}})).is_err());
        assert!(s.execute("text.setVariations", &json!({"ids": ids})).is_err());
        // node.set props (whole text or a range) replace the custom values.
        s.execute("node.set", &json!({"id": id, "props": {"fontVariations": {"wght": 300}, "start": 6, "end": 11}})).unwrap();
        let t = data(&s, id);
        assert_eq!(t.style_at(0).variations.get("wght"), Some(&550.0));
        assert_eq!(t.style_at(7).variations.get("wght"), Some(&300.0));
        s.execute("node.set", &json!({"id": id, "props": {"variationSettings": null}})).unwrap();
        assert!(data(&s, id).runs.is_empty() && data(&s, id).style.variations.is_empty());
        // Picking a named style (Bold, weight steps) drops the custom weight but keeps width.
        s.execute("text.setVariations", &json!({"ids": ids, "variations": {"wght": 650, "wdth": 90}})).unwrap();
        s.execute("text.bold", &json!({"ids": ids})).unwrap();
        let st = data(&s, id).style;
        assert_eq!(st.font_style, "Bold", "650 is lighter than bold, so Bold applies");
        assert!(!st.variations.contains_key("wght"));
        assert_eq!(st.variations.get("wdth"), Some(&90.0));
        // While editing, the command styles the selected range only.
        begin(&mut s, id, Some(0), false).unwrap();
        set_caret(&mut s, 5, true).unwrap();
        s.execute("text.setVariations", &json!({"variations": {"wdth": 75}})).unwrap();
        end(&mut s).unwrap();
        let t = data(&s, id);
        assert_eq!(t.style_at(0).variations.get("wdth"), Some(&75.0));
        assert_eq!(t.style_at(8).variations.get("wdth"), Some(&90.0));
        // The axes query answers without fonts too.
        let r = s.execute("text.fonts", &json!({"family": "Inter", "axes": true})).unwrap();
        assert!(r.is_null() || r["axes"].is_array());
    }

    /// No text command panics on junk parameters while a text is being edited.
    #[test]
    fn text_commands_survive_junk_while_editing() {
        let junk = [
            json!({}),
            json!(null),
            json!({"index": -5, "features": {"": 1}}),
            json!({"index": 1e300, "style": 3, "case": null}),
            json!([1]),
            json!({"variations": {"wght": "heavy", "toolong": 1}, "text": "\u{0}\u{7}", "cursor": 1e300}),
            json!({"variations": {"wght": 1e300, "opsz": -1e300}, "replace": "yes", "text": "日本語", "cursor": -1}),
        ];
        // `text.rescanFonts` takes no params and walks the font folders: one walk per junk is slow.
        for spec in crate::cmd::all_commands().iter().filter(|c| c.id.starts_with("text.") && c.id != "text.rescanFonts") {
            for j in &junk {
                let (mut s, id) = setup("Hi there\nyou");
                begin(&mut s, id, Some(2), false).unwrap();
                set_caret(&mut s, 5, true).unwrap();
                if let Err(EngineError::Internal { cmd, msg }) = s.execute(spec.id, j) {
                    panic!("{cmd} panicked on {j}: {msg}");
                }
            }
        }
    }

    /// Asserts surrounding text is logical UTF-8 text with byte offsets and keeps a reversed
    /// selection reversed.
    #[test]
    fn ime_surrounding_uses_logical_utf8_bytes_and_keeps_selection_direction() {
        let (mut s, id) = setup("café😀e\u{301}x");
        begin(&mut s, id, Some(8), false).unwrap();
        set_caret(&mut s, 8, false).unwrap();
        set_caret(&mut s, 5, true).unwrap(); // anchor after the emoji, caret before it
        let ime = ime_surrounding(&s).unwrap();
        assert_eq!(ime.text, "café😀e\u{301}x");
        assert_eq!((ime.cursor, ime.anchor), (9, 13), "char 5 and char 8 as UTF-8 bytes");
        assert!(ime.cursor < ime.anchor, "the reversed selection stays reversed");
        assert_eq!(ime.owner.node, id);
        assert_eq!(ime.owner.document, s.ds().unwrap().uid);
    }

    /// Asserts an active composition is excluded from surrounding text and an empty text layer
    /// reports an available, empty payload.
    #[test]
    fn ime_surrounding_excludes_the_composition_and_empty_text_is_available() {
        let (mut s, id) = setup("Hi");
        begin(&mut s, id, Some(2), false).unwrap();
        preedit(&mut s, "にほ", Some(1)).unwrap();
        let ime = ime_surrounding(&s).unwrap();
        assert_eq!(ime.text, "Hi", "the composition is not committed text");
        assert_eq!((ime.cursor, ime.anchor), (2, 2));
        end(&mut s).unwrap();

        let (mut s, id) = setup("");
        begin(&mut s, id, Some(0), false).unwrap();
        let ime = ime_surrounding(&s).unwrap();
        assert_eq!((ime.text.as_str(), ime.cursor, ime.anchor), ("", 0, 0), "an empty layer is available, not unavailable");
    }

    /// Asserts surrounding text is capped at `MAX_IME_SURROUNDING_BYTES`, an exactly budget-sized
    /// selection is describable, and one byte more is `SelectionTooWide`.
    #[test]
    fn ime_surrounding_is_bounded_and_a_too_wide_selection_is_explicitly_unavailable() {
        let long = "a".repeat(5000);
        let (mut s, id) = setup(&long);
        begin(&mut s, id, Some(5000), false).unwrap();
        let ime = ime_surrounding(&s).unwrap();
        assert_eq!(ime.text.len(), MAX_IME_SURROUNDING_BYTES);
        assert_eq!((ime.cursor, ime.anchor), (MAX_IME_SURROUNDING_BYTES, MAX_IME_SURROUNDING_BYTES));

        // Exactly the budget is still describable; one more byte is not.
        set_caret(&mut s, 1000, false).unwrap();
        set_caret(&mut s, 5000, true).unwrap();
        let ime = ime_surrounding(&s).unwrap();
        assert_eq!(ime.text.len(), 4000);
        assert_eq!((ime.cursor, ime.anchor), (4000, 0));
        set_caret(&mut s, 999, false).unwrap();
        set_caret(&mut s, 5000, true).unwrap();
        assert_eq!(ime_surrounding(&s), Err(ImeSurroundingUnavailable::SelectionTooWide));
    }

    /// Asserts edits and caret moves keep the same IME owner, while ending and beginning a new
    /// edit session changes the generation.
    #[test]
    fn ime_owner_is_stable_through_edits_and_the_session_changes_on_begin() {
        let (mut s, id) = setup("Hello");
        begin(&mut s, id, Some(0), false).unwrap();
        let first = ime_surrounding(&s).unwrap().owner;
        insert(&mut s, "x").unwrap();
        set_caret(&mut s, 2, false).unwrap();
        assert_eq!(ime_surrounding(&s).unwrap().owner, first, "edits and caret moves keep the owner");
        end(&mut s).unwrap();
        begin(&mut s, id, Some(0), false).unwrap();
        let replaced = ime_surrounding(&s).unwrap().owner;
        assert_eq!(replaced.node, id);
        assert_ne!(replaced.editing_session, first.editing_session, "a new edit session is a replacement epoch");
    }

    /// Asserts byte deletion around a reversed selection removes whole characters on both
    /// sides, keeps the selected text and direction, and a final no-op leaves the text alone.
    #[test]
    fn ime_delete_surrounding_bytes_keeps_the_reversed_selection_and_deletes_around_it() {
        let (mut s, id) = setup("ab你好cd");
        begin(&mut s, id, Some(4), false).unwrap();
        set_caret(&mut s, 4, false).unwrap();
        set_caret(&mut s, 2, true).unwrap(); // reversed: caret before 你, anchor after 好
        assert!(delete_surrounding_bytes(&mut s, 1, 1).unwrap());
        // "b" (whole character before the selection) and "c" (whole character after it) go; the
        // selected "你好" stays for the paired commit, and the reversed direction is preserved.
        assert_eq!(data(&s, id).characters, "a你好d");
        assert_eq!(sel(&s), (3, 1)); // (anchor, caret), still reversed
        assert!(delete_surrounding_bytes(&mut s, 1, 1).unwrap());
        assert_eq!(data(&s, id).characters, "你好");
        assert_eq!(sel(&s), (2, 0)); // (anchor, caret), still reversed
        assert!(!delete_surrounding_bytes(&mut s, 1, 1).unwrap(), "nothing is left around the selection");
        assert_eq!(data(&s, id).characters, "你好", "a no-op delete does not touch the text");
    }

    /// Asserts character deletion keeps and shifts the selection, and a no-op delete leaves an
    /// active composition and the revision untouched.
    #[test]
    fn ime_delete_surrounding_chars_keeps_the_selection_and_ignores_a_no_op() {
        let (mut s, id) = setup("ab你好cd");
        begin(&mut s, id, Some(2), false).unwrap();
        set_caret(&mut s, 4, true).unwrap(); // "你好"
        assert!(delete_surrounding_chars(&mut s, 1, 1).unwrap());
        assert_eq!(data(&s, id).characters, "a你好d");
        assert_eq!(sel(&s), (1, 3), "the selection is kept, shifted by the deleted prefix");
        set_caret(&mut s, 0, false).unwrap();
        // A composition in progress survives a delete that finds nothing to remove.
        preedit(&mut s, "x", None).unwrap();
        let revision = s.ds().unwrap().revision;
        assert!(!delete_surrounding_chars(&mut s, 1, 0).unwrap());
        assert_eq!(s.ds().unwrap().text_edit.as_ref().and_then(|te| te.preedit.as_ref()).map(|p| p.text.as_str()), Some("x"));
        assert_eq!(s.ds().unwrap().revision, revision, "a no-op does not bump the revision");
        assert!(!delete_surrounding_bytes(&mut s, 1, 0).unwrap());
        assert_eq!(s.ds().unwrap().text_edit.as_ref().and_then(|te| te.preedit.as_ref()).map(|p| p.text.as_str()), Some("x"));
        assert_eq!(s.ds().unwrap().revision, revision);
    }

    /// Asserts a text layer in another document has a different IME owner.
    #[test]
    fn ime_owner_identity_changes_with_the_document() {
        let (mut s, id) = setup("Hi");
        begin(&mut s, id, Some(2), false).unwrap();
        let first = ime_surrounding(&s).unwrap().owner;
        end(&mut s).unwrap();
        s.execute("file.new", &json!({"name": "Second"})).unwrap();
        let r = s.execute("text.create", &json!({"text": "There", "x": 0, "y": 0})).unwrap();
        let other = crate::params::id_of(&r["id"]).unwrap();
        begin(&mut s, other, Some(5), false).unwrap();
        let second = ime_surrounding(&s).unwrap().owner;
        assert_ne!(second.document, first.document, "another document is another IME owner");
        assert_eq!(second.node, other);
    }

    /// Asserts a space committed after a line-start `-` becomes a bulleted list and reports
    /// `Transformed`; a plain `"x"` commit reports `Plain`, and an empty commit reports `Nothing`.
    #[test]
    fn ime_commit_reports_list_autoformat_as_an_application_transformation() {
        let (mut s, id) = setup("-");
        begin(&mut s, id, Some(1), false).unwrap();
        assert_eq!(insert_ime_commit(&mut s, " ").unwrap(), ImeInsertion::Transformed);
        assert_eq!(data(&s, id).characters, "");
        assert_eq!(data(&s, id).para_list(0).list, ListStyle::Bulleted);
        assert_eq!(insert_ime_commit(&mut s, "x").unwrap(), ImeInsertion::Plain);
        assert_eq!(insert_ime_commit(&mut s, "").unwrap(), ImeInsertion::Nothing);
    }

    /// Asserts CRLF normalization reports `Transformed`, control-only input reports `Nothing`,
    /// and bare newline/CR commits are ignored while a live composition remains.
    #[test]
    fn ime_commit_normalisation_is_a_transformation_and_a_bare_newline_is_ignored() {
        let (mut s, id) = setup("Hi");
        begin(&mut s, id, Some(2), false).unwrap();
        // CRLF is inserted as LF, but the editor changed the commit: application-origin.
        assert_eq!(insert_ime_commit(&mut s, "a\r\nb").unwrap(), ImeInsertion::Transformed);
        assert_eq!(data(&s, id).characters, "Hia\nb");
        // Control characters are dropped; the commit itself is a no-op.
        assert_eq!(insert_ime_commit(&mut s, "\u{0}\u{7}").unwrap(), ImeInsertion::Nothing);
        assert_eq!(data(&s, id).characters, "Hia\nb");
        // Stock `TextEdit` ignores a bare newline/CR commit; typing Enter still inserts one.
        // (It also leaves a live composition alone, unlike the non-ignored commit below.)
        preedit(&mut s, "x", None).unwrap();
        assert_eq!(insert_ime_commit(&mut s, "\n").unwrap(), ImeInsertion::Nothing);
        assert_eq!(insert_ime_commit(&mut s, "\r").unwrap(), ImeInsertion::Nothing);
        assert_eq!(preedit_state(&s).map(|p| p.text), Some("x".into()));
        preedit(&mut s, "", None).unwrap();
        assert_eq!(data(&s, id).characters, "Hia\nb");
        insert(&mut s, "\n").unwrap();
        assert_eq!(data(&s, id).characters, "Hia\nb\n");
    }

    /// Asserts an empty or control-only IME commit ends a live composition (bumping the revision
    /// once), inserts nothing, and is a harmless no-op when no composition is active.
    #[test]
    fn an_empty_ime_commit_ends_the_composition_without_inserting_text() {
        let (mut s, id) = setup("Hello");
        begin(&mut s, id, Some(5), false).unwrap();
        preedit(&mut s, "にほ", None).unwrap();
        assert_eq!(preedit_state(&s).map(|p| p.text), Some("にほ".into()));
        let revision = s.ds().unwrap().revision;
        assert_eq!(insert_ime_commit(&mut s, "").unwrap(), ImeInsertion::Nothing);
        assert_eq!(s.ds().unwrap().revision, revision + 1, "ending a live composition bumps the revision");
        assert!(preedit_state(&s).is_none(), "an empty commit ends the composition");
        assert!(preedit_doc(&s).is_none());
        assert_eq!(data(&s, id).characters, "Hello", "and inserts nothing");

        // A control-only commit normalises to empty and ends the composition too.
        preedit(&mut s, "x", None).unwrap();
        let revision = s.ds().unwrap().revision;
        assert_eq!(insert_ime_commit(&mut s, "\u{0}\u{7}").unwrap(), ImeInsertion::Nothing);
        assert_eq!(s.ds().unwrap().revision, revision + 1, "clearing a live composition bumps the revision");
        assert!(preedit_state(&s).is_none());
        assert_eq!(data(&s, id).characters, "Hello");

        // With no composition (and not even while editing) an empty commit stays a harmless no-op.
        let revision = s.ds().unwrap().revision;
        assert_eq!(insert_ime_commit(&mut s, "").unwrap(), ImeInsertion::Nothing);
        assert_eq!(s.ds().unwrap().revision, revision);
        end(&mut s).unwrap();
        assert_eq!(insert_ime_commit(&mut s, "").unwrap(), ImeInsertion::Nothing);
    }
}
