//! Ctrl+K command palette: jump to pages and settings sections, or find a dictation in history.
//! `items` is pure (unit-tested); `show` draws the modal and returns the chosen item.
use super::settings::Section;
use super::theme::Palette;
use super::widgets::{self as w, cr};
use super::{Page, fonts, history, icons, timefmt};
use crate::core::history::{Entry, Status};
use eframe::egui::{self, Align2, Key, Margin, Sense, vec2};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Page(Page),
    Settings(Section),
    /// A history entry (its id).
    Entry(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub item: Item,
    pub title: String,
    pub subtitle: String,
    pub group: &'static str,
}

#[derive(Default)]
pub struct State {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    just_opened: bool,
}

impl State {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.query.clear();
            self.selected = 0;
            self.just_opened = true;
        }
    }
    pub fn close(&mut self) {
        self.open = false;
    }
}

const MAX_ENTRIES: usize = 8;

/// Rows matching `query`: pages and settings by label, history by `history::visible` (newest first).
pub fn items(query: &str, entries: &[Entry], now_ms: i64) -> Vec<Row> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<Row> = Vec::new();
    let go = |label: &str| q.is_empty() || label.to_lowercase().contains(&q);
    for (page, label) in [(Page::History, "History"), (Page::Dictionary, "Dictionary")] {
        if go(label) {
            out.push(Row { item: Item::Page(page), title: label.into(), subtitle: "Go to page".into(), group: "Pages" });
        }
    }
    for s in Section::ALL {
        let label = format!("{} settings", s.label());
        if q.is_empty() || label.to_lowercase().contains(&q) || s.label().to_lowercase().contains(&q) {
            out.push(Row { item: Item::Settings(s), title: s.label().into(), subtitle: "Open settings".into(), group: "Settings" });
        }
    }
    if q.is_empty() {
        // Without a query, only offer a few recent dictations.
        for e in entries.iter().filter(|e| e.status != Status::Discarded).take(3) {
            out.push(entry_row(e, now_ms));
        }
    } else {
        for i in history::visible(entries, query, false).into_iter().take(MAX_ENTRIES) {
            out.push(entry_row(&entries[i], now_ms));
        }
    }
    out
}

fn entry_row(e: &Entry, now_ms: i64) -> Row {
    let off = |ts: i64| timefmt::local_offset_secs(ts);
    let day = timefmt::day_label(e.ts, now_ms, &off);
    let when = format!("{day}, {}", timefmt::clock(e.ts, off(e.ts)));
    let title: String = e.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if title.is_empty() { e.error.clone().unwrap_or_else(|| "(empty)".into()) } else { title };
    let sub = match e.app_name.as_deref() {
        Some(a) if !a.is_empty() => format!("{when} · {a}"),
        _ => when,
    };
    Row { item: Item::Entry(e.id), title, subtitle: sub, group: "History" }
}

