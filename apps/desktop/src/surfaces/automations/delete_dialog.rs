//! Delete automation confirmation dialog (packages/app/src/automations/route.tsx:confirmDelete).

use opencode_i18n::t;
use opencode_ui::palette::{Elevate as _, Pal, flat, kit};
use opencode_ui::widgets::{BODY, DialogSize, Fit, button, dialog_window, sized_modal};
use rune::{Color, Cx, El, TextStyle, Tx, col, row, spacer, text};
use rune_kit::ButtonVariant;

use crate::App;

use super::{AUTOMATIONS, DeleteModal};

const DIALOG_TITLE: TextStyle = TextStyle::new(15.0, 530.0).lh(20.0).tracking(-0.13);

pub fn view(s: &App, p: &Pal, cx: &Cx<App>) -> Vec<El> {
    let DeleteModal::Confirm { key, server, id, name } = &s.kernel.slots.get(&AUTOMATIONS).delete_modal else {
        return vec![];
    };
    let (key, server, id, name) = (key.clone(), server.clone(), id.clone(), name.clone());
    let k = kit(p);
    let cancel = button(t!("common.cancel"), ButtonVariant::Ghost)
        .on_press_tx(|s: &mut App, tx: &mut Tx| { super::cancel_delete(s, tx); })
        .build(k)
        .key("auto-delete-cancel")
        .autofocus();
    let delete = button(t!("automations.delete.button"), ButtonVariant::Danger)
        .on_press_tx(move |s: &mut App, tx: &mut Tx| super::confirm_delete(s, &key, &server, &id, tx))
        .build(k)
        .key("auto-delete-confirm");
    let delete = if p.dark && p.theme == 0 {
        delete.fill(flat(Color::WHITE.alpha(0.06), p.layer1))
    } else {
        delete
    };
    let card = col()
        .key("auto-delete-card")
        .role("dialog")
        .label_static(t!("automations.delete.title"))
        .radius(6.0)
        .fill(p.layer1)
        .elevate(p.overlay)
        .clip_soft(0.0)
        .modal_tx(true, |s: &mut App, tx: &mut Tx| { super::cancel_delete(s, tx); })
        .child(
            col()
                .key("auto-delete-header")
                .w_full()
                .padding(16.0)
                .gap(4.0)
                .child(text(t!("automations.delete.title")).font(DIALOG_TITLE).color(p.text))
                .child(text(t!("automations.delete.confirm", name = name)).font(BODY).color(p.muted).wrap().w_full()),
        )
        .child(row().key("auto-delete-footer").w_full().padding(16.0).gap(8.0).child(spacer()).child(cancel).child(delete));
    sized_modal("auto-delete-dialog", p, DialogSize::Normal, Fit::Content, dialog_window(cx, *s.zoom), card)
}
