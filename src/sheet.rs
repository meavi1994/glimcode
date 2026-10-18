use anyhow::{Context, Result};
use eframe::egui;
use egui_data_table::{
    DataTable, RowViewer,
    viewer::{DecodeErrorBehavior, RowCodec, UiActionContext, default_hotkeys},
};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

type ValueSelection = Option<BTreeSet<String>>;
type RowSelection = (String, Vec<ValueSelection>);

pub type Row = Vec<String>;

pub struct Sheet {
    pub table: DataTable<Row>,
    pub viewer: Viewer,
    delimiter: u8,
    has_headers: bool,
    sort_column: usize,
    counted_filter: Option<RowSelection>,
    values_dirty: bool,
    visible_count: usize,
    compact: bool,
    single_click: bool,
    inspect: bool,
}

#[derive(Clone)]
pub struct Viewer {
    pub headers: Vec<String>,
    filter: RowSelection,
    unique_values: Vec<Vec<(String, usize)>>,
    value_search: Vec<String>,
    view_sort: Vec<(usize, bool)>,
    sort_requested: bool,
    pending_values: Option<(usize, ValueSelection)>,
    commit_requested: bool,
    edited_this_frame: bool,
    wrap: bool,
    selected: Option<(String, String)>,
    widths: Vec<f32>,
}

/// Score complete records so quoted delimiters and multiline fields stay intact.
pub fn detect_delimiter(source: &str) -> u8 {
    (*b",;\t|")
        .into_iter()
        .max_by_key(|delimiter| {
            let mut reader = csv::ReaderBuilder::new()
                .delimiter(*delimiter)
                .has_headers(false)
                .flexible(true)
                .from_reader(source.as_bytes());
            let counts: Vec<_> = reader
                .records()
                .take(20)
                .filter_map(Result::ok)
                .map(|r| r.len())
                .collect();
            let columns = counts.first().copied().unwrap_or(1);
            if columns <= 1 {
                return 0;
            }
            let consistent = counts.iter().filter(|&&n| n == columns).count();
            if consistent == counts.len() {
                1000 + columns
            } else {
                consistent
            }
        })
        .filter(|delimiter| {
            csv::ReaderBuilder::new()
                .delimiter(*delimiter)
                .has_headers(false)
                .from_reader(source.as_bytes())
                .records()
                .next()
                .is_some_and(|r| r.is_ok_and(|r| r.len() > 1))
        })
        .unwrap_or(b',')
}

