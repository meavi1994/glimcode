//! A persistent history scrollbar, independent of application mouse reporting.
use super::*;

pub(super) const WIDTH: f32 = 12.0;

#[derive(Default)]
pub(super) struct Scrollbar {
    drag_offset: Option<f32>,
    wheel_remainder: f32,
}

impl Scrollbar {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        screen: &mut vt100::Screen,
        enabled: bool,
        row_height: f32,
    ) {
        let rows = usize::from(screen.size().0);
        let total = screen.history_bounds().1;
        let max_offset = total.saturating_sub(rows);
        let active = enabled && max_offset > 0;
        let response = ui
            .interact(
                rect,
                ui.id().with("terminal-history-scrollbar"),
                if active {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                },
            )
            .on_hover_text(if max_offset == 0 {
                "Terminal history · no retained output above this screen"
            } else {
                "Terminal history · drag to browse, bottom returns to live output"
            });
        let thumb_height = (rect.height() * rows as f32 / total.max(1) as f32)
            .max(24.0)
            .min(rect.height());
        let travel = (rect.height() - thumb_height).max(0.0);
        let thumb_top =
            |offset: usize| rect.top() + travel * (1.0 - offset as f32 / max_offset.max(1) as f32);
        let old_thumb = egui::Rect::from_min_size(
            egui::pos2(rect.left(), thumb_top(screen.scrollback())),
            egui::vec2(rect.width(), thumb_height),
        );
        if active {
            if response.drag_started_by(egui::PointerButton::Primary)
                && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            {
                self.drag_offset = Some(if old_thumb.contains(origin) {
                    origin.y - old_thumb.top()
                } else {
                    thumb_height / 2.0
                });
            }
            let pointer_top = if response.dragged_by(egui::PointerButton::Primary) {
                response
                    .interact_pointer_pos()
                    .map(|pos| pos.y - self.drag_offset.unwrap_or(thumb_height / 2.0))
            } else if response.clicked_by(egui::PointerButton::Primary) {
                response
                    .interact_pointer_pos()
                    .filter(|pos| !old_thumb.contains(*pos))
                    .map(|pos| pos.y - thumb_height / 2.0)
            } else {
                None
            };
            if let Some(top) = pointer_top {
                let fraction = ((top - rect.top()) / travel.max(1.0)).clamp(0.0, 1.0);
                screen.set_scrollback(((1.0 - fraction) * max_offset as f32).round() as usize);
                ui.ctx().request_repaint();
            }
            if response.hovered() {
                self.wheel_remainder += ui.input(|i| i.smooth_scroll_delta.y) / row_height.max(1.0);
                let lines = self.wheel_remainder.trunc() as i64;
                self.wheel_remainder -= lines as f32;
                if lines != 0 {
                    screen.set_scrollback((screen.scrollback() as i64 + lines).max(0) as usize);
                }
            }
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(if response.dragged() {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
            }
        }
        if !ui.input(|i| i.pointer.primary_down()) {
            self.drag_offset = None;
        }
        ui.painter().rect_filled(rect, 3.0, chrome::PANEL);
        let thumb = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 2.0, thumb_top(screen.scrollback())),
            egui::vec2((rect.width() - 4.0).max(1.0), thumb_height),
        );
        let color = if !active {
            chrome::MUTED.gamma_multiply(0.25)
        } else if response.dragged() || response.hovered() {
            chrome::TEXT.gamma_multiply(0.7)
        } else {
            chrome::MUTED.gamma_multiply(0.65)
        };
        ui.painter().rect_filled(thumb, 3.0, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_scrollbar_stays_visible_and_supports_dragging_and_disabled_views() {
        let ctx = egui::Context::default();
        let mut parser = vt100::Parser::new(10, 30, 100);
        let mut scrollbar = Scrollbar::default();
        let rect = egui::Rect::from_min_max(egui::pos2(200.0, 10.0), egui::pos2(212.0, 310.0));
        let mut time = 0.0;
        let mut frame = |scrollbar: &mut Scrollbar, screen: &mut vt100::Screen, events, enabled| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 400.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| scrollbar.show(ui, rect, screen, enabled, 30.0));
                },
            )
        };
        frame(&mut scrollbar, parser.screen_mut(), vec![], true);
        let output = frame(&mut scrollbar, parser.screen_mut(), vec![], true);
        assert!(
            output.shapes.iter().any(
                |shape| matches!(&shape.shape, egui::Shape::Rect(shape) if shape.rect == rect)
            ),
            "the track stays visible even with no history"
        );
        for row in 0..80 {
            parser.process(format!("line {row}\r\n").as_bytes());
        }
        frame(&mut scrollbar, parser.screen_mut(), vec![], true);
        let bottom = egui::pos2(206.0, 300.0);
        let top = egui::pos2(206.0, 20.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: Modifiers::NONE,
        };
        frame(
            &mut scrollbar,
            parser.screen_mut(),
            vec![egui::Event::PointerMoved(bottom), button(bottom, true)],
            true,
        );
        frame(
            &mut scrollbar,
            parser.screen_mut(),
            vec![egui::Event::PointerMoved(top)],
            true,
        );
        frame(
            &mut scrollbar,
            parser.screen_mut(),
            vec![button(top, false)],
            true,
        );
        assert!(
            parser.screen().scrollback() >= 60,
            "dragging the thumb reveals retained history"
        );
        let offset = parser.screen().scrollback();
        for pressed in [true, false] {
            frame(
                &mut scrollbar,
                parser.screen_mut(),
                vec![egui::Event::PointerMoved(bottom), button(bottom, pressed)],
                false,
            );
        }
        assert_eq!(
            parser.screen().scrollback(),
            offset,
            "modal/disabled views do not scroll"
        );
        // egui builds hit testing from the previous frame's active widgets.
        frame(&mut scrollbar, parser.screen_mut(), vec![], true);
        for pressed in [true, false] {
            frame(
                &mut scrollbar,
                parser.screen_mut(),
                vec![egui::Event::PointerMoved(bottom), button(bottom, pressed)],
                true,
            );
        }
        assert_eq!(
            parser.screen().scrollback(),
            0,
            "clicking the bottom returns to live output"
        );
    }
}
