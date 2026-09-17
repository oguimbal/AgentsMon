//! Generic report UI. Provider/protocol decisions belong to the provider and
//! application crates; this view only displays and copies supplied plain text.
use gpui::prelude::*;
use gpui::{div, px, rgb, ClipboardItem, Context, IntoElement, Render, Window};

pub struct DiagnosticReportView {
    title: String,
    report: String,
    copied: bool,
}

impl DiagnosticReportView {
    pub fn new(title: String, report: String) -> Self {
        Self {
            title,
            report,
            copied: false,
        }
    }
}

impl Render for DiagnosticReportView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().size_full().p_5().gap_3()
            .bg(rgb(0x1c1d21)).text_color(rgb(0xe8e8ea)).text_sm()
            .child(div().text_lg().font_weight(gpui::FontWeight::SEMIBOLD).child(self.title.clone()))
            .child(div().text_color(rgb(0xb9bbc3)).child("Copiez ce rapport dans une demande de correction. Il reste disponible dans le menu Diagnostic des connexions."))
            .child(div().id("connection-diagnostic-text").flex_1().min_h_0().overflow_y_scroll()
                .p_3().rounded_sm().bg(rgb(0x25262c)).text_size(px(12.0))
                .children(self.report.lines().map(|line| div().min_h(px(17.0)).child(line.to_owned()))))
            .child(div().flex().justify_end().child(
                div().id("copy-connection-diagnostic").px_3().py_2().rounded_sm()
                    .bg(rgb(0x315f8c)).cursor_pointer().hover(|style| style.opacity(0.88))
                    .on_click(cx.listener(|view, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(view.report.clone()));
                        view.copied = true;
                        cx.notify();
                    }))
                    .child(if self.copied { "Prompt copié" } else { "Copier le prompt de diagnostic" })
            ))
    }
}