pub fn compare(a: &str, b: &str) -> Ordering {
    let number = |value: &str| value.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    match (number(a), number(b)) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

impl Sheet {
    pub fn parse(source: &str, delimiter: u8, has_headers: bool) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .from_reader(source.as_bytes());
        let mut records = reader.records();
        let first = records.next().transpose().context("Invalid CSV record")?;
        let count = first.as_ref().map_or(1, |r| r.len());
        let headers = if has_headers {
            first
                .as_ref()
                .map(|r| r.iter().map(str::to_owned).collect())
                .unwrap_or_else(|| vec!["Column 1".into()])
        } else {
            (1..=count).map(|i| format!("Column {i}")).collect()
        };
        let mut rows: Vec<Row> = vec![];
        if !has_headers && let Some(first) = first {
            rows.push(first.iter().map(str::to_owned).collect());
        }
        for record in records {
            rows.push(
                record
                    .context("CSV rows must have the same number of fields")?
                    .iter()
                    .map(str::to_owned)
                    .collect(),
            );
        }
        // Bound auto-sizing work independently of file size.
        let widths = (0..count)
            .map(|column| {
                let chars = std::iter::once(headers[column].chars().count() + 5)
                    .chain(rows.iter().take(200).map(|row| {
                        row[column]
                            .lines()
                            .map(|line| line.chars().count())
                            .max()
                            .unwrap_or(0)
                    }))
                    .max()
                    .unwrap_or(12);
                (chars as f32 * 7.5 + 24.0).clamp(90.0, 320.0)
            })
            .collect();
        Ok(Self {
            table: rows.into_iter().collect(),
            viewer: Viewer {
                headers,
                filter: (String::new(), vec![None; count]),
                unique_values: vec![vec![]; count],
                value_search: vec![String::new(); count],
                view_sort: vec![],
                sort_requested: false,
                pending_values: None,
                commit_requested: false,
                edited_this_frame: false,
                wrap: false,
                selected: None,
                widths,
            },
            delimiter,
            has_headers,
            sort_column: 0,
            counted_filter: None,
            values_dirty: true,
            visible_count: 0,
            compact: false,
            single_click: false,
            inspect: false,
        })
    }

    pub fn delimiter(&self) -> u8 {
        self.delimiter
    }

    pub fn serialize(&self) -> Result<String> {
        let mut writer = csv::WriterBuilder::new()
            .delimiter(self.delimiter)
            .from_writer(Vec::new());
        if self.has_headers {
            writer.write_record(&self.viewer.headers)?;
        }
        for row in self.table.iter() {
            writer.write_record(row)?;
        }
        String::from_utf8(writer.into_inner()?).context("Cannot encode CSV")
    }

    pub fn commit_edit(&mut self) {
        self.viewer.commit_requested = true;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        if self.values_dirty {
            self.refresh_unique_values();
        }
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);
        ui.visuals_mut().faint_bg_color = egui::Color32::from_rgb(32, 35, 41);
        ui.visuals_mut().widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(47, 52, 62));
        let mut changed = false;
        let previous_filter = self.viewer.filter.clone();
        let mut add_row = false;
        let mut add_column = false;
        let mut sort = None;
        egui::Frame::new()
            .fill(crate::chrome::PANEL)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    egui::Frame::new()
                        .fill(crate::chrome::EDITOR)
                        .corner_radius(7)
                        .stroke(egui::Stroke::new(1.0_f32, crate::chrome::BORDER))
                        .inner_margin(egui::Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                crate::chrome::heading_icon(
                                    ui,
                                    crate::chrome::Icon::Search,
                                    crate::chrome::MUTED,
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.viewer.filter.0)
                                        .frame(false)
                                        .hint_text("Search all columns…")
                                        .desired_width(
                                            (ui.available_width() * 0.28).clamp(140.0, 260.0),
                                        ),
                                );
                            });
                        });
                    ui.add_space(4.0);
                    ui.menu_button("+ Insert", |ui| {
                        if ui.button("Add row").clicked() {
                            add_row = true;
                            ui.close();
                        }
                        if ui.button("Add column").clicked() {
                            add_column = true;
                            ui.close();
                        }
                    });
                    ui.menu_button("Display", |ui| {
                        ui.checkbox(&mut self.compact, "Compact rows");
                        ui.checkbox(&mut self.viewer.wrap, "Wrap cell text");
                        ui.checkbox(&mut self.single_click, "Single-click editing");
                        ui.checkbox(&mut self.inspect, "Show selected cell value");
                    });
                    ui.menu_button("Organize", |ui| {
                        if self.has_headers {
                            ui.label("Column names");
                            for header in &mut self.viewer.headers {
                                changed |= ui.text_edit_singleline(header).changed();
                            }
                        }
                        ui.separator();
                        ui.label("Reorder rows in saved file");
                        egui::ComboBox::from_id_salt("sort-column")
                            .selected_text(&self.viewer.headers[self.sort_column])
                            .show_ui(ui, |ui| {
                                for (i, header) in self.viewer.headers.iter().enumerate() {
                                    ui.selectable_value(&mut self.sort_column, i, header);
                                }
                            });
                        ui.horizontal(|ui| {
                            for (label, descending) in [("Ascending", false), ("Descending", true)]
                            {
                                if ui.button(label).clicked() {
                                    sort = Some(descending);
                                }
                            }
                        });
                        ui.weak(
                            "Header sorting changes only the view. This changes saved row order.",
                        );
                    });
                    let active = !self.viewer.filter.0.is_empty()
                        || self.viewer.filter.1.iter().any(Option::is_some)
                        || !self.viewer.view_sort.is_empty();
                    if ui
                        .add_enabled(active, egui::Button::new("Reset view").frame(false))
                        .on_hover_text("Clear search, value selections and sorting")
                        .clicked()
                    {
                        self.viewer.filter = (String::new(), vec![None; self.viewer.headers.len()]);
                        self.viewer.view_sort.clear();
                        self.viewer.sort_requested = true;
                        self.viewer.commit_requested = true;
                    }
                    ui.menu_button("Help", |ui| {
                        ui.label("Click a column header to sort or choose values.");
                        ui.label(
                            "Values within a column combine with OR; columns combine with AND.",
                        );
                        ui.label("Double-click / Enter: edit cell");
                        ui.label("Tab: commit and move · Escape: commit");
                        ui.label("Cmd/Ctrl Escape: cancel edit");
                        ui.label("Cmd/Ctrl C / V: copy / paste a range");
                        ui.label("Cmd/Ctrl Z: undo · right-click: row actions");
                        ui.label(
                            "Drag column borders to resize; use header menus to hide columns.",
                        );
                    });
                });
            });
        if self.inspect {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if let Some((header, value)) = &self.viewer.selected {
                    ui.label(egui::RichText::new(header).color(crate::chrome::BLUE));
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("cell-value")
                        .max_height(70.0)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(value).selectable(true).wrap());
                        });
                } else {
                    ui.weak("Select a cell to inspect its full value");
                }
            });
        }
        let next_filter = self.viewer.filter.clone();
        if previous_filter != next_filter || add_row || add_column || sort.is_some() {
            // Commit the current editor before filtering or changing the table shape.
            self.viewer.commit_requested = true;
            self.viewer.filter = previous_filter;
        }
        if changed || self.counted_filter.as_ref() != Some(&self.viewer.filter) {
            self.visible_count = self
                .table
                .iter()
                .filter(|row| self.viewer.filter_row(row))
                .count();
            self.counted_filter = Some(self.viewer.filter.clone());
        }
        egui::Frame::new()
            .fill(crate::chrome::TABS)
            .inner_margin(egui::Margin::symmetric(14, 5))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.set_min_height(28.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} of {} rows",
                            self.visible_count,
                            self.table.len()
                        ))
                        .size(12.0)
                        .color(crate::chrome::TEXT),
                    );
                    ui.weak(
                        egui::RichText::new(format!("/  {} columns", self.viewer.headers.len()))
                            .size(12.0),
                    );
                    ui.separator();
                    ui.weak(
                        egui::RichText::new(match self.delimiter {
                            b'\t' => "TSV",
                            b';' => "Semicolon",
                            b'|' => "Pipe",
                            _ => "CSV",
                        })
                        .size(11.0),
                    );
                    ui.add_space(8.0);
                    egui::ScrollArea::horizontal()
                        .id_salt("selection-chips")
                        .max_height(28.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if self.viewer.filter.1.iter().all(Option::is_none) {
                                    ui.weak(
                                        egui::RichText::new(
                                            "Click a column header to sort or choose values",
                                        )
                                        .size(11.0),
                                    );
                                }
                                for column in 0..self.viewer.headers.len() {
                                    if let Some(values) = &self.viewer.filter.1[column] {
                                        let label = format!(
                                            "{}: {} values  ×",
                                            self.viewer.headers[column],
                                            values.len()
                                        );
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new(label)
                                                        .size(11.0)
                                                        .color(crate::chrome::BLUE),
                                                )
                                                .fill(crate::chrome::SELECTED.gamma_multiply(0.5))
                                                .corner_radius(5),
                                            )
                                            .on_hover_text("Show all values in this column")
                                            .clicked()
                                        {
                                            self.viewer.pending_values = Some((column, None));
                                            self.viewer.commit_requested = true;
                                        }
                                    }
                                }
                            });
                        });
                });
            });
        ui.push_id("data-grid", |ui| {
            let wrap = self.viewer.wrap;
            ui.add(
                egui_data_table::Renderer::new(&mut self.table, &mut self.viewer)
                    .with_style_modify(|style| {
                        style.table_row_height = if wrap {
                            None
                        } else {
                            Some(if self.compact { 27.0 } else { 36.0 })
                        };
                        style.single_click_edit_mode = self.single_click;
                        style.auto_shrink = egui::Vec2b::new(false, false);
                        style.bg_selected_cell = Some(crate::chrome::SELECTED);
                        style.bg_selected_highlight_cell =
                            Some(egui::Color32::from_rgb(49, 82, 135));
                        style.focused_row_stroke = Some(crate::chrome::BLUE.gamma_multiply(0.45));
                    }),
            );
        });
        self.viewer.filter = next_filter;
        if let Some((column, values)) = self.viewer.pending_values.take() {
            self.viewer.filter.1[column] = values;
            ui.ctx().request_repaint();
        }
        if add_row {
            self.table
                .push(vec![String::new(); self.viewer.headers.len()]);
            changed = true;
        }
        if add_column {
            self.viewer
                .headers
                .push(format!("Column {}", self.viewer.headers.len() + 1));
            self.viewer.filter.1.push(None);
            self.viewer.unique_values.push(vec![]);
            self.viewer.value_search.push(String::new());
            self.viewer.widths.push(140.0);
            for row in self.table.iter_mut() {
                row.push(String::new());
            }
            changed = true;
        }
        if let Some(descending) = sort {
            let column = self.sort_column;
            self.table.sort_by(|a, b| {
                let order = compare(&a[column], &b[column]);
                if descending { order.reverse() } else { order }
            });
            changed = true;
        }
        changed |= self.table.has_user_modification() || self.viewer.edited_this_frame;
        self.viewer.commit_requested = false;
        self.viewer.edited_this_frame = false;
        if changed {
            self.counted_filter = None;
            self.values_dirty = true;
        }
        self.table.clear_user_modification_flag();
        changed
    }
}

