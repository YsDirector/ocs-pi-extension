//! Update handlers for the **OCS Pi Extension** panel (`Message::Pi`).
//!
//! The panel's data lives in `DocumentTab::pi_panel`; worker events are
//! drained here on a 10 Hz timer (`PiMsg::Poll`) — never on the render path —
//! so a chatty stream can't stall the drawing.

use base64::Engine as _;
use iced::Task;
use iced::widget::text_editor;

use super::OpenCADStudio;
use crate::app::Message;
use crate::ui::pi_panel::{PiEntryKind, PiMsg};

impl OpenCADStudio {
    pub(in crate::app) fn on_pi_msg(&mut self, msg: PiMsg) -> Task<Message> {
        let tab = self.active_tab;
        match msg {
            PiMsg::Poll => {
                // Drain every tab's worker channel; apply only mutates per-tab
                // state, so hidden tabs cost nothing but the drain itself.
                let mut scroll = false;
                for (i, tab_state) in self.tabs.iter_mut().enumerate() {
                    // Refresh the selection label only when the selection set
                    // actually changed (fingerprint is cached and O(selected)).
                    let fingerprint = tab_state.scene.selection_fingerprint();
                    if fingerprint != tab_state.pi_panel.selection_fingerprint {
                        tab_state.pi_panel.selection_fingerprint = fingerprint;
                        tab_state.pi_panel.selection_label =
                            selection_label(&tab_state.scene);
                    }
                    let mut events = Vec::new();
                    if let Some(worker) = &tab_state.pi_panel.worker {
                        while let Ok(ev) = worker.rx.try_recv() {
                            events.push(ev);
                            if events.len() >= 512 {
                                break; // one frame's budget; rest polls next tick
                            }
                        }
                    }
                    for ev in events {
                        let changed = tab_state.pi_panel.apply(ev);
                        if i == tab && changed {
                            scroll = true;
                        }
                    }
                }
                if scroll {
                    scroll_transcript_to_bottom()
                } else {
                    Task::none()
                }
            }
            PiMsg::Editor(action) => {
                let panel = &mut self.tabs[tab].pi_panel;
                panel.input.perform(action);
                // Keep the `/` and `@` completion popups in sync with the text.
                panel.refresh_popup();
                Task::none()
            }
            PiMsg::Send => {
                if self.tabs[tab].pi_panel.send_current() {
                    scroll_transcript_to_bottom()
                } else {
                    Task::none()
                }
            }
            PiMsg::SessionPick(id) => {
                // Switch what we follow; the worker refetches + backfills and
                // the panel replaces its list wholesale.
                self.tabs[tab].pi_panel.ensure_worker();
                self.tabs[tab].pi_panel.watch_session(id);
                Task::none()
            }
            PiMsg::ToggleEntry(id) => {
                self.tabs[tab].pi_panel.toggle_entry(id);
                Task::none()
            }
            PiMsg::Reconnect => {
                let panel = &mut self.tabs[tab].pi_panel;
                let had_worker = panel.worker.is_some();
                panel.ensure_worker();
                if had_worker {
                    panel.send_command(crate::pi::Command::Reconnect);
                }
                panel.status = crate::ui::pi_panel::PiStatus::Connecting;
                Task::none()
            }
            PiMsg::Paste => {
                // Image clipboard first; text falls back into the editor.
                iced::clipboard::read_image()
                    .map(|result| result.ok())
                    .then(|image| match image {
                        Some(image) => {
                            Task::done(Message::PiImagePasted(Ok(image)))
                        }
                        None => iced::clipboard::read_text().map(|result| {
                            match result {
                                Ok(text) => Message::PiImagePasted(Err(Some(text))),
                                Err(_) => Message::PiImagePasted(Err(None)),
                            }
                        }),
                    })
            }
            PiMsg::ClearImage => {
                let panel = &mut self.tabs[tab].pi_panel;
                panel.pending_image = None;
                panel.pending_image_size = None;
                Task::none()
            }
            PiMsg::ModelPick(model) => {
                let panel = &mut self.tabs[tab].pi_panel;
                if panel.active.is_some() {
                    panel.send_command(crate::pi::Command::SetModel {
                        provider: model.provider.clone(),
                        model_id: model.id.clone(),
                    });
                }
                Task::none()
            }
            PiMsg::ThinkingPick(level) => {
                let panel = &mut self.tabs[tab].pi_panel;
                if panel.active.is_some() {
                    panel.current_thinking = Some(level.clone());
                    panel.send_command(crate::pi::Command::SetThinking { level });
                }
                Task::none()
            }
            PiMsg::BackendPick(mode) => {
                let panel = &mut self.tabs[tab].pi_panel;
                if panel.mode == mode {
                    return Task::none();
                }
                panel.mode = mode.clone();
                crate::pi::save_backend_mode(&mode);
                // The backends keep different session lists and transports, so
                // switching means starting over: drop the worker (and its
                // in-flight turn) and forget everything derived from it.
                panel.stop_worker();
                panel.reset_for_backend_switch();
                panel.ensure_worker();
                panel.status = crate::ui::pi_panel::PiStatus::Connecting;
                Task::none()
            }
            PiMsg::MenuUp => {
                self.tabs[tab].pi_panel.menu_move(-1);
                Task::none()
            }
            PiMsg::MenuDown => {
                self.tabs[tab].pi_panel.menu_move(1);
                Task::none()
            }
            PiMsg::MenuAccept => {
                self.tabs[tab].pi_panel.menu_accept();
                Task::none()
            }
            PiMsg::MenuClose => {
                self.tabs[tab].pi_panel.menu_close();
                Task::none()
            }
            PiMsg::NewSessionOpen => {
                self.tabs[tab].pi_panel.open_browse();
                Task::none()
            }
            PiMsg::NewSessionCancel => {
                self.tabs[tab].pi_panel.browse = None;
                Task::none()
            }
            PiMsg::DirOpen(path) => {
                self.tabs[tab].pi_panel.browse_to(Some(path));
                Task::none()
            }
            PiMsg::DirUp => {
                self.tabs[tab].pi_panel.browse_to(None);
                Task::none()
            }
            PiMsg::CreateSession => {
                self.tabs[tab].pi_panel.create_session();
                Task::none()
            }
            PiMsg::UiAnswer { value, confirmed } => {
                let panel = &mut self.tabs[tab].pi_panel;
                if let Some(request) = panel.pending_ui.take() {
                    panel.send_command(crate::pi::Command::UiRespond {
                        id: request.id,
                        value,
                        confirmed,
                        cancelled: false,
                    });
                }
                Task::none()
            }
            PiMsg::UiEdit(action) => {
                self.tabs[tab].pi_panel.ui_answer.perform(action);
                Task::none()
            }
            PiMsg::UiSubmit => {
                let panel = &mut self.tabs[tab].pi_panel;
                if let Some(request) = panel.pending_ui.take() {
                    let value = panel.ui_answer.text().trim().to_string();
                    panel.send_command(crate::pi::Command::UiRespond {
                        id: request.id,
                        value: Some(value),
                        confirmed: None,
                        cancelled: false,
                    });
                }
                Task::none()
            }
            PiMsg::Compact => {
                let panel = &mut self.tabs[tab].pi_panel;
                if panel.active.is_some() {
                    panel.send_command(crate::pi::Command::Compact);
                }
                Task::none()
            }
            PiMsg::UiCancel => {
                let panel = &mut self.tabs[tab].pi_panel;
                if let Some(request) = panel.pending_ui.take() {
                    panel.send_command(crate::pi::Command::UiRespond {
                        id: request.id,
                        value: None,
                        confirmed: None,
                        cancelled: true,
                    });
                }
                Task::none()
            }
        }
    }

