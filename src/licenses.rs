// SPDX-License-Identifier: GPL-3.0-or-later
//! Offline notices remain available before connecting to a car.

use eframe::egui;

pub const HEAD_UNIT_WARNING: &str = "This can brick your head unit. Recovery may require professional service or replacement, at your expense. Provided AS IS; use at your own risk, subject to applicable law. See Licenses for the full disclaimer.";

const NOTICES: &str = include_str!("../legal/NOTICE.txt");

pub struct View {
    open: bool,
    selected: usize,
    sections: Vec<(&'static str, &'static str)>,
}

impl Default for View {
    fn default() -> Self {
        let sections = NOTICES
            .trim()
            .split(
                "\n\n========================================================================\n\n",
            )
            .map(|section| section.split_once("\n\n").unwrap_or((section, "")))
            .collect();
        Self {
            open: false,
            selected: 0,
            sections,
        }
    }
}

impl View {
    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let Self {
            open,
            selected,
            sections,
        } = self;
        egui::Window::new("Licenses")
            .open(open)
            .default_size([640.0, 480.0])
            .show(ctx, |ui| {
                egui::ComboBox::from_id_salt("license_component")
                    .selected_text(sections[*selected].0)
                    .show_ui(ui, |ui| {
                        for (index, (title, _)) in sections.iter().enumerate() {
                            ui.selectable_value(selected, index, *title);
                        }
                    });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt(("license_text", *selected))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(sections[*selected].1)
                                .selectable(true)
                                .wrap(),
                        );
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_notices_include_rights_and_full_license() {
        let view = View::default();
        assert!(view.sections[0].1.contains("WITHOUT ANY WARRANTY"));
        assert!(view.sections.iter().any(|(title, text)| {
            *title == "GPL-3.0-or-later" && text.contains("TERMS AND CONDITIONS")
        }));
    }
}