impl Sheet {
    fn refresh_unique_values(&mut self) {
        self.viewer.unique_values = (0..self.viewer.headers.len())
            .map(|column| {
                let mut counts = BTreeMap::<String, usize>::new();
                for row in self.table.iter() {
                    *counts.entry(row[column].clone()).or_default() += 1;
                }
                let mut values: Vec<_> = counts.into_iter().collect();
                values.sort_by(|(a, _), (b, _)| compare(a, b).then_with(|| a.cmp(b)));
                values
            })
            .collect();
        self.values_dirty = false;
    }
}

impl RowViewer<Row> for Viewer {
    fn num_columns(&mut self) -> usize {
        self.headers.len()
    }
    fn column_name(&mut self, column: usize) -> Cow<'static, str> {
        format!("{}   {}", column_letter(column), self.headers[column]).into()
    }
    fn is_sortable_column(&mut self, _: usize) -> bool {
        true
    }
    fn show_column_header(&mut self, ui: &mut egui::Ui, column: usize) -> bool {
        ui.painter()
            .rect_filled(ui.max_rect(), 0, egui::Color32::from_rgb(35, 39, 47));
        ui.push_id(column, |ui| {
            ui.horizontal(|ui| {
                let order = self
                    .view_sort
                    .iter()
                    .find(|(c, _)| *c == column)
                    .map(|(_, asc)| *asc);
                let indicator = match order {
                    Some(true) => " ↑",
                    Some(false) => " ↓",
                    None => "",
                };
                let label = format!("{}{}", self.headers[column], indicator);
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ui.available_height().max(28.0)),
                    egui::Sense::click(),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        format!("{}: sort and choose values", self.headers[column]),
                    )
                });
                let popup_id = egui::Popup::default_response_id(&response);
                let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
                if response.hovered() || response.has_focus() || open {
                    ui.painter().rect_filled(rect, 4, crate::chrome::HEADER);
                }
                let active = self.filter.1[column].is_some();
                let color = if order.is_some() || active {
                    crate::chrome::BLUE
                } else {
                    crate::chrome::TEXT
                };
                let galley =
                    ui.painter()
                        .layout_no_wrap(label, egui::FontId::proportional(12.0), color);
                let text_rect = egui::Rect::from_min_max(
                    rect.min,
                    egui::pos2(rect.right() - 28.0, rect.bottom()),
                );
                ui.painter().with_clip_rect(text_rect).galley(
                    egui::pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0),
                    galley,
                    color,
                );
                ui.painter().text(
                    egui::pos2(rect.right() - 14.0, rect.center().y),
                    egui::Align2::CENTER_CENTER,
                    "...",
                    egui::FontId::proportional(12.0),
                    color,
                );
                // The whole header opens one popup; keep it open for multiple choices.
                egui::Popup::menu(&response)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .style(column_menu_style)
                    .show(|ui| {
                        ui.set_width(280.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&self.headers[column])
                                    .size(14.0)
                                    .strong()
                                    .color(crate::chrome::TEXT),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.weak(egui::RichText::new("COLUMN").size(9.0));
                                },
                            );
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                            for (label, ascending) in
                                [("↑  Ascending", true), ("↓  Descending", false)]
                            {
                                let selected = order == Some(ascending);
                                if ui
                                    .add_sized(
                                        [width, 30.0],
                                        egui::Button::new(egui::RichText::new(label).size(12.0))
                                            .fill(if selected {
                                                crate::chrome::SELECTED
                                            } else {
                                                crate::chrome::HEADER
                                            })
                                            .stroke(egui::Stroke::NONE)
                                            .corner_radius(5),
                                    )
                                    .clicked()
                                {
                                    self.view_sort = vec![(column, ascending)];
                                    self.sort_requested = true;
                                    self.commit_requested = true;
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    order.is_some(),
                                    egui::Button::new(
                                        egui::RichText::new("Original row order").size(11.0),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                self.view_sort.clear();
                                self.sort_requested = true;
                                self.commit_requested = true;
                            }
                        });
                        ui.add_space(3.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Show values").size(12.0).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.weak(
                                        egui::RichText::new(format!(
                                            "{} unique",
                                            self.unique_values[column].len()
                                        ))
                                        .size(11.0),
                                    );
                                },
                            );
                        });
                        egui::Frame::new()
                            .fill(crate::chrome::EDITOR)
                            .corner_radius(5)
                            .stroke(egui::Stroke::new(1.0_f32, crate::chrome::BORDER))
                            .inner_margin(egui::Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    crate::chrome::heading_icon(
                                        ui,
                                        crate::chrome::Icon::Search,
                                        crate::chrome::MUTED,
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.value_search[column])
                                            .frame(false)
                                            .font(egui::TextStyle::Body)
                                            .hint_text("Find a value…")
                                            .desired_width(ui.available_width()),
                                    );
                                });
                            });
                        let mut selection = self.filter.1[column].clone();
                        let before = selection.clone();
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Select all")
                                            .size(11.0)
                                            .color(crate::chrome::BLUE),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                selection = None;
                            }
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Select none")
                                            .size(11.0)
                                            .color(crate::chrome::BLUE),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                selection = Some(BTreeSet::new());
                            }
                        });
                        let search = self.value_search[column].to_lowercase();
                        let matching: Vec<_> = self.unique_values[column]
                            .iter()
                            .filter(|(value, _)| value.to_lowercase().contains(&search))
                            .collect();
                        if matching.is_empty() {
                            ui.weak("No matching values");
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("values")
                            .max_height(260.0)
                            .show_rows(ui, 30.0, matching.len(), |ui, range| {
                                for index in range {
                                    let (value, count) = matching[index];
                                    let checked = selection
                                        .as_ref()
                                        .is_none_or(|values| values.contains(value));
                                    if value_choice(ui, value, *count, checked).clicked() {
                                        let values = selection.get_or_insert_with(|| {
                                            self.unique_values[column]
                                                .iter()
                                                .map(|(v, _)| v.clone())
                                                .collect()
                                        });
                                        if checked {
                                            values.remove(value);
                                        } else {
                                            values.insert(value.clone());
                                        }
                                    }
                                }
                            });
                        ui.add_space(3.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            let selected = selection
                                .as_ref()
                                .map_or(self.unique_values[column].len(), BTreeSet::len);
                            ui.weak(egui::RichText::new(format!("{selected} selected")).size(10.0));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.weak(
                                        egui::RichText::new("Counts in entire file").size(10.0),
                                    );
                                },
                            );
                        });
                        if selection != before {
                            self.pending_values = Some((column, selection));
                            self.commit_requested = true;
                        }
                    });
                #[cfg(feature = "ui-capture")]
                if column == 1 && std::env::var("CODER_UI_PREVIEW").as_deref() == Ok("csv-column") {
                    egui::Popup::open_id(ui.ctx(), popup_id);
                }
                response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Sort and choose unique values");
            });
        });
        true
    }
    fn sync_column_sort(&mut self, sort: &[(usize, bool)]) {
        if !self.sort_requested {
            self.view_sort = sort.to_vec();
        }
    }
    fn take_column_sort(&mut self) -> Option<Vec<(usize, bool)>> {
        std::mem::take(&mut self.sort_requested).then(|| self.view_sort.clone())
    }
    fn compare_cell(&self, a: &Row, b: &Row, column: usize) -> Ordering {
        compare(&a[column], &b[column])
    }
    fn row_filter_hash(&mut self) -> &impl std::hash::Hash {
        &self.filter
    }
    fn filter_row(&mut self, row: &Row) -> bool {
        let global = self.filter.0.to_lowercase();
        row.iter().any(|v| v.to_lowercase().contains(&global))
            && row
                .iter()
                .zip(&self.filter.1)
                .all(|(v, selected)| selected.as_ref().is_none_or(|values| values.contains(v)))
    }
    fn column_render_config(
        &mut self,
        column: usize,
        _: bool,
    ) -> egui_data_table::viewer::TableColumnConfig {
        egui_data_table::viewer::TableColumnConfig::initial(
            self.widths.get(column).copied().unwrap_or(140.0),
        )
        .at_least(70.0)
        .resizable(true)
        .clip(true)
    }
    fn on_highlight_cell(&mut self, row: &Row, column: usize) {
        self.selected = Some((self.headers[column].clone(), row[column].clone()));
    }
    fn show_cell_view(&mut self, ui: &mut egui::Ui, row: &Row, column: usize) {
        let value = &row[column];
        let numeric = value.trim().parse::<f64>().is_ok_and(f64::is_finite);
        let color = if numeric {
            egui::Color32::from_rgb(115, 175, 225)
        } else if matches!(value.as_str(), "true" | "false") {
            egui::Color32::from_rgb(195, 146, 220)
        } else {
            ui.visuals().text_color()
        };
        let layout = if numeric {
            egui::Layout::right_to_left(egui::Align::Center)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        ui.with_layout(layout, |ui| {
            let label = egui::Label::new(egui::RichText::new(value).color(color));
            ui.add(if self.wrap {
                label.wrap()
            } else {
                label.truncate()
            })
            .on_hover_text(value);
        });
    }
    fn show_cell_editor(
        &mut self,
        ui: &mut egui::Ui,
        row: &mut Row,
        column: usize,
    ) -> Option<egui::Response> {
        if self.commit_requested {
            return None;
        }
        let response = ui.add(
            egui::TextEdit::multiline(&mut row[column])
                .desired_rows(1)
                .desired_width(f32::INFINITY),
        );
        self.edited_this_frame |= response.changed();
        self.selected = Some((self.headers[column].clone(), row[column].clone()));
        Some(response)
    }
    fn set_cell_value(&mut self, src: &Row, dst: &mut Row, column: usize) {
        dst[column].clone_from(&src[column]);
    }
    fn new_empty_row(&mut self) -> Row {
        vec![String::new(); self.headers.len()]
    }
    fn try_create_codec(&mut self, _: bool) -> Option<impl RowCodec<Row>> {
        Some(Codec(self.headers.len()))
    }
    fn hotkeys(
        &mut self,
        context: &UiActionContext,
    ) -> Vec<(egui::KeyboardShortcut, egui_data_table::UiAction)> {
        default_hotkeys(context)
            .into_iter()
            .map(|(mut shortcut, action)| {
                if shortcut.modifiers.ctrl {
                    shortcut.modifiers.ctrl = false;
                    shortcut.modifiers.command = true;
                }
                (shortcut, action)
            })
            .collect()
    }
}

