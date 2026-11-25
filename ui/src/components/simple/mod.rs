mod button;
mod checkbox;
mod chevron;
mod choice_list;
mod disclosure;
mod row_action;
#[cfg(test)]
mod testing;
mod text_input;

pub use button::Button;
pub use checkbox::Checkbox;
pub use chevron::Chevron;
pub use choice_list::{Choice, ChoiceList};
#[cfg(test)]
pub(crate) use disclosure::ToggleDriver;
pub use disclosure::{use_open_handler, Disclosure};
pub use row_action::RowAction;
#[cfg(test)]
pub(crate) use testing::{change, click, click_and_find, listeners, opening_tag, submit_form};
pub use text_input::{submitted_text, Autofill, FieldKind, FieldLook, TextInput, FIELD_CLASS};

/// What a small control looks like when it has keyboard focus.
pub const FOCUS_RING: &str =
    "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-beet-leaf";