    /// Composer paste result: stage a clipboard image as the attachment or
    /// fall back to inserting clipboard text into the editor.
    pub(in crate::app) fn on_pi_image_pasted(
        &mut self,
        payload: Result<iced::clipboard::Image, Option<std::sync::Arc<String>>>,
    ) -> Task<Message> {
        let tab = self.active_tab;
        match payload {
            Ok(image) => {
                let fail = |panel: &mut crate::ui::pi_panel::PiPanelState, message: String| {
                    panel.push_notice(format!("粘贴图片失败：{message}"));
                };
                let mut img = match image::RgbaImage::from_raw(
                    image.size.width,
                    image.size.height,
                    image.rgba.to_vec(),
                ) {
                    Some(img) => img,
                    None => {
                        fail(&mut self.tabs[tab].pi_panel, "图像数据异常".into());
                        return Task::none();
                    }
                };
                // Cap the longest side so the base64 stays well under the
                // 10 MB prompt-image limit.
                let longest = img.width().max(img.height());
                if longest > 1600 {
                    let scale = 1600.0 / longest as f64;
                    let width = ((img.width() as f64 * scale).round() as u32).max(1);
                    let height = ((img.height() as f64 * scale).round() as u32).max(1);
                    img = image::imageops::resize(
                        &img,
                        width,
                        height,
                        image::imageops::FilterType::Triangle,
                    );
                }
                let mut png: Vec<u8> = Vec::new();
                if img
                    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .is_err()
                {
                    fail(&mut self.tabs[tab].pi_panel, "PNG 编码失败".into());
                    return Task::none();
                }
                if png.len() > 8 * 1024 * 1024 {
                    fail(&mut self.tabs[tab].pi_panel, "图片过大（>8 MB）".into());
                    return Task::none();
                }
                let size = (img.width(), img.height());
                let base64 = base64::engine::general_purpose::STANDARD.encode(&png);
                let panel = &mut self.tabs[tab].pi_panel;
                panel.pending_image = Some(crate::pi::ImageAttachment {
                    mime: "image/png".into(),
                    base64,
                });
                panel.pending_image_size = Some(size);
                Task::none()
            }
            Err(Some(text)) => {
                // No image on the clipboard — insert the text as usual.
                let panel = &mut self.tabs[tab].pi_panel;
                panel
                    .input
                    .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                        text,
                    )));
                Task::none()
            }
            Err(None) => Task::none(),
        }
    }
}

