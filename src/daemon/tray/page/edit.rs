//! Custom text editor for popup fields (renderer agent A). Pure, unit-tested: `Editor { text,
//! caret (char index), anchor, multiline, secret, max_chars, scroll }` with insert / backspace /
//! delete / caret moves / word moves / selection / clipboard ops (CF_UNICODETEXT via
//! `paste::read_clipboard_text` and `paste::copy_to_clipboard`; secret fields refuse copy/cut) /
//! click-to-caret from `prefix_widths`. Native EDIT children cannot live in the layered popup.
//!
//! STUB: nothing here yet.