/// Draw the palette. Returns the activated item, if any (the caller closes it).
pub fn show(ctx: &egui::Context, p: &Palette, st: &mut State, entries: &[Entry], now_ms: i64) -> Option<Item> {
    if !st.open {
        return None;
    }
    let rows = items(&st.query, entries, now_ms);
    if !rows.is_empty() {
        st.selected = st.selected.min(rows.len() - 1);
    }
    let (down, up, enter) = ctx.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
    if down && !rows.is_empty() {
        st.selected = (st.selected + 1) % rows.len();
    }
    if up && !rows.is_empty() {
        st.selected = (st.selected + rows.len() - 1) % rows.len();
    }
    let mut chosen: Option<Item> = None;
    if enter {
        chosen = rows.get(st.selected).map(|r| r.item.clone());
    }

    let screen = ctx.content_rect();
    let width = (screen.width() - 48.0).clamp(320.0, 600.0);
    let area = egui::Area::new(egui::Id::new("palette-area"))
        .anchor(Align2::CENTER_TOP, vec2(0.0, screen.height() * 0.14))
        .order(egui::Order::Foreground);
    let modal = w::glass_modal_at(ctx, p, egui::Id::new("palette"), 0, Some(area), |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            // Search field
            let (rect, _) = ui.allocate_exact_size(vec2(width, 48.0), Sense::hover());
            w::paint_icon(ui, egui::Rect::from_center_size(egui::pos2(rect.min.x + 24.0, rect.center().y), vec2(16.0, 16.0)), icons::SEARCH, 14.0, p.muted_a(70));
            let id = egui::Id::new("palette-input");
            let edit = egui::TextEdit::singleline(&mut st.query)
                .id(id)
                .frame(egui::Frame::NONE)
                .hint_text(egui::RichText::new("Search dictations, pages and settings").color(p.muted_a(60)))
                .font(fonts::regular(14.0))
                .text_color(p.foreground)
                .margin(Margin::ZERO)
                .vertical_align(egui::Align::Center)
                .desired_width(width - 64.0);
            let r = ui.put(egui::Rect::from_min_size(egui::pos2(rect.min.x + 44.0, rect.min.y), vec2(width - 56.0, 48.0)), edit);
            if st.just_opened {
                r.request_focus();
                st.just_opened = false;
            }
            if r.changed() {
                st.selected = 0;
            }
            w::divider(ui, p);
            egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.add_space(6.0);
                if rows.is_empty() {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| w::text(ui, "No results", 13.0, p.muted_a(70)));
                    ui.add_space(18.0);
                }
                let mut last_group = "";
                for (i, row) in rows.iter().enumerate() {
                    if row.group != last_group {
                        last_group = row.group;
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            w::text_semi(ui, row.group, 11.0, p.muted_a(65));
                        });
                        ui.add_space(2.0);
                    }
                    let (r, resp) = ui.allocate_exact_size(vec2(width - 12.0, 40.0), Sense::click());
                    let r = r.translate(vec2(6.0, 0.0));
                    w::pointer(&resp);
                    if resp.hovered() && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) {
                        st.selected = i;
                    }
                    if st.selected == i {
                        ui.painter().rect_filled(r, cr(8.0), p.primary_wash(1));
                    }
                    let glyph = match &row.item {
                        Item::Page(Page::History) => icons::HISTORY,
                        Item::Page(Page::Dictionary) => icons::BOOK,
                        Item::Settings(_) => icons::SETTINGS,
                        Item::Entry(_) => icons::MIC,
                    };
                    ui.painter().text(egui::pos2(r.min.x + 20.0, r.center().y), Align2::CENTER_CENTER, glyph.to_string(), fonts::icon(14.0), p.muted_a(75));
                    let clip = egui::Rect::from_min_max(egui::pos2(r.min.x + 40.0, r.min.y), egui::pos2(r.max.x - 12.0, r.max.y));
                    let painter = ui.painter().with_clip_rect(clip);
                    painter.text(egui::pos2(r.min.x + 40.0, r.center().y - 7.0), Align2::LEFT_CENTER, &row.title, fonts::regular(13.0), p.foreground);
                    painter.text(egui::pos2(r.min.x + 40.0, r.center().y + 8.0), Align2::LEFT_CENTER, &row.subtitle, fonts::regular(11.0), p.muted_a(65));
                    if resp.clicked() {
                        chosen = Some(row.item.clone());
                    }
                }
                ui.add_space(6.0);
            });
        });
    if modal.should_close() {
        st.close();
        return None;
    }
    if chosen.is_some() {
        st.close();
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: i64, text: &str, status: Status) -> Entry {
        Entry { id, ts: 1_760_000_000_000 + id, text: text.into(), status, ..Default::default() }
    }

    #[test]
    fn empty_query_lists_pages_settings_and_recent() {
        let es = vec![e(4, "four", Status::Ok), e(3, "three", Status::Discarded), e(2, "two", Status::Ok), e(1, "one", Status::Ok), e(0, "zero", Status::Ok)];
        let r = items("", &es, 1_760_000_100_000);
        assert_eq!(r.iter().filter(|x| x.group == "Pages").count(), 2);
        assert_eq!(r.iter().filter(|x| x.group == "Settings").count(), Section::ALL.len());
        let ids: Vec<i64> = r.iter().filter_map(|x| if let Item::Entry(i) = x.item { Some(i) } else { None }).collect();
        assert_eq!(ids, vec![4, 2, 1], "discarded entries are not suggested");
    }

    #[test]
    fn query_filters_everything() {
        let es = vec![e(2, "Send the budget to Priya", Status::Ok), e(1, "Buy milk", Status::Ok)];
        let r = items("priya", &es, 1_760_000_100_000);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].item, Item::Entry(2));
        let r = items("hotkey", &es, 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].item, Item::Settings(Section::Hotkeys));
        let r = items("dict", &es, 0);
        assert!(r.iter().any(|x| x.item == Item::Page(Page::Dictionary)));
    }

    #[test]
    fn entry_cap() {
        let es: Vec<Entry> = (0..30).map(|i| e(i, "hello world", Status::Ok)).collect();
        let r = items("hello", &es, 1_760_000_100_000);
        assert_eq!(r.len(), MAX_ENTRIES);
    }

    #[test]
    fn toggle_resets_query() {
        let mut s = State::default();
        s.toggle();
        assert!(s.open);
        s.query = "abc".into();
        s.toggle();
        assert!(!s.open);
        s.toggle();
        assert!(s.query.is_empty());
    }
}