/// Scroll the transcript scrollable to its very bottom.
fn scroll_transcript_to_bottom() -> Task<Message> {
    iced::widget::operation::snap_to_end(iced::widget::Id::new(
        crate::ui::pi_panel::TRANSCRIPT_ID,
    ))
}

/// Selection summary for the Pi composer ("圆弧（1）" / "直线（2）" /
/// "全部（2）"), mirroring the Properties panel's grouping method:
/// one object or several of one kind → the kind name; mixed kinds → 全部.
/// (翻译/命名与 `build_selection_groups` 同源，避免两套叫法。)
fn selection_label(scene: &crate::scene::Scene) -> String {
    use std::collections::BTreeMap;
    let selected = scene.selected_entities();
    if selected.is_empty() {
        return String::new();
    }
    let n = selected.len();
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    for (_, entity) in &selected {
        *by_type
            .entry(crate::app::helpers::entity_type_key(entity))
            .or_default() += 1;
    }
    match by_type.iter().next() {
        // One object, or several of one kind → the kind name; mixed → 全部.
        Some((kind, count)) if by_type.len() == 1 => format!(
            "{}（{}）",
            crate::t!(crate::app::helpers::title_case_word(kind)),
            count
        ),
        _ => format!("{}（{}）", crate::t!("All"), n),
    }
}

/// Silence unused-import warning when entry kinds are only used in tests.
#[allow(dead_code)]
fn _assert_kinds(_: PiEntryKind) {}