fn column_menu_style(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(12);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
    style.visuals.window_fill = egui::Color32::from_rgb(36, 39, 46);
    style.visuals.window_stroke = egui::Stroke::new(1.0_f32, crate::chrome::BORDER);
}

fn value_choice(ui: &mut egui::Ui, value: &str, count: usize, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), selected, value)
    });
    let painter = ui.painter();
    if response.hovered() || response.has_focus() {
        painter.rect_filled(rect, 5, crate::chrome::HEADER);
    }
    let checkbox = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::vec2(14.0, 14.0),
    );
    painter.rect(
        checkbox,
        3,
        if selected {
            crate::chrome::BLUE
        } else {
            crate::chrome::EDITOR
        },
        egui::Stroke::new(
            1.0_f32,
            if selected {
                crate::chrome::BLUE
            } else {
                crate::chrome::MUTED.gamma_multiply(0.5)
            },
        ),
        egui::StrokeKind::Inside,
    );
    if selected {
        crate::chrome::paint_icon(
            painter,
            checkbox.center(),
            crate::chrome::Icon::Check,
            egui::Color32::WHITE,
        );
    }
    let count_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 20.0, rect.center().y),
        egui::vec2(32.0, 20.0),
    );
    painter.rect_filled(count_rect, 4, crate::chrome::EDITOR);
    painter.text(
        count_rect.center(),
        egui::Align2::CENTER_CENTER,
        count.to_string(),
        egui::FontId::proportional(10.0),
        crate::chrome::MUTED,
    );
    let label = if value.is_empty() { "(Blank)" } else { value };
    let galley = painter.layout(
        label.to_owned(),
        egui::FontId::proportional(12.0),
        crate::chrome::TEXT,
        f32::INFINITY,
    );
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 28.0, rect.top()),
        egui::pos2(count_rect.left() - 8.0, rect.bottom()),
    );
    painter.with_clip_rect(text_rect).galley(
        egui::pos2(text_rect.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        crate::chrome::TEXT,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(value)
}

fn column_letter(mut index: usize) -> String {
    let mut label = String::new();
    loop {
        label.insert(0, (b'A' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    label
}

struct Codec(usize);
impl RowCodec<Row> for Codec {
    type DeserializeError = ();
    fn create_empty_decoded_row(&mut self) -> Row {
        vec![String::new(); self.0]
    }
    fn encode_column(&mut self, row: &Row, column: usize, dst: &mut String) {
        dst.push_str(&row[column]);
    }
    fn decode_column(
        &mut self,
        data: &str,
        column: usize,
        row: &mut Row,
    ) -> Result<(), DecodeErrorBehavior> {
        let Some(cell) = row.get_mut(column) else {
            return Err(DecodeErrorBehavior::Abort);
        };
        *cell = data.to_owned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_delimiters_without_splitting_quoted_values() -> Result<()> {
        for (source, delimiter) in [
            ("name;note\nAlice;\"commas, and|pipes\"\n", b';'),
            ("name|value\nAlice|10\n", b'|'),
            ("name\tvalue\nAlice\t10\n", b'\t'),
            ("name,value\nAlice,10\n", b','),
            ("name\nAlice\n", b','),
        ] {
            assert_eq!(detect_delimiter(source), delimiter);
            let sheet = Sheet::parse(source, delimiter, true)?;
            assert_eq!(
                Sheet::parse(&sheet.serialize()?, delimiter, true)?
                    .table
                    .len(),
                sheet.table.len()
            );
        }
        Ok(())
    }
    #[test]
    fn quoted_multiline_csv_roundtrips_and_filters_do_not_drop_rows() -> Result<()> {
        let original = "name,note\r\n猫,\"hello, world\"\r\nZoe,\"line one\nline two\"\r\n";
        let mut sheet = Sheet::parse(original, b',', true)?;
        sheet.viewer.filter.0 = "猫".into();
        assert!(sheet.viewer.filter_row(&sheet.table[0]));
        assert!(!sheet.viewer.filter_row(&sheet.table[1]));
        let restored = Sheet::parse(&sheet.serialize()?, b',', true)?;
        assert_eq!(*restored.table, *sheet.table);
        assert_eq!(restored.viewer.headers, ["name", "note"]);
        Ok(())
    }
    #[test]
    fn headerless_tsv_and_numeric_sort() -> Result<()> {
        let sheet = Sheet::parse("10\tx\n2\ty\n", b'\t', false)?;
        assert_eq!(sheet.table.len(), 2);
        assert_eq!(
            Sheet::parse(&sheet.serialize()?, b'\t', false)?.table.len(),
            2
        );
        assert_eq!(compare("10", "2"), Ordering::Greater);
        assert!(Sheet::parse("a,b\n1,2,3\n", b',', true).is_err());
        Ok(())
    }

    #[test]
    fn mixed_column_sort_is_transitive() {
        let values = ["2", "10", "1a", "NaN", "", "-1", "猫"];
        for a in values {
            for b in values {
                for c in values {
                    if compare(a, b) != Ordering::Greater && compare(b, c) != Ordering::Greater {
                        assert_ne!(compare(a, c), Ordering::Greater);
                    }
                }
            }
        }
    }

    #[test]
    fn exact_values_combine_with_or_within_columns_and_and_across_columns() -> Result<()> {
        let source = "name,city\nAlice,York\nBob,York\nAlice,New York\nCara,\n";
        let mut sheet = Sheet::parse(source, b',', true)?;
        sheet.viewer.filter.1[0] = Some(["Alice".into(), "Bob".into()].into());
        sheet.viewer.filter.1[1] = Some(["York".into()].into());
        assert_eq!(
            sheet
                .table
                .iter()
                .filter(|row| sheet.viewer.filter_row(row))
                .count(),
            2
        );
        sheet.viewer.filter.0 = "bob".into();
        assert_eq!(
            sheet
                .table
                .iter()
                .filter(|row| sheet.viewer.filter_row(row))
                .count(),
            1
        );
        sheet.viewer.filter.1[0] = Some(BTreeSet::new());
        assert!(!sheet.viewer.filter_row(&sheet.table[1]));
        sheet.viewer.filter = (String::new(), vec![None, Some([String::new()].into())]);
        assert!(sheet.viewer.filter_row(&sheet.table[3]));
        assert!(!sheet.viewer.filter_row(&sheet.table[0]));
        assert_eq!(
            Sheet::parse(&sheet.serialize()?, b',', true)?.table.len(),
            4
        );
        Ok(())
    }

    #[test]
    fn unique_values_include_blanks_exact_case_and_numeric_order() -> Result<()> {
        let mut sheet = Sheet::parse("value\n10\n2\n10\nAlice\nalice\n\"\"\n", b',', true)?;
        sheet.refresh_unique_values();
        assert_eq!(
            sheet.viewer.unique_values[0],
            vec![
                ("2".into(), 1),
                ("10".into(), 2),
                ("".into(), 1),
                ("Alice".into(), 1),
                ("alice".into(), 1),
            ]
        );
        sheet.table[0][0] = "new".into();
        sheet.refresh_unique_values();
        assert!(sheet.viewer.unique_values[0].contains(&("new".into(), 1)));
        assert!(sheet.viewer.unique_values[0].contains(&("10".into(), 1)));
        Ok(())
    }

    #[test]
    fn header_menu_keeps_multiple_value_choices_open_and_sort_preserves_saved_order() -> Result<()>
    {
        let ctx = egui::Context::default();
        let source = "name,value\nAlice,10\nBob,2\nCara,3\n";
        let mut sheet = Sheet::parse(source, b',', true)?;
        let mut time = 0.0;
        let mut frame = |events: Vec<egui::Event>, sheet: &mut Sheet| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        sheet.ui(ui);
                    });
                },
            )
        };
        let text_position = |output: &egui::FullOutput, label: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == label
                    {
                        Some(text.pos + egui::vec2(4.0, 5.0))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("Missing UI label: {label}"))
        };
        let click = |position| {
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        frame(vec![], &mut sheet);
        let output = frame(vec![], &mut sheet);
        let position = text_position(&output, "name");
        frame(click(position), &mut sheet);
        assert!(
            sheet.viewer.view_sort.is_empty(),
            "Opening a header must not silently sort"
        );
        let output = frame(vec![], &mut sheet);
        let position = text_position(&output, "↑  Ascending");
        frame(click(position), &mut sheet);
        assert_eq!(sheet.viewer.view_sort, vec![(0, true)]);
        frame(
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut sheet,
        );
        let output = frame(vec![], &mut sheet);
        let position = text_position(&output, "...");
        frame(click(position), &mut sheet);
        let output = frame(vec![], &mut sheet);
        let position = text_position(&output, "Select none");
        frame(click(position), &mut sheet);
        let output = frame(vec![], &mut sheet);
        // Values are now only present in the open popup; the grid has zero rows.
        let position = text_position(&output, "Alice");
        frame(click(position), &mut sheet);
        let output = frame(vec![], &mut sheet);
        // Choose Bob from the still-open menu rather than the newly visible grid row.
        let position = text_position(&output, "Bob");
        frame(click(position), &mut sheet);
        frame(vec![], &mut sheet);
        assert_eq!(
            sheet.viewer.filter.1[0],
            Some(["Alice".into(), "Bob".into()].into())
        );
        assert_eq!(sheet.visible_count, 2);
        assert_eq!(sheet.serialize()?, source);
        Ok(())
    }

    #[test]
    fn pending_cell_edit_is_committed_before_saving() -> Result<()> {
        let ctx = egui::Context::default();
        let mut sheet = Sheet::parse("name,value\nAlice,10\nBob,2\n", b',', true)?;
        let mut time = 0.0;
        let mut frame = |events: Vec<egui::Event>, sheet: &mut Sheet| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        sheet.ui(ui);
                    });
                },
            )
        };
        frame(vec![], &mut sheet);
        let output = frame(vec![], &mut sheet);
        let position = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.text() == "Alice"
                {
                    Some(text.pos + egui::vec2(8.0, 5.0))
                } else {
                    None
                }
            })
            .expect("CSV cell should be rendered");
        for pressed in [true, false] {
            frame(
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut sheet,
            );
        }
        frame(
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut sheet,
        );
        frame(vec![], &mut sheet);
        frame(vec![egui::Event::Text("Z".into())], &mut sheet);
        sheet.commit_edit();
        frame(vec![], &mut sheet);
        assert!(
            sheet.serialize()?.contains('Z'),
            "Saving must retain the live cell editor's text"
        );
        Ok(())
    }
}
